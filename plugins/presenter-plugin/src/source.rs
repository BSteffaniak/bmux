//! Source loading is explicit and local; remote viewers never execute deck providers.
use anyhow::{Context, Result, ensure};
use bmux_presenter::Deck;
use std::path::Path;

const MAX_SOURCE_BYTES: usize = 8 * 1024 * 1024;

pub(crate) fn load(path: &Path) -> Result<Deck> {
    Ok(loaded(path)?.deck)
}

pub(crate) fn loaded(path: &Path) -> Result<bmux_presenter_runtime::loader::LoadedDeck> {
    let path = std::fs::canonicalize(path)?;
    let base = path.parent().context("deck requires parent directory")?;
    let loader = bmux_presenter_runtime::loader::SourceLoader {
        root: base.to_owned(),
        limits: bmux_presenter_runtime::Limits::default(),
        mermaid: Some(bmux_mermaid_render::WasmMermaidRenderer::new()?),
    };
    ensure!(
        path.extension().is_none_or(|ext| ext != "rs"),
        "CLI .rs compilation is outside scope; linked Rust applications construct Deck and Host directly (see examples/linked_host.rs)"
    );
    if path.extension().is_some_and(|ext| ext == "json") {
        let bytes = std::fs::read(&path)?;
        ensure!(
            bytes.len() <= MAX_SOURCE_BYTES,
            "deck exceeds source budget"
        );
        let deck: Deck = serde_json::from_slice(&bytes)?;
        return loader.prepare(deck, base).map_err(anyhow::Error::from);
    }
    let loaded = loader.load_markdown(&path)?;
    ensure!(!loaded.deck.slides.is_empty(), "deck contains no slides");
    Ok(loaded)
}

pub(crate) fn credential(path: &Path) -> Result<String> {
    let metadata = std::fs::metadata(path).context("reading credential metadata")?;
    ensure!(
        metadata.is_file() && metadata.len() <= 4096,
        "credential must be a bounded regular file"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        ensure!(
            metadata.permissions().mode() & 0o077 == 0,
            "credential file must not be group/world accessible"
        );
    }
    let secret = std::fs::read_to_string(path).context("reading private credential file")?;
    let secret = secret.trim().to_owned();
    ensure!(
        secret.len() >= 32 && secret.len() <= 256,
        "credential must contain between 32 and 256 bytes"
    );
    Ok(secret)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_mermaid_loads_without_worker_discovery() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("deck.md");
        std::fs::write(
            &path,
            "# Diagram\n\n```mermaid\nflowchart LR\nA[Hello] --> B[World]\n```\n",
        )
        .unwrap();
        let loaded = loaded(&path).unwrap();
        assert_eq!(loaded.assets.len(), 1);
        assert_eq!(loaded.assets[0].media_type, "image/svg+xml");
        assert!(String::from_utf8_lossy(&loaded.assets[0].bytes).contains("Hello"));
    }

    #[test]
    fn rejects_missing_sources_without_terminal_io() {
        assert!(load(Path::new("/nonexistent/bmux-presentation-deck.md")).is_err());
    }
}
