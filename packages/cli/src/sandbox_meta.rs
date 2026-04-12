use anyhow::{Context, Result, anyhow};
use bmux_config::ConfigPaths;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;
use std::time::{SystemTime, UNIX_EPOCH};

pub const MANIFEST_FILE: &str = "sandbox.json";
pub const LOCK_FILE: &str = "sandbox.lock";
const SANDBOX_INDEX_DIR: &str = "sandbox";
const SANDBOX_INDEX_FILE: &str = "index.json";
const SANDBOX_INDEX_LOCK_FILE: &str = "index.lock";
const SANDBOX_INDEX_SCHEMA_VERSION: u32 = 1;
const INDEX_LOCK_RETRY_MS: u64 = 5;
const INDEX_LOCK_MAX_ATTEMPTS: usize = 400;
const INDEX_LOCK_STALE_SECS: u64 = 30;

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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SandboxIndexEntry {
    pub id: String,
    pub root: String,
    pub source: String,
    pub status: String,
    pub created_at_unix_ms: u128,
    pub updated_at_unix_ms: u128,
    pub last_seen_unix_ms: u128,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SandboxIndex {
    #[serde(default = "sandbox_index_schema_version")]
    schema_version: u32,
    #[serde(default)]
    entries: Vec<SandboxIndexEntry>,
}

const fn sandbox_index_schema_version() -> u32 {
    SANDBOX_INDEX_SCHEMA_VERSION
}

fn sandbox_index_path() -> PathBuf {
    ConfigPaths::default()
        .state_dir()
        .join(SANDBOX_INDEX_DIR)
        .join(SANDBOX_INDEX_FILE)
}

fn sandbox_index_lock_path() -> PathBuf {
    ConfigPaths::default()
        .state_dir()
        .join(SANDBOX_INDEX_DIR)
        .join(SANDBOX_INDEX_LOCK_FILE)
}

pub fn sandbox_index_exists() -> bool {
    sandbox_index_path().exists()
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

pub fn read_index_entries() -> Result<Vec<SandboxIndexEntry>> {
    let index_path = sandbox_index_path();
    if !index_path.exists() {
        return Ok(Vec::new());
    }
    let bytes = std::fs::read(&index_path)
        .with_context(|| format!("failed reading {}", index_path.display()))?;
    let mut index = serde_json::from_slice::<SandboxIndex>(&bytes)
        .with_context(|| format!("failed parsing {}", index_path.display()))?;
    if index.schema_version == 0 {
        index.schema_version = SANDBOX_INDEX_SCHEMA_VERSION;
    }
    Ok(index.entries)
}

pub fn upsert_index_entry(manifest: &SandboxManifest) -> Result<()> {
    with_index_lock(|| {
        let mut entries = read_index_entries().unwrap_or_default();
        let now = unix_millis_now();
        let root = manifest.paths.root.clone();
        let entry = SandboxIndexEntry {
            id: manifest.id.clone(),
            root: root.clone(),
            source: manifest.source.clone(),
            status: manifest.status.clone(),
            created_at_unix_ms: manifest.created_at_unix_ms,
            updated_at_unix_ms: manifest.updated_at_unix_ms,
            last_seen_unix_ms: now,
        };

        if let Some(existing) = entries.iter_mut().find(|existing| existing.root == root) {
            *existing = entry;
        } else {
            entries.push(entry);
        }

        write_index_entries(entries)
    })
}

pub fn remove_index_entry(root: &Path) -> Result<()> {
    with_index_lock(|| {
        let root_value = root.to_string_lossy().to_string();
        let entries = read_index_entries().unwrap_or_default();
        let filtered = entries
            .into_iter()
            .filter(|entry| entry.root != root_value)
            .collect::<Vec<_>>();
        write_index_entries(filtered)
    })
}

pub fn prune_missing_index_entries() -> Result<usize> {
    with_index_lock(|| {
        let entries = read_index_entries().unwrap_or_default();
        let before = entries.len();
        let filtered = entries
            .into_iter()
            .filter(|entry| Path::new(&entry.root).is_dir())
            .collect::<Vec<_>>();
        let removed = before.saturating_sub(filtered.len());
        if removed > 0 {
            write_index_entries(filtered)?;
        }
        Ok(removed)
    })
}

pub fn replace_index_entries(entries: Vec<SandboxIndexEntry>) -> Result<()> {
    with_index_lock(|| write_index_entries(entries))
}

fn with_index_lock<T>(operation: impl FnOnce() -> Result<T>) -> Result<T> {
    let lock_path = sandbox_index_lock_path();
    if let Some(parent) = lock_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed creating {}", parent.display()))?;
    }

    for _ in 0..INDEX_LOCK_MAX_ATTEMPTS {
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&lock_path)
        {
            Ok(_) => {
                let guard = IndexLockGuard { path: lock_path };
                let result = operation();
                drop(guard);
                return result;
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                if index_lock_is_stale(&lock_path) {
                    let _ = std::fs::remove_file(&lock_path);
                }
                thread::sleep(Duration::from_millis(INDEX_LOCK_RETRY_MS));
            }
            Err(error) => {
                return Err(anyhow!(
                    "failed acquiring sandbox index lock {}: {error}",
                    lock_path.display()
                ));
            }
        }
    }

    Err(anyhow!(
        "timed out acquiring sandbox index lock {}",
        lock_path.display()
    ))
}

fn index_lock_is_stale(lock_path: &Path) -> bool {
    lock_path
        .metadata()
        .ok()
        .and_then(|metadata| metadata.modified().ok())
        .and_then(|modified| SystemTime::now().duration_since(modified).ok())
        .is_some_and(|elapsed| elapsed >= Duration::from_secs(INDEX_LOCK_STALE_SECS))
}

struct IndexLockGuard {
    path: PathBuf,
}

impl Drop for IndexLockGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn write_index_entries(entries: Vec<SandboxIndexEntry>) -> Result<()> {
    let index_path = sandbox_index_path();
    if let Some(parent) = index_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed creating {}", parent.display()))?;
    }
    let index = SandboxIndex {
        schema_version: SANDBOX_INDEX_SCHEMA_VERSION,
        entries,
    };
    let encoded = serde_json::to_vec_pretty(&index)?;
    write_atomic_file(&index_path, &encoded)
        .with_context(|| format!("failed writing {}", index_path.display()))
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
