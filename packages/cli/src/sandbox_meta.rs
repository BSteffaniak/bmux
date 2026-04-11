use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

pub const MANIFEST_FILE: &str = "sandbox.json";
pub const LOCK_FILE: &str = "sandbox.lock";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SandboxManifestPaths {
    pub root: String,
    pub logs: String,
    pub runtime: String,
    pub state: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SandboxManifest {
    pub id: String,
    #[serde(default)]
    pub source: String,
    pub created_at_unix_ms: u128,
    pub updated_at_unix_ms: u128,
    pub pid: u32,
    pub bmux_bin: String,
    pub command: Vec<String>,
    pub env_mode: String,
    pub status: String,
    pub exit_code: Option<i32>,
    pub kept: bool,
    pub paths: SandboxManifestPaths,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SandboxLock {
    pub pid: u32,
    pub updated_at_unix_ms: u128,
}

pub fn default_source() -> String {
    "sandbox-cli".to_string()
}

pub fn unix_millis_now() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_millis())
}

pub fn sandbox_id_from_root(root: &Path) -> String {
    root.file_name()
        .map_or_else(String::new, |value| value.to_string_lossy().to_string())
}

pub fn read_manifest(root: &Path) -> Result<SandboxManifest> {
    let manifest_path = root.join(MANIFEST_FILE);
    let bytes = std::fs::read(&manifest_path)
        .with_context(|| format!("failed reading {}", manifest_path.display()))?;
    let mut manifest = serde_json::from_slice::<SandboxManifest>(&bytes)
        .with_context(|| format!("failed parsing {}", manifest_path.display()))?;
    if manifest.source.trim().is_empty() {
        manifest.source = default_source();
    }
    Ok(manifest)
}

pub fn write_manifest(root: &Path, manifest: &SandboxManifest) -> Result<()> {
    let manifest_path = root.join(MANIFEST_FILE);
    let encoded = serde_json::to_vec_pretty(manifest)?;
    write_atomic_file(&manifest_path, &encoded)
        .with_context(|| format!("failed writing {}", manifest_path.display()))
}

pub fn read_lock(root: &Path) -> Option<SandboxLock> {
    let lock_path = root.join(LOCK_FILE);
    std::fs::read(lock_path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<SandboxLock>(&bytes).ok())
}

pub fn write_lock(root: &Path, pid: u32) -> Result<()> {
    let lock_path = root.join(LOCK_FILE);
    let lock = SandboxLock {
        pid,
        updated_at_unix_ms: unix_millis_now(),
    };
    let bytes = serde_json::to_vec(&lock)?;
    write_atomic_file(&lock_path, &bytes)
}

pub fn clear_lock(root: &Path) {
    let _ = std::fs::remove_file(root.join(LOCK_FILE));
}

fn write_atomic_file(path: &Path, bytes: &[u8]) -> Result<()> {
    let temp_path = path.with_extension("tmp");
    std::fs::write(&temp_path, bytes)
        .with_context(|| format!("failed writing {}", temp_path.display()))?;
    std::fs::rename(&temp_path, path).with_context(|| {
        format!(
            "failed renaming {} to {}",
            temp_path.display(),
            path.display()
        )
    })
}
