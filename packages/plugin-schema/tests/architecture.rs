//! Repository architecture tests for BPDL plugin API boundaries.

use std::fs;
use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("plugin-schema should live under packages/plugin-schema")
        .to_path_buf()
}

fn plugin_api_src_dirs() -> Vec<PathBuf> {
    let plugins_dir = repo_root().join("plugins");
    fs::read_dir(plugins_dir)
        .expect("plugins directory should be readable")
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.is_dir()
                && path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.ends_with("-plugin-api"))
        })
        .map(|path| path.join("src"))
        .filter(|path| path.exists())
        .collect()
}

fn rust_files(root: &Path) -> Vec<PathBuf> {
    let mut pending = vec![root.to_path_buf()];
    let mut files = Vec::new();
    while let Some(path) = pending.pop() {
        if path.is_dir() {
            for entry in fs::read_dir(&path).expect("directory should be readable") {
                pending.push(entry.expect("entry should be readable").path());
            }
        } else if path.extension().and_then(|ext| ext.to_str()) == Some("rs") {
            files.push(path);
        }
    }
    files
}

#[test]
fn plugin_api_crates_do_not_publish_handwritten_typed_client_modules() {
    let offenders: Vec<_> = plugin_api_src_dirs()
        .into_iter()
        .flat_map(|src| rust_files(&src))
        .filter(|path| path.file_name().and_then(|name| name.to_str()) == Some("typed_client.rs"))
        .collect();
    assert!(
        offenders.is_empty(),
        "plugin API crates must use BPDL-generated clients, not public typed_client.rs files: {offenders:?}"
    );
}

#[test]
fn plugin_api_crates_do_not_publish_broad_transport_client_modules() {
    let offenders: Vec<_> = plugin_api_src_dirs()
        .into_iter()
        .flat_map(|src| rust_files(&src))
        .filter(|path| {
            let Ok(contents) = fs::read_to_string(path) else {
                return false;
            };
            contents.contains("pub mod typed_client")
                || contents.contains("pub struct TypedClient")
                || contents.contains("pub struct TransportClient")
                || contents.contains("pub struct ServiceClient")
        })
        .collect();
    assert!(
        offenders.is_empty(),
        "plugin API crates must not publish broad handwritten transport clients where BPDL generated clients exist: {offenders:?}"
    );
}

#[test]
fn plugin_api_crates_do_not_reintroduce_public_request_response_envelopes() {
    let offenders: Vec<_> = plugin_api_src_dirs()
        .into_iter()
        .flat_map(|src| rust_files(&src))
        .filter(|path| {
            let Ok(contents) = fs::read_to_string(path) else {
                return false;
            };
            contents.contains("pub struct ServiceRequest")
                || contents.contains("pub struct ServiceResponse")
                || contents.contains("pub enum ServiceRequest")
                || contents.contains("pub enum ServiceResponse")
                || contents.contains("pub struct RequestEnvelope")
                || contents.contains("pub struct ResponseEnvelope")
        })
        .collect();
    assert!(
        offenders.is_empty(),
        "plugin API crates must not expose broad handwritten request/response transport envelopes: {offenders:?}"
    );
}
