use anyhow::{Context, Result};
use bmux_config::{ConfigPaths, ScrollbackMode};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::Path;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use uuid::Uuid;

const SCROLLBACK_MODE_STATE_VERSION: u32 = 1;
const ABSENT_SESSION_RETENTION_SECS: u64 = 30 * 24 * 60 * 60;
const SAVE_DEBOUNCE: Duration = Duration::from_millis(250);

#[derive(Default)]
struct CachedState {
    state: ScrollbackModeStateFile,
    runtime_overrides: BTreeMap<(std::path::PathBuf, Uuid, Uuid), ScrollbackMode>,
    loaded_path: Option<std::path::PathBuf>,
    generation: u64,
}

fn cached_state() -> &'static Mutex<CachedState> {
    static STATE: OnceLock<Mutex<CachedState>> = OnceLock::new();
    STATE.get_or_init(|| Mutex::new(CachedState::default()))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct PersistedPaneScrollbackMode {
    pub mode: ScrollbackMode,
    pub updated_epoch_secs: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScrollbackModeStateFile {
    pub version: u32,
    pub panes: BTreeMap<Uuid, BTreeMap<Uuid, PersistedPaneScrollbackMode>>,
}

impl Default for ScrollbackModeStateFile {
    fn default() -> Self {
        Self {
            version: SCROLLBACK_MODE_STATE_VERSION,
            panes: BTreeMap::new(),
        }
    }
}

impl ScrollbackModeStateFile {
    #[must_use]
    pub fn load(paths: &ConfigPaths) -> Self {
        let path = paths.scrollback_mode_state_file();
        let Ok(bytes) = std::fs::read(&path) else {
            return Self::default();
        };
        match serde_json::from_slice::<Self>(&bytes) {
            Ok(state) if state.version == SCROLLBACK_MODE_STATE_VERSION => state,
            Ok(_) => {
                tracing::warn!(path = %path.display(), "ignoring unsupported scrollback mode state version");
                Self::default()
            }
            Err(error) => {
                tracing::warn!(path = %path.display(), %error, "ignoring invalid scrollback mode state");
                Self::default()
            }
        }
    }

    pub fn set(&mut self, session_id: Uuid, pane_id: Uuid, mode: ScrollbackMode) {
        self.panes.entry(session_id).or_default().insert(
            pane_id,
            PersistedPaneScrollbackMode {
                mode,
                updated_epoch_secs: epoch_secs(),
            },
        );
    }

    #[must_use]
    pub fn get(&self, session_id: Uuid, pane_id: Uuid) -> Option<ScrollbackMode> {
        self.panes
            .get(&session_id)?
            .get(&pane_id)
            .map(|entry| entry.mode)
    }

    pub fn prune(&mut self, active_panes: &BTreeMap<Uuid, BTreeSet<Uuid>>, now_epoch_secs: u64) {
        self.panes.retain(|session_id, pane_modes| {
            if let Some(active) = active_panes.get(session_id) {
                pane_modes.retain(|pane_id, _| active.contains(pane_id));
            } else {
                pane_modes.retain(|_, entry| {
                    now_epoch_secs.saturating_sub(entry.updated_epoch_secs)
                        <= ABSENT_SESSION_RETENTION_SECS
                });
            }
            !pane_modes.is_empty()
        });
    }

    pub fn save_atomic(&self, paths: &ConfigPaths) -> Result<()> {
        let path = paths.scrollback_mode_state_file();
        let parent = path
            .parent()
            .context("scrollback mode state path has no parent")?;
        std::fs::create_dir_all(parent).with_context(|| {
            format!(
                "creating scrollback mode state directory {}",
                parent.display()
            )
        })?;
        let temporary = temporary_path(&path);
        let bytes = serde_json::to_vec_pretty(self).context("encoding scrollback mode state")?;
        let mut temporary_file = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&temporary)
            .with_context(|| {
                format!(
                    "opening temporary scrollback mode state {}",
                    temporary.display()
                )
            })?;
        temporary_file.write_all(&bytes).with_context(|| {
            format!(
                "writing temporary scrollback mode state {}",
                temporary.display()
            )
        })?;
        temporary_file.sync_all().with_context(|| {
            format!(
                "syncing temporary scrollback mode state {}",
                temporary.display()
            )
        })?;
        std::fs::rename(&temporary, &path).with_context(|| {
            format!(
                "replacing scrollback mode state {} from {}",
                path.display(),
                temporary.display()
            )
        })?;
        if let Ok(parent_dir) = std::fs::File::open(parent) {
            let _ = parent_dir.sync_all();
        }
        Ok(())
    }
}

pub fn cached_mode(
    paths: &ConfigPaths,
    session_id: Uuid,
    pane_id: Uuid,
    load_persisted: bool,
) -> Option<ScrollbackMode> {
    let path = paths.scrollback_mode_state_file();
    let mut cache = cached_state().lock().ok()?;
    if let Some(mode) = cache
        .runtime_overrides
        .get(&(path.clone(), session_id, pane_id))
        .copied()
    {
        return Some(mode);
    }
    if !load_persisted {
        return None;
    }
    if cache.loaded_path.as_ref() != Some(&path) {
        cache.state = ScrollbackModeStateFile::load(paths);
        cache.loaded_path = Some(path);
    }
    cache.state.get(session_id, pane_id)
}

pub fn set_runtime_mode(
    paths: &ConfigPaths,
    session_id: Uuid,
    pane_id: Uuid,
    mode: ScrollbackMode,
) -> Result<()> {
    let path = paths.scrollback_mode_state_file();
    cached_state()
        .lock()
        .map_err(|_| anyhow::anyhow!("scrollback mode state cache poisoned"))?
        .runtime_overrides
        .insert((path, session_id, pane_id), mode);
    Ok(())
}

pub fn set_cached_mode_debounced(
    paths: ConfigPaths,
    session_id: Uuid,
    pane_id: Uuid,
    mode: ScrollbackMode,
    active_panes: &BTreeMap<Uuid, BTreeSet<Uuid>>,
) -> Result<()> {
    let path = paths.scrollback_mode_state_file();
    set_runtime_mode(&paths, session_id, pane_id, mode)?;
    let generation = {
        let mut cache = cached_state()
            .lock()
            .map_err(|_| anyhow::anyhow!("scrollback mode state cache poisoned"))?;
        if cache.loaded_path.as_ref() != Some(&path) {
            cache.state = ScrollbackModeStateFile::load(&paths);
            cache.loaded_path = Some(path);
        }
        cache.state.set(session_id, pane_id, mode);
        cache.state.prune(active_panes, epoch_secs());
        cache.generation = cache.generation.saturating_add(1);
        cache.generation
    };
    std::thread::spawn(move || {
        std::thread::sleep(SAVE_DEBOUNCE);
        let state = cached_state()
            .lock()
            .ok()
            .and_then(|cache| (cache.generation == generation).then(|| cache.state.clone()));
        if let Some(state) = state
            && let Err(error) = state.save_atomic(&paths)
        {
            tracing::warn!(%error, "failed persisting debounced scrollback mode state");
        }
    });
    Ok(())
}

fn temporary_path(path: &Path) -> std::path::PathBuf {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("scrollback-modes.json");
    path.with_file_name(format!(".{file_name}.{}.tmp", std::process::id()))
}

fn epoch_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_paths() -> ConfigPaths {
        let root = std::env::temp_dir().join(format!(
            "bmux-scrollback-mode-state-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |duration| duration.as_nanos())
        ));
        ConfigPaths::new(
            root.join("config"),
            root.join("runtime"),
            root.join("data"),
            root.join("state"),
        )
    }

    #[test]
    fn runtime_override_works_without_loading_or_writing_persisted_state() {
        let paths = test_paths();
        let session_id = Uuid::new_v4();
        let pane_id = Uuid::new_v4();
        set_runtime_mode(&paths, session_id, pane_id, ScrollbackMode::Frozen)
            .expect("set runtime override");
        assert_eq!(
            cached_mode(&paths, session_id, pane_id, false),
            Some(ScrollbackMode::Frozen)
        );
        assert!(!paths.scrollback_mode_state_file().exists());
    }

    #[test]
    fn debounced_updates_persist_only_the_latest_mode() {
        let paths = test_paths();
        let session_id = Uuid::new_v4();
        let pane_id = Uuid::new_v4();
        let active = BTreeMap::from([(session_id, BTreeSet::from([pane_id]))]);
        set_cached_mode_debounced(
            paths.clone(),
            session_id,
            pane_id,
            ScrollbackMode::Frozen,
            &active,
        )
        .expect("queue frozen state");
        set_cached_mode_debounced(
            paths.clone(),
            session_id,
            pane_id,
            ScrollbackMode::Live,
            &active,
        )
        .expect("queue live state");
        std::thread::sleep(SAVE_DEBOUNCE + Duration::from_millis(100));
        assert_eq!(
            ScrollbackModeStateFile::load(&paths).get(session_id, pane_id),
            Some(ScrollbackMode::Live)
        );
        std::fs::remove_dir_all(paths.state_dir()).ok();
    }

    #[test]
    fn state_round_trips_atomically() {
        let paths = test_paths();
        let session_id = Uuid::new_v4();
        let pane_id = Uuid::new_v4();
        let mut state = ScrollbackModeStateFile::default();
        state.set(session_id, pane_id, ScrollbackMode::Frozen);
        state.save_atomic(&paths).expect("save state");
        assert_eq!(
            ScrollbackModeStateFile::load(&paths).get(session_id, pane_id),
            Some(ScrollbackMode::Frozen)
        );
        std::fs::remove_dir_all(paths.state_dir()).ok();
    }

    #[test]
    fn invalid_state_falls_back_to_default() {
        let paths = test_paths();
        let path = paths.scrollback_mode_state_file();
        std::fs::create_dir_all(path.parent().expect("state parent")).expect("create state parent");
        std::fs::write(&path, b"not-json").expect("write invalid state");
        assert!(ScrollbackModeStateFile::load(&paths).panes.is_empty());
        std::fs::remove_dir_all(paths.state_dir()).ok();
    }

    #[test]
    fn prune_removes_dead_panes_but_retains_recent_absent_sessions() {
        let current_session = Uuid::new_v4();
        let active_pane = Uuid::new_v4();
        let dead_pane = Uuid::new_v4();
        let absent_session = Uuid::new_v4();
        let absent_pane = Uuid::new_v4();
        let mut state = ScrollbackModeStateFile::default();
        state.set(current_session, active_pane, ScrollbackMode::Frozen);
        state.set(current_session, dead_pane, ScrollbackMode::Frozen);
        state.set(absent_session, absent_pane, ScrollbackMode::Live);
        let now = epoch_secs();
        let active = BTreeMap::from([(current_session, BTreeSet::from([active_pane]))]);
        state.prune(&active, now);
        assert_eq!(
            state.get(current_session, active_pane),
            Some(ScrollbackMode::Frozen)
        );
        assert_eq!(state.get(current_session, dead_pane), None);
        assert_eq!(
            state.get(absent_session, absent_pane),
            Some(ScrollbackMode::Live)
        );
    }
}
