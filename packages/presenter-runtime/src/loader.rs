//! Bounded local Markdown loader. Local assets stay beneath a caller-selected root;
//! no network fetches, shell execution, or viewer-supplied Rust activation.
use crate::{Asset, HostError, Limits};
use bmux_mermaid_render::{
    MermaidCancellationToken, MermaidRenderRequest, MermaidRenderedOutput, WasmMermaidRenderer,
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
    pub mermaid: Option<WasmMermaidRenderer>,
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
                        let renderer = loader.mermaid.as_ref().ok_or(HostError::NotFound)?;
                        let mut request = MermaidRenderRequest::svg(source.clone(), 1280, 720);
                        request.limits.max_source_bytes = loader.limits.action_bytes.min(64 * 1024);
                        request.limits.max_output_bytes =
                            loader.limits.asset_bytes.min(4 * 1024 * 1024);
                        let rendered = renderer
                            .render(&request, &MermaidCancellationToken::default())
                            .map_err(|error| HostError::Diagram {
                                message: error.to_string(),
                            })?;
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
    let output = bmux_raster::transcode_png(bytes, 4096, 4096, 64 * 1024 * 1024)
        .map_err(|_| HostError::Invalid)?;
    if output.len() > limit {
        return Err(HostError::Limit);
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::normalize_image;

    #[test]
    fn normalization_keeps_sixteen_bit_grayscale_png() {
        // One grayscale sample, 0x1234: RGBA8 normalization would lose precision.
        let source = [
            0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48,
            0x44, 0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x10, 0x00, 0x00, 0x00,
            0x00, 0x6a, 0xee, 0x47, 0x16, 0x00, 0x00, 0x00, 0x0b, 0x49, 0x44, 0x41, 0x54, 0x78,
            0x9c, 0x63, 0x10, 0x32, 0x01, 0x00, 0x00, 0x5b, 0x00, 0x47, 0x96, 0xfb, 0x1b, 0x65,
            0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
        ];
        let png = normalize_image(&source, 4096).unwrap();
        assert_eq!(&png[24..26], &[16, 0], "IHDR bit depth and color type");
        bmux_raster::validate_png(&png, 1, 1, 4096).unwrap();
    }
}
