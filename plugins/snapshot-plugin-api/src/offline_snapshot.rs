//! Offline snapshot-mutation utilities.
//!
//! When the server is not running, CLI subcommands like
//! `bmux kill-session` still need to prune entries from the persisted
//! snapshot so killed sessions don't re-resurrect on the next server
//! start. This module hosts those utilities: it reads the combined
//! envelope written by [`crate::envelope::CombinedSnapshotEnvelope`],
//! decodes each section it cares about (sessions / contexts /
//! follow-state / pane-runtime), mutates the in-memory structures,
//! re-encodes, and atomically writes the envelope back.
//!
//! File-level mutual exclusion is guaranteed by a `.lock` sidecar
//! file acquired via `O_CREATE|O_EXCL`. Callers that might race with
//! a running server (e.g. `bmux server save` while the user kills a
//! session) should either run with the server down, or rely on the
//! lock's retry/backoff to serialize.

#![allow(clippy::module_name_repetitions)]

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use bmux_client_state::FollowStateSnapshot;
use bmux_config::ConfigPaths;
use bmux_context_state::ContextStateSnapshot;
use bmux_ipc::SessionSelector;
use bmux_session_models::{Session, SessionId};
use bmux_session_state::SessionManagerSnapshot;
use serde::Deserialize;
use uuid::Uuid;

use crate::envelope::{CombinedSnapshotEnvelope, SectionV1};

const OFFLINE_SNAPSHOT_LOCK_RETRY_INTERVAL: Duration = Duration::from_millis(50);
const OFFLINE_SNAPSHOT_LOCK_TIMEOUT: Duration = Duration::from_secs(3);

/// Section id for the sessions plugin's snapshot payload. Must match
/// `plugins/sessions-plugin`'s `SESSIONS_STATEFUL_ID`.
const SESSIONS_SECTION_ID: &str = "bmux.sessions/session-manager";
/// Section id for the contexts plugin's snapshot payload.
const CONTEXTS_SECTION_ID: &str = "bmux.contexts/context-state";
/// Section id for the clients plugin's snapshot payload.
const CLIENTS_SECTION_ID: &str = "bmux.clients/follow-state";
/// Section id for the server's pane-runtime snapshot payload. Matches
/// `packages/server/src/pane_runtime_snapshot.rs`'s `SERVER_PANE_RUNTIME_ID`.
const PANE_RUNTIME_SECTION_ID: &str = "bmux.server/pane-runtime";

/// Default snapshot file name. Must match the value registered by
/// CLI bootstrap into `SnapshotPluginConfig.snapshot_path.file_name`.
const DEFAULT_SNAPSHOT_FILENAME: &str = "bmux-snapshot-v1.json";

// ── Public API ──────────────────────────────────────────────────────

/// Target for offline session kill: one specific session or all of them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OfflineSessionKillTarget {
    All,
    One(SessionSelector),
}

/// Result of an offline session kill.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OfflineSessionKillReport {
    /// Whether a snapshot file existed on disk at the time of the call.
    /// `false` means the CLI fell through to "server is not running"
    /// with nothing to prune.
    pub had_snapshot: bool,
    /// Session ids that were removed from the snapshot.
    pub removed_session_ids: Vec<Uuid>,
    /// Context ids that were removed because their binding pointed at
    /// a removed session.
    pub removed_context_ids: Vec<Uuid>,
}

/// Kill sessions offline (without a running server) by editing the
/// persisted combined snapshot file. Reads the envelope, resolves the
/// target, terminates process groups for panes in removed sessions,
/// prunes session / context / follow-state entries, and writes the
/// updated envelope atomically.
///
/// # Errors
///
/// Returns an error if the snapshot cannot be read, decoded, encoded,
/// or written; or if the file lock cannot be acquired within the
/// timeout.
#[allow(clippy::needless_pass_by_value)] // Public API; by-value matches caller idiom.
pub fn offline_kill_sessions(
    target: OfflineSessionKillTarget,
) -> anyhow::Result<OfflineSessionKillReport> {
    let paths = ConfigPaths::default();
    let snapshot_path = snapshot_path_from_config(&paths);
    if !snapshot_path.exists() {
        return Ok(OfflineSessionKillReport {
            had_snapshot: false,
            ..OfflineSessionKillReport::default()
        });
    }

    let _lock = acquire_offline_snapshot_lock(&snapshot_path)?;

    let mut envelope = match read_envelope(&snapshot_path) {
        Ok(e) => e,
        Err(OfflineSnapshotError::NotFound) => {
            return Ok(OfflineSessionKillReport {
                had_snapshot: false,
                ..OfflineSessionKillReport::default()
            });
        }
        Err(error) if matches!(target, OfflineSessionKillTarget::All) => {
            // If we can't even decode the envelope and the user asked
            // to wipe everything, remove the file so the next server
            // start is clean.
            if let Err(remove_error) = std::fs::remove_file(&snapshot_path)
                && remove_error.kind() != std::io::ErrorKind::NotFound
            {
                anyhow::bail!(
                    "failed reading snapshot for offline kill ({error}); failed removing invalid snapshot: {remove_error}"
                );
            }
            return Ok(OfflineSessionKillReport {
                had_snapshot: true,
                ..OfflineSessionKillReport::default()
            });
        }
        Err(error) => anyhow::bail!("failed reading snapshot for offline kill: {error}"),
    };

    // Decode the sessions section so we can resolve the selector.
    let Some(mut sessions_snapshot) =
        decode_section::<SessionManagerSnapshot>(&envelope, SESSIONS_SECTION_ID)?
    else {
        // No sessions section → nothing to kill.
        return Ok(OfflineSessionKillReport {
            had_snapshot: true,
            ..OfflineSessionKillReport::default()
        });
    };

    // Resolve target → removed_session_ids.
    let removed_session_ids = match &target {
        OfflineSessionKillTarget::All => sessions_snapshot
            .0
            .iter()
            .map(|s| s.id.0)
            .collect::<Vec<_>>(),
        OfflineSessionKillTarget::One(selector) => {
            resolve_session_selector(&sessions_snapshot.0, selector)
                .into_iter()
                .collect::<Vec<_>>()
        }
    };

    if removed_session_ids.is_empty() {
        return Ok(OfflineSessionKillReport {
            had_snapshot: true,
            ..OfflineSessionKillReport::default()
        });
    }

    let removed_set: BTreeSet<Uuid> = removed_session_ids.iter().copied().collect();

    // Kill process groups belonging to panes in removed sessions.
    // Decode the pane-runtime section opaquely — we only need
    // `sessions[].session_id` and `sessions[].panes[].process_group_id`
    // to drive `terminate_process_group`. Keeping the decode shallow
    // means this plugin-api crate does not have to duplicate the full
    // pane-runtime schema that lives in `packages/server`.
    if let Some(pane_runtime_value) =
        get_section_json::<serde_json::Value>(&envelope, PANE_RUNTIME_SECTION_ID)?
    {
        terminate_process_groups_for_removed_sessions(&pane_runtime_value, &removed_set);
    }

    // Prune sessions section.
    sessions_snapshot
        .0
        .retain(|session| !removed_set.contains(&session.id.0));
    let sessions_version = section_version(&envelope, SESSIONS_SECTION_ID).unwrap_or(1);
    replace_section(
        &mut envelope,
        SESSIONS_SECTION_ID,
        &sessions_snapshot,
        sessions_version,
    )?;

    // Drop pane-runtime entries for removed sessions.
    prune_pane_runtime_for_removed_sessions(&mut envelope, &removed_set)?;

    // Mutate contexts section — drop bindings pointing at removed
    // sessions, drop orphan contexts, clear selections, prune MRU.
    let removed_context_ids = prune_contexts_for_removed_sessions(&mut envelope, &removed_set)?;

    // Mutate follow-state section — clear selected_session /
    // selected_context entries that point at removed sessions or
    // contexts.
    prune_follow_state_for_removed(&mut envelope, &removed_set, &removed_context_ids)?;

    // Rebuild envelope checksum with the mutated sections, then write
    // atomically.
    let rebuilt = CombinedSnapshotEnvelope::build(envelope.sections)
        .map_err(|e| anyhow::anyhow!("rebuilding snapshot envelope: {e}"))?;
    write_envelope_atomic(&snapshot_path, &rebuilt)
        .map_err(|e| anyhow::anyhow!("writing snapshot for offline kill: {e}"))?;

    Ok(OfflineSessionKillReport {
        had_snapshot: true,
        removed_session_ids,
        removed_context_ids,
    })
}

// ── Errors ──────────────────────────────────────────────────────────

#[derive(Debug, thiserror::Error)]
enum OfflineSnapshotError {
    #[error("snapshot file not found")]
    NotFound,
    #[error("snapshot I/O error: {0}")]
    Io(String),
    #[error("snapshot codec error: {0}")]
    Codec(String),
}

impl From<std::io::Error> for OfflineSnapshotError {
    fn from(err: std::io::Error) -> Self {
        if err.kind() == std::io::ErrorKind::NotFound {
            Self::NotFound
        } else {
            Self::Io(err.to_string())
        }
    }
}

impl From<serde_json::Error> for OfflineSnapshotError {
    fn from(err: serde_json::Error) -> Self {
        Self::Codec(err.to_string())
    }
}

// ── Path + I/O helpers ──────────────────────────────────────────────

fn snapshot_path_from_config(paths: &ConfigPaths) -> PathBuf {
    paths
        .data_dir
        .join("runtime")
        .join(DEFAULT_SNAPSHOT_FILENAME)
}

fn read_envelope(path: &Path) -> Result<CombinedSnapshotEnvelope, OfflineSnapshotError> {
    let bytes = std::fs::read(path)?;
    let envelope: CombinedSnapshotEnvelope = serde_json::from_slice(&bytes)?;
    envelope
        .validate()
        .map_err(|e| OfflineSnapshotError::Codec(e.to_string()))?;
    Ok(envelope)
}

fn write_envelope_atomic(
    path: &Path,
    envelope: &CombinedSnapshotEnvelope,
) -> Result<(), OfflineSnapshotError> {
    let bytes = serde_json::to_vec_pretty(envelope)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut temp_path = path.to_path_buf();
    let temp_name = path.file_name().map_or_else(
        || "bmux-snapshot.tmp".to_string(),
        |name| format!("{}.tmp", name.to_string_lossy()),
    );
    temp_path.set_file_name(temp_name);

    let mut temp_file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&temp_path)?;
    temp_file.write_all(&bytes)?;
    temp_file.sync_all()?;
    std::fs::rename(&temp_path, path)?;
    if let Some(parent) = path.parent()
        && let Ok(parent_dir) = std::fs::File::open(parent)
    {
        let _ = parent_dir.sync_all();
    }
    Ok(())
}

// ── Locking ─────────────────────────────────────────────────────────

struct OfflineSnapshotMutationLock {
    path: PathBuf,
}

impl Drop for OfflineSnapshotMutationLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn acquire_offline_snapshot_lock(
    snapshot_path: &Path,
) -> anyhow::Result<OfflineSnapshotMutationLock> {
    let parent = snapshot_path.parent().ok_or_else(|| {
        anyhow::anyhow!("failed acquiring offline snapshot lock: snapshot has no parent directory")
    })?;
    std::fs::create_dir_all(parent).map_err(|e| {
        anyhow::anyhow!(
            "failed creating snapshot directory {}: {e}",
            parent.display()
        )
    })?;
    let lock_name = snapshot_path.file_name().map_or_else(
        || "bmux-snapshot.lock".to_string(),
        |name| format!("{}.lock", name.to_string_lossy()),
    );
    let lock_path = parent.join(lock_name);
    let started = Instant::now();

    loop {
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&lock_path)
        {
            Ok(mut file) => {
                let _ = writeln!(file, "pid={}", std::process::id());
                return Ok(OfflineSnapshotMutationLock { path: lock_path });
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                if started.elapsed() >= OFFLINE_SNAPSHOT_LOCK_TIMEOUT {
                    anyhow::bail!(
                        "timed out waiting for snapshot lock {}; retry once no other snapshot mutation is in progress",
                        lock_path.display()
                    );
                }
                std::thread::sleep(OFFLINE_SNAPSHOT_LOCK_RETRY_INTERVAL);
            }
            Err(error) => {
                anyhow::bail!(
                    "failed acquiring snapshot lock {}: {error}",
                    lock_path.display()
                );
            }
        }
    }
}

// ── Section decode / encode helpers ─────────────────────────────────

fn decode_section<T: for<'de> Deserialize<'de>>(
    envelope: &CombinedSnapshotEnvelope,
    id: &str,
) -> anyhow::Result<Option<T>> {
    let Some(section) = envelope.sections.iter().find(|s| s.id == id) else {
        return Ok(None);
    };
    let decoded: T = serde_json::from_slice(&section.bytes)
        .map_err(|e| anyhow::anyhow!("decoding section '{id}': {e}"))?;
    Ok(Some(decoded))
}

fn get_section_json<T: for<'de> Deserialize<'de>>(
    envelope: &CombinedSnapshotEnvelope,
    id: &str,
) -> anyhow::Result<Option<T>> {
    decode_section(envelope, id)
}

fn section_version(envelope: &CombinedSnapshotEnvelope, id: &str) -> Option<u32> {
    envelope
        .sections
        .iter()
        .find(|s| s.id == id)
        .map(|s| s.version)
}

fn replace_section<T: serde::Serialize>(
    envelope: &mut CombinedSnapshotEnvelope,
    id: &str,
    payload: &T,
    version: u32,
) -> anyhow::Result<()> {
    let bytes =
        serde_json::to_vec(payload).map_err(|e| anyhow::anyhow!("encoding section '{id}': {e}"))?;
    if let Some(section) = envelope.sections.iter_mut().find(|s| s.id == id) {
        section.bytes = bytes;
        section.version = version;
    } else {
        envelope.sections.push(SectionV1 {
            id: id.to_string(),
            version,
            bytes,
        });
    }
    Ok(())
}

// ── Selector resolution ─────────────────────────────────────────────

fn resolve_session_selector(sessions: &[Session], selector: &SessionSelector) -> Option<Uuid> {
    match selector {
        SessionSelector::ById(raw_id) => {
            sessions.iter().find(|s| s.id.0 == *raw_id).map(|s| s.id.0)
        }
        SessionSelector::ByName(value) => {
            if let Some(session) = sessions
                .iter()
                .find(|s| s.name.as_deref() == Some(value.as_str()))
            {
                return Some(session.id.0);
            }
            if let Some(session) = sessions
                .iter()
                .find(|s| s.id.0.to_string().eq_ignore_ascii_case(value))
            {
                return Some(session.id.0);
            }
            let value_lower = value.to_ascii_lowercase();
            sessions
                .iter()
                .find(|s| {
                    s.id.0
                        .to_string()
                        .to_ascii_lowercase()
                        .starts_with(&value_lower)
                })
                .map(|s| s.id.0)
        }
    }
}

// ── Pane-runtime shallow decode (process-group termination) ────────

/// Walk the pane-runtime section as a `serde_json::Value`, extract the
/// set of process-group ids belonging to panes of removed sessions,
/// and terminate each.
fn terminate_process_groups_for_removed_sessions(
    pane_runtime_value: &serde_json::Value,
    removed_sessions: &BTreeSet<Uuid>,
) {
    let Some(sessions) = pane_runtime_value
        .get("sessions")
        .and_then(|v| v.as_array())
    else {
        return;
    };

    let mut groups = BTreeSet::new();
    for session in sessions {
        let Some(session_id) = session
            .get("session_id")
            .and_then(serde_json::Value::as_str)
            .and_then(|s| Uuid::parse_str(s).ok())
            .or_else(|| {
                // serde may emit `session_id` as raw bytes; handle
                // both the string form (`Uuid::to_string()`) and the
                // object/tuple form defensively.
                session
                    .get("session_id")
                    .and_then(serde_json::Value::as_array)
                    .and_then(|arr| {
                        if arr.len() == 16 {
                            let bytes: Option<Vec<u8>> = arr
                                .iter()
                                .map(|b| b.as_u64().and_then(|n| u8::try_from(n).ok()))
                                .collect();
                            bytes.and_then(|bytes| {
                                <[u8; 16]>::try_from(bytes.as_slice())
                                    .ok()
                                    .map(Uuid::from_bytes)
                            })
                        } else {
                            None
                        }
                    })
            })
        else {
            continue;
        };
        if !removed_sessions.contains(&session_id) {
            continue;
        }
        let Some(panes) = session.get("panes").and_then(|v| v.as_array()) else {
            continue;
        };
        for pane in panes {
            if let Some(pgid) = pane
                .get("process_group_id")
                .and_then(serde_json::Value::as_i64)
                .and_then(|n| i32::try_from(n).ok())
                .filter(|&n| n > 0)
            {
                groups.insert(pgid);
            }
        }
    }

    for process_group_id in groups {
        let _ = terminate_process_group(process_group_id);
    }
}

fn prune_pane_runtime_for_removed_sessions(
    envelope: &mut CombinedSnapshotEnvelope,
    removed_sessions: &BTreeSet<Uuid>,
) -> anyhow::Result<()> {
    let Some(section) = envelope
        .sections
        .iter_mut()
        .find(|s| s.id == PANE_RUNTIME_SECTION_ID)
    else {
        return Ok(());
    };
    let mut value: serde_json::Value = serde_json::from_slice(&section.bytes)
        .map_err(|e| anyhow::anyhow!("decoding pane-runtime section: {e}"))?;
    if let Some(sessions_array) = value.get_mut("sessions").and_then(|v| v.as_array_mut()) {
        sessions_array.retain(|session_value| {
            let Some(session_id) = session_value
                .get("session_id")
                .and_then(serde_json::Value::as_str)
                .and_then(|s| Uuid::parse_str(s).ok())
            else {
                return true; // keep anything we can't parse
            };
            !removed_sessions.contains(&session_id)
        });
    }
    section.bytes = serde_json::to_vec(&value)
        .map_err(|e| anyhow::anyhow!("re-encoding pane-runtime section: {e}"))?;
    Ok(())
}

// ── Contexts section mutation ───────────────────────────────────────

fn prune_contexts_for_removed_sessions(
    envelope: &mut CombinedSnapshotEnvelope,
    removed_sessions: &BTreeSet<Uuid>,
) -> anyhow::Result<Vec<Uuid>> {
    let Some(mut contexts) = decode_section::<ContextStateSnapshot>(envelope, CONTEXTS_SECTION_ID)?
    else {
        return Ok(Vec::new());
    };

    let removed_contexts: Vec<Uuid> = contexts
        .session_by_context
        .iter()
        .filter_map(|(context_id, session_id)| {
            removed_sessions
                .contains(&session_id.0)
                .then_some(*context_id)
        })
        .collect();
    let removed_context_set: BTreeSet<Uuid> = removed_contexts.iter().copied().collect();

    contexts
        .session_by_context
        .retain(|context_id, _| !removed_context_set.contains(context_id));
    contexts
        .contexts
        .retain(|context_id, _| !removed_context_set.contains(context_id));
    contexts
        .mru_contexts
        .retain(|context_id| !removed_context_set.contains(context_id));

    let mut new_selected: BTreeMap<_, _> = BTreeMap::new();
    for (client_id, context_id) in &contexts.selected_by_client {
        if !removed_context_set.contains(context_id) {
            new_selected.insert(*client_id, *context_id);
        }
    }
    contexts.selected_by_client = new_selected;

    let contexts_version = section_version(envelope, CONTEXTS_SECTION_ID).unwrap_or(1);
    replace_section(envelope, CONTEXTS_SECTION_ID, &contexts, contexts_version)?;

    Ok(removed_contexts)
}

// ── Follow-state section mutation ───────────────────────────────────

fn prune_follow_state_for_removed(
    envelope: &mut CombinedSnapshotEnvelope,
    removed_sessions: &BTreeSet<Uuid>,
    removed_contexts: &[Uuid],
) -> anyhow::Result<()> {
    let Some(mut follow) = decode_section::<FollowStateSnapshot>(envelope, CLIENTS_SECTION_ID)?
    else {
        return Ok(());
    };

    let removed_context_set: BTreeSet<Uuid> = removed_contexts.iter().copied().collect();

    for selected_session in follow.selected_sessions.values_mut() {
        if let Some(SessionId(session_id)) = *selected_session
            && removed_sessions.contains(&session_id)
        {
            *selected_session = None;
        }
    }
    for selected_context in follow.selected_contexts.values_mut() {
        if let Some(context_id) = *selected_context
            && removed_context_set.contains(&context_id)
        {
            *selected_context = None;
        }
    }

    let follow_version = section_version(envelope, CLIENTS_SECTION_ID).unwrap_or(1);
    replace_section(envelope, CLIENTS_SECTION_ID, &follow, follow_version)?;

    Ok(())
}

// ── Process-group termination (relocated from server) ───────────────

#[cfg(unix)]
fn terminate_process_group(process_group_id: i32) -> bool {
    if process_group_id <= 0 {
        return false;
    }
    let sent_term = std::process::Command::new("kill")
        .arg("-TERM")
        .arg(format!("-{process_group_id}"))
        .status()
        .is_ok_and(|status| status.success());

    std::thread::sleep(Duration::from_millis(120));

    let group_still_alive = std::process::Command::new("kill")
        .arg("-0")
        .arg(format!("-{process_group_id}"))
        .status()
        .is_ok_and(|status| status.success());

    if group_still_alive {
        return std::process::Command::new("kill")
            .arg("-KILL")
            .arg(format!("-{process_group_id}"))
            .status()
            .is_ok_and(|status| status.success())
            || sent_term;
    }
    sent_term
}

/// On Windows there are no POSIX process groups. `taskkill /T` kills
/// the process tree rooted at a PID; we use the value stored in
/// `process_group_id` (which on Windows is the PID itself) as the
/// tree-kill target.
#[cfg(windows)]
fn terminate_process_group(process_group_id: i32) -> bool {
    if process_group_id <= 0 {
        return false;
    }
    let pid = process_group_id.to_string();
    let sent_term = std::process::Command::new("taskkill")
        .args(["/PID", &pid, "/T"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);

    std::thread::sleep(Duration::from_millis(120));

    let still_alive = std::process::Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/NH"])
        .output()
        .map(|o| o.status.success() && String::from_utf8_lossy(&o.stdout).contains(&pid))
        .unwrap_or(false);

    if still_alive {
        return std::process::Command::new("taskkill")
            .args(["/PID", &pid, "/T", "/F"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
            || sent_term;
    }
    sent_term
}

#[cfg(not(any(unix, windows)))]
fn terminate_process_group(_process_group_id: i32) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use bmux_session_models::{ClientId, Session, SessionId};
    use std::collections::{BTreeSet, VecDeque};
    use tempfile::tempdir;

    fn make_envelope(sessions: &[Session], bindings: &[(Uuid, Uuid)]) -> CombinedSnapshotEnvelope {
        let sessions_bytes =
            serde_json::to_vec(&SessionManagerSnapshot(sessions.to_vec())).unwrap();
        let sessions_section = SectionV1 {
            id: SESSIONS_SECTION_ID.to_string(),
            version: 1,
            bytes: sessions_bytes,
        };

        let contexts_snapshot = ContextStateSnapshot {
            contexts: bindings
                .iter()
                .map(|(context_id, _)| {
                    (
                        *context_id,
                        bmux_context_state::RuntimeContext {
                            id: *context_id,
                            name: None,
                            attributes: BTreeMap::new(),
                        },
                    )
                })
                .collect(),
            session_by_context: bindings.iter().map(|(c, s)| (*c, SessionId(*s))).collect(),
            selected_by_client: BTreeMap::new(),
            mru_contexts: bindings.iter().map(|(c, _)| *c).collect::<VecDeque<_>>(),
        };
        let contexts_bytes = serde_json::to_vec(&contexts_snapshot).unwrap();
        let contexts_section = SectionV1 {
            id: CONTEXTS_SECTION_ID.to_string(),
            version: 1,
            bytes: contexts_bytes,
        };

        let follow_snapshot = FollowStateSnapshot {
            connected_clients: BTreeSet::new(),
            selected_contexts: BTreeMap::new(),
            selected_sessions: {
                let mut m = BTreeMap::new();
                if let Some(first) = sessions.first() {
                    m.insert(ClientId(Uuid::new_v4()), Some(first.id));
                }
                m
            },
            follows: BTreeMap::new(),
            attached_stream_sessions: BTreeMap::new(),
            attach_detach_allowed: BTreeMap::new(),
        };
        let follow_bytes = serde_json::to_vec(&follow_snapshot).unwrap();
        let follow_section = SectionV1 {
            id: CLIENTS_SECTION_ID.to_string(),
            version: 1,
            bytes: follow_bytes,
        };

        let pane_runtime_json = serde_json::json!({
            "sessions": sessions.iter().map(|s| {
                serde_json::json!({
                    "session_id": s.id.0.to_string(),
                    "panes": [],
                    "focused_pane_id": null,
                    "layout_root": null,
                    "floating_surfaces": [],
                })
            }).collect::<Vec<_>>()
        });
        let pane_runtime_section = SectionV1 {
            id: PANE_RUNTIME_SECTION_ID.to_string(),
            version: 1,
            bytes: serde_json::to_vec(&pane_runtime_json).unwrap(),
        };

        CombinedSnapshotEnvelope::build(vec![
            sessions_section,
            contexts_section,
            follow_section,
            pane_runtime_section,
        ])
        .unwrap()
    }

    #[test]
    fn kill_one_by_name_prunes_session_context_and_selection() {
        let tmp = tempdir().unwrap();
        let snapshot_path = tmp.path().join(DEFAULT_SNAPSHOT_FILENAME);

        let session_id = Uuid::new_v4();
        let context_id = Uuid::new_v4();
        let sessions = vec![Session {
            id: SessionId(session_id),
            name: Some("dev".into()),
            clients: BTreeSet::new(),
        }];
        let envelope = make_envelope(&sessions, &[(context_id, session_id)]);
        write_envelope_atomic(&snapshot_path, &envelope).unwrap();

        // Manually invoke the low-level prune functions (the public
        // `offline_kill_sessions` reads `ConfigPaths::default()`; we
        // can't redirect that within a test without env var gymnastics).
        let mut decoded = read_envelope(&snapshot_path).unwrap();
        let mut removed = BTreeSet::new();
        removed.insert(session_id);

        // Prune sessions section.
        let mut sessions_snap: SessionManagerSnapshot =
            decode_section(&decoded, SESSIONS_SECTION_ID)
                .unwrap()
                .unwrap();
        sessions_snap.0.retain(|s| !removed.contains(&s.id.0));
        replace_section(&mut decoded, SESSIONS_SECTION_ID, &sessions_snap, 1).unwrap();

        prune_pane_runtime_for_removed_sessions(&mut decoded, &removed).unwrap();
        let removed_contexts = prune_contexts_for_removed_sessions(&mut decoded, &removed).unwrap();
        assert_eq!(removed_contexts, vec![context_id]);
        prune_follow_state_for_removed(&mut decoded, &removed, &removed_contexts).unwrap();

        // Sessions section is empty now.
        let sessions_after: SessionManagerSnapshot = decode_section(&decoded, SESSIONS_SECTION_ID)
            .unwrap()
            .unwrap();
        assert!(sessions_after.0.is_empty());

        // Contexts section has no entries.
        let contexts_after: ContextStateSnapshot = decode_section(&decoded, CONTEXTS_SECTION_ID)
            .unwrap()
            .unwrap();
        assert!(contexts_after.contexts.is_empty());
        assert!(contexts_after.session_by_context.is_empty());

        // Follow-state selected_sessions entries that referred to the
        // killed session are now None.
        let follow_after: FollowStateSnapshot = decode_section(&decoded, CLIENTS_SECTION_ID)
            .unwrap()
            .unwrap();
        assert!(
            follow_after
                .selected_sessions
                .values()
                .all(std::option::Option::is_none)
        );
    }

    #[test]
    fn resolve_selector_matches_by_name() {
        let sessions = vec![Session {
            id: SessionId(Uuid::new_v4()),
            name: Some("dev".into()),
            clients: BTreeSet::new(),
        }];
        let result = resolve_session_selector(&sessions, &SessionSelector::ByName("dev".into()));
        assert_eq!(result, Some(sessions[0].id.0));
    }

    #[test]
    fn resolve_selector_matches_by_uuid_prefix() {
        let id = Uuid::new_v4();
        let sessions = vec![Session {
            id: SessionId(id),
            name: None,
            clients: BTreeSet::new(),
        }];
        let prefix = id.to_string()[..8].to_string();
        let result = resolve_session_selector(&sessions, &SessionSelector::ByName(prefix));
        assert_eq!(result, Some(id));
    }
}
