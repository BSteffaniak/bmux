//! Bounded local Markdown loader. Local assets stay beneath a caller-selected root;
//! no network fetches, shell execution, or viewer-supplied Rust activation.
use crate::{Asset, HostError, Limits};
use bmux_mermaid_render::{
    IsolatedMermaidRenderer, MermaidCancellationToken, MermaidRenderRequest, MermaidRenderedOutput,
};
use bmux_presenter::{Block, BlockKind, Deck, TextFormat, parse_markdown};
use std::{
    fs::File,
    io::Read,
    path::{Path, PathBuf},
};

pub struct LoadedDeck {
    pub deck: Deck,
    pub assets: Vec<Asset>,
}
pub struct SourceLoader {
    pub root: PathBuf,
    pub limits: Limits,
    pub mermaid: Option<IsolatedMermaidRenderer>,
}
impl SourceLoader {
    pub fn load_markdown(&self, path: &Path) -> Result<LoadedDeck, HostError> {
        let root = self.root.canonicalize().map_err(|_| HostError::NotFound)?;
        let path = bounded_path(&root, path)?;
        let bytes = read_bounded(&path, self.limits.deck_bytes)?;
        let source = String::from_utf8(bytes).map_err(|_| HostError::Invalid)?;
        // Diagnostic source name is owner-local; public projection strips it.
        let deck =
            parse_markdown(&path.to_string_lossy(), &source).map_err(|_| HostError::Invalid)?;
        self.prepare(deck, path.parent().ok_or(HostError::Invalid)?)
    }
    /// Linked Rust decks use the same asset preparation and audience boundary.
    pub fn prepare(&self, mut deck: Deck, base: &Path) -> Result<LoadedDeck, HostError> {
        crate::projection::validate_theme(&deck)?;
        let root = self.root.canonicalize().map_err(|_| HostError::NotFound)?;
        let base = bounded_path(&root, base)?;
        let mut assets = Vec::new();
        let mut total = 0_usize;
        fn visit(
            loader: &SourceLoader,
            root: &Path,
            base: &Path,
            blocks: &mut [Block],
            assets: &mut Vec<Asset>,
            total: &mut usize,
        ) -> Result<(), HostError> {
            for block in blocks {
                match &mut block.kind {
                    BlockKind::Image { destination, .. } => {
                        let path = bounded_path(root, &base.join(&*destination))?;
                        let bytes = read_bounded(&path, loader.limits.asset_bytes)?;
                        let png = normalize_image(&bytes, loader.limits.asset_bytes)?;
                        let id = uuid::Uuid::new_v4().to_string();
                        *destination = id.clone();
                        push_asset(
                            loader,
                            assets,
                            total,
                            Asset {
                                id,
                                media_type: "image/png".into(),
                                bytes: png,
                            },
                        )?;
                    }
                    BlockKind::Text {
                        content,
                        format: TextFormat::Markdown,
                    } => {
                        *content =
                            crate::projection::remap_markdown_images(content, |destination| {
                                let path = bounded_path(root, &base.join(destination))?;
                                let bytes = read_bounded(&path, loader.limits.asset_bytes)?;
                                let png = normalize_image(&bytes, loader.limits.asset_bytes)?;
                                let id = uuid::Uuid::new_v4().to_string();
                                push_asset(
                                    loader,
                                    assets,
                                    total,
                                    Asset {
                                        id: id.clone(),
                                        media_type: "image/png".into(),
                                        bytes: png,
                                    },
                                )?;
                                Ok(id)
                            })?;
                    }
                    BlockKind::Mermaid { source, rows } => {
                        let worker = loader.mermaid.as_ref().ok_or(HostError::NotFound)?;
                        let mut request = MermaidRenderRequest::svg(source.clone(), 1280, 720);
                        request.limits.max_source_bytes = loader.limits.action_bytes.min(64 * 1024);
                        request.limits.max_output_bytes =
                            loader.limits.asset_bytes.min(4 * 1024 * 1024);
                        // Snapshot explicit worker bytes before invocation even if the
                        // application supplied a mutable from_path renderer.
                        let pinned = IsolatedMermaidRenderer::pin(worker.executable())
                            .map_err(|_| HostError::Invalid)?;
                        let rendered = pinned
                            .render(&request, &MermaidCancellationToken::default())
                            .map_err(|_| HostError::Invalid)?;
                        let MermaidRenderedOutput::Svg(bytes) = rendered.output;
                        let id = uuid::Uuid::new_v4().to_string();
                        push_asset(
                            loader,
                            assets,
                            total,
                            Asset {
                                id: id.clone(),
                                media_type: "image/svg+xml".into(),
                                bytes,
                            },
                        )?;
                        block.kind = BlockKind::Image {
                            destination: id,
                            alt: "diagram".into(),
                            rows: *rows,
                        };
                    }
                    BlockKind::Columns { columns } => {
                        for column in columns {
                            visit(loader, root, base, &mut column.blocks, assets, total)?;
                        }
                    }
                    _ => {}
                }
            }
            Ok(())
        }
        for slide in &mut deck.slides {
            visit(
                self,
                &root,
                &base,
                &mut slide.blocks,
                &mut assets,
                &mut total,
            )?;
        }
        deck.validate().map_err(|_| HostError::Invalid)?;
        Ok(LoadedDeck { deck, assets })
    }
}
fn push_asset(
    loader: &SourceLoader,
    assets: &mut Vec<Asset>,
    total: &mut usize,
    asset: Asset,
) -> Result<(), HostError> {
    *total = total
        .checked_add(asset.bytes.len())
        .ok_or(HostError::Limit)?;
    if assets.len() >= loader.limits.assets
        || *total > loader.limits.total_asset_bytes
        || asset.bytes.len() > loader.limits.asset_bytes
    {
        return Err(HostError::Limit);
    }
    assets.push(asset);
    Ok(())
}
fn bounded_path(root: &Path, path: &Path) -> Result<PathBuf, HostError> {
    let path = if path.is_absolute() {
        path.to_owned()
    } else {
        root.join(path)
    };
    let path = path.canonicalize().map_err(|_| HostError::NotFound)?;
    if !path.starts_with(root) {
        return Err(HostError::Unauthorized);
    }
    Ok(path)
}
fn read_bounded(path: &Path, limit: usize) -> Result<Vec<u8>, HostError> {
    let file = File::open(path).map_err(|_| HostError::NotFound)?;
    if !file.metadata().map_err(|_| HostError::Invalid)?.is_file() {
        return Err(HostError::Invalid);
    }
    let mut bytes = Vec::new();
    file.take(
        u64::try_from(limit)
            .map_err(|_| HostError::Limit)?
            .saturating_add(1),
    )
    .read_to_end(&mut bytes)
    .map_err(|_| HostError::Invalid)?;
    if bytes.len() > limit {
        return Err(HostError::Limit);
    }
    Ok(bytes)
}
fn normalize_image(bytes: &[u8], limit: usize) -> Result<Vec<u8>, HostError> {
    let mut reader = image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|_| HostError::Invalid)?;
    let mut bounds = image::Limits::default();
    bounds.max_image_width = Some(4096);
    bounds.max_image_height = Some(4096);
    bounds.max_alloc = Some(64 * 1024 * 1024);
    reader.limits(bounds);
    let decoded = reader.decode().map_err(|_| HostError::Invalid)?;
    let mut output = std::io::Cursor::new(Vec::new());
    decoded
        .write_to(&mut output, image::ImageFormat::Png)
        .map_err(|_| HostError::Invalid)?;
    let output = output.into_inner();
    if output.len() > limit {
        return Err(HostError::Limit);
    }
    Ok(output)
}
