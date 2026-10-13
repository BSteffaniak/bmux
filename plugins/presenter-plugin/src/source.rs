//! Source loading is explicit and local; remote viewers never execute deck providers.
use anyhow::{Context, Result, ensure};
use bmux_presenter::Deck;
use std::path::Path;

use std::sync::{Mutex, OnceLock};
static WORKER: OnceLock<Mutex<Option<bmux_mermaid_render::IsolatedMermaidRenderer>>> =
    OnceLock::new();

pub(crate) fn activate(context: &bmux_plugin_sdk::NativeLifecycleContext) -> Result<()> {
    let Some(helper) = context.auxiliary_executables.get("mermaid-worker") else {
        return Ok(());
    };
    ensure!(
        helper.protocol_version == 1,
        "unsupported packaged Mermaid worker protocol"
    );
    let worker = bmux_mermaid_render::IsolatedMermaidRenderer::pin(Path::new(&helper.path))?;
    *WORKER
        .get_or_init(Mutex::default)
        .lock()
        .map_err(|_| anyhow::anyhow!("Mermaid worker registry poisoned"))? = Some(worker);
    Ok(())
}

const MAX_SOURCE_BYTES: usize = 8 * 1024 * 1024;

pub(crate) fn load(path: &Path) -> Result<Deck> {
    Ok(loaded(path)?.deck)
}

pub(crate) fn loaded(path: &Path) -> Result<bmux_presenter_runtime::loader::LoadedDeck> {
    let path = std::fs::canonicalize(path)?;
    let base = path.parent().context("deck requires parent directory")?;
    let mermaid = if let Some(worker) = WORKER
        .get()
        .and_then(|registry| registry.lock().ok()?.clone())
    {
        Some(worker)
    } else {
        std::env::var_os("BMUX_MERMAID_WORKER_PATH")
            .map(|worker| bmux_mermaid_render::IsolatedMermaidRenderer::pin(Path::new(&worker)))
            .transpose()?
    };
    let mermaid = if mermaid.is_some() {
        mermaid
    } else {
        let executable = std::env::current_exe()?;
        if let Some(directory) = executable.parent() {
            let _ = bmux_mermaid_render::IsolatedMermaidRenderer::initialize_packaged(directory);
        }
        bmux_mermaid_render::IsolatedMermaidRenderer::packaged().ok()
    };
    let loader = bmux_presenter_runtime::loader::SourceLoader {
        root: base.to_owned(),
        limits: bmux_presenter_runtime::Limits::default(),
        mermaid,
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
    fn rejects_missing_sources_without_terminal_io() {
        assert!(load(Path::new("/nonexistent/bmux-presentation-deck.md")).is_err());
    }
}
