//! Remembered local targets, separate from declarative config and runtime state.
//! Version two preserves pane-view overrides on window-only updates; version one
//! remains readable. Pane records require explicit verified caller bindings.
//!
//! The key uses ordered selected source locators (not file contents/presence),
//! profile/scope selectors and a caller-supplied stable runtime target. Callers
//! should use the selected runtime's durable roots/endpoint identity, not its
//! artifact hash, boot nonce or configuration-content hash. Native typography
//! edits therefore neither lose preferences nor change companion identity.

use std::collections::BTreeSet;
use std::fs::{File, OpenOptions};
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use crate::configuration::ResolvedConfiguration;
use crate::pane_zoom::PaneViewRestoreToken;

const VERSION: u32 = 2;
const MAX_PANE_ENTRIES: usize = 64;
const MAX_BYTES: u64 = 64 * 1024;
const MAX_ENTRIES: usize = 64;

/// Last requested window or pane-view target, never an intermediate animation sample.
/// The zoom controller applies current configured limits when restoring this value.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ZoomPreferences {
    pub target_scale: f64,
}

impl ZoomPreferences {
    pub fn validate(self) -> Result<()> {
        ensure!(
            self.target_scale.is_finite() && self.target_scale > 0.0,
            "remembered window scale must be positive and finite"
        );
        Ok(())
    }
}

/// A store has no side effects until `save`. No ambient environment, global
/// state root, config file rewrite or background task is involved.
#[derive(Debug, Clone)]
pub struct PreferenceStore {
    path: PathBuf,
    key: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Document {
    version: u32,
    /// Least recently saved first. Reads intentionally do not mutate state.
    entries: Vec<Entry>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    key: String,
    preferences: ZoomPreferences,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    panes: Vec<PaneEntry>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PaneEntry {
    key: String,
    preferences: ZoomPreferences,
}

impl PreferenceStore {
    /// `runtime_target` must identify the selected runtime, including external
    /// or sandbox routing. It is hashed, never persisted as plaintext.
    pub fn for_configuration(
        configuration: &ResolvedConfiguration,
        runtime_target: &str,
    ) -> Result<Self> {
        ensure!(
            !runtime_target.is_empty() && runtime_target.len() <= 16 * 1024,
            "invalid zoom preference runtime target identity"
        );
        let inputs = &configuration.inputs;
        crate::runtime_paths::validate_absolute(&inputs.paths.state_dir)?;
        let locators: Vec<_> = [&inputs.bterm, &inputs.bmux]
            .into_iter()
            .map(|family| {
                family
                    .documents
                    .iter()
                    .map(|document| {
                        (
                            &document.source.path,
                            &document.inline_origin,
                            document.source.required,
                            document.local,
                        )
                    })
                    .collect::<Vec<_>>()
            })
            .collect();
        let scope = inputs
            .options
            .scope
            .as_ref()
            .map(|scope| (&scope.name, &scope.attributes));
        let selection = serde_json::to_vec(&(
            locators,
            &inputs.options.forced_profile,
            scope,
            runtime_target,
        ))?;
        let mut digest = Sha256::new();
        digest.update(b"bterm-window-zoom-preferences-v1\0");
        digest.update(selection);
        let key: String = digest
            .finalize()
            .iter()
            .flat_map(|byte| {
                const HEX: &[u8; 16] = b"0123456789abcdef";
                [
                    char::from(HEX[usize::from(byte >> 4)]),
                    char::from(HEX[usize::from(byte & 0x0f)]),
                ]
            })
            .collect();
        Ok(Self {
            path: inputs.paths.state_dir.join("window-zoom-preferences.json"),
            key,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Missing state is normal. Corrupt, oversized or unsupported state is an
    /// error, not an apparently healthy default and not silently overwritten.
    pub fn load(&self) -> Result<Option<ZoomPreferences>> {
        Ok(self
            .read_document()?
            .entries
            .into_iter()
            .find(|entry| entry.key == self.key)
            .map(|entry| entry.preferences))
    }

    /// Nonblocking serialized read/modify/replace, with bounded retention. Last
    /// successfully saved target wins across windows sharing the same identity.
    /// Call from a debounced persistence worker, not the presentation hot path.
    pub fn save(&self, preferences: ZoomPreferences) -> Result<()> {
        preferences.validate()?;
        self.update_entry(|entry| entry.preferences = preferences)
    }

    /// Restore only for an explicitly verified durable local-view/pane binding.
    /// Absence is normal; corrupt or unsupported documents remain errors.
    pub fn load_pane(&self, token: &PaneViewRestoreToken) -> Result<Option<ZoomPreferences>> {
        let key = token.binding().preference_key();
        Ok(self
            .read_document()?
            .entries
            .into_iter()
            .find(|entry| entry.key == self.key)
            .and_then(|entry| entry.panes.into_iter().find(|pane| pane.key == key))
            .map(|pane| pane.preferences))
    }

    /// Save a target, never an animation sample. Call from the caller's debounced
    /// IO worker. The lock covers read/modify/replace across window and pane saves.
    /// Bounded least-recently-saved pane retention (64 per runtime/config namespace)
    /// is separate from live state. The entire document remains capped at 64 KiB.
    pub fn save_pane(
        &self,
        token: &PaneViewRestoreToken,
        preferences: ZoomPreferences,
    ) -> Result<()> {
        preferences.validate()?;
        let key = token.binding().preference_key();
        self.update_entry(|entry| {
            entry.panes.retain(|pane| pane.key != key);
            if entry.panes.len() == MAX_PANE_ENTRIES {
                entry.panes.remove(0);
            }
            entry.panes.push(PaneEntry { key, preferences });
        })
    }

    fn update_entry(&self, update: impl FnOnce(&mut Entry)) -> Result<()> {
        let parent = self
            .path
            .parent()
            .context("zoom preference parent missing")?;
        create_directory_durably(parent)?;
        // A permanent lock inode must not be removed while other writers may
        // have it open. OS locks are released on crash; no stale-PID guessing.
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
        }
        let lock = options
            .open(self.path.with_extension("lock"))
            .context("open zoom preference lock")?;
        ensure!(
            lock.metadata()?.is_file(),
            "zoom preference lock must be a regular file"
        );
        lock.try_lock()
            .context("zoom preferences are being saved by another window")?;
        let mut document = self.read_document()?;
        let mut entry = if let Some(index) = document
            .entries
            .iter()
            .position(|entry| entry.key == self.key)
        {
            document.entries.remove(index)
        } else {
            Entry {
                key: self.key.clone(),
                preferences: ZoomPreferences { target_scale: 1.0 },
                panes: Vec::new(),
            }
        };
        update(&mut entry);
        if document.entries.len() == MAX_ENTRIES {
            document.entries.remove(0);
        }
        document.entries.push(entry);
        document.version = VERSION;
        let bytes = serde_json::to_vec(&document)?;
        ensure!(
            u64::try_from(bytes.len())? <= MAX_BYTES,
            "zoom preferences exceed byte limit"
        );
        let temporary = self.path.with_extension("tmp");
        // Only this locked writer owns this fixed scratch name. Unlink (never
        // follow) any interrupted scratch file before exclusively creating it.
        match std::fs::remove_file(&temporary) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("remove interrupted zoom preference write"),
        }
        let result = (|| -> Result<()> {
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt as _;
                options.mode(0o600);
            }
            let mut file = options
                .open(&temporary)
                .context("create atomic zoom preference write")?;
            file.write_all(&bytes).context("write zoom preferences")?;
            file.sync_all().context("sync zoom preferences")?;
            std::fs::rename(&temporary, &self.path).context("publish zoom preferences")?;
            File::open(parent)?
                .sync_all()
                .context("sync zoom preference directory")?;
            Ok(())
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&temporary);
        }
        result
    }

    fn read_document(&self) -> Result<Document> {
        let mut options = OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            // Do not follow state-file symlinks or block on special files.
            options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
        }
        let file = match options.open(&self.path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Document {
                    version: VERSION,
                    entries: Vec::new(),
                });
            }
            Err(error) => return Err(error).context("open zoom preferences"),
        };
        ensure!(
            file.metadata()?.is_file(),
            "zoom preferences must be a regular file"
        );
        let mut bytes = Vec::new();
        file.take(MAX_BYTES + 1)
            .read_to_end(&mut bytes)
            .context("read zoom preferences")?;
        ensure!(
            u64::try_from(bytes.len())? <= MAX_BYTES,
            "zoom preferences exceed byte limit"
        );
        let document: Document =
            serde_json::from_slice(&bytes).context("corrupt zoom preferences")?;
        ensure!(
            document.version == 1 || document.version == VERSION,
            "unsupported zoom preference version"
        );
        ensure!(
            document.entries.len() <= MAX_ENTRIES,
            "too many zoom preference entries"
        );
        let mut keys = BTreeSet::new();
        for entry in &document.entries {
            ensure!(
                entry.key.len() == 64 && entry.key.bytes().all(|byte| byte.is_ascii_hexdigit()),
                "invalid zoom preference identity"
            );
            ensure!(
                keys.insert(&entry.key),
                "duplicate zoom preference identity"
            );
            entry.preferences.validate()?;
            ensure!(
                entry.panes.len() <= MAX_PANE_ENTRIES,
                "too many pane zoom preferences"
            );
            ensure!(
                document.version != 1 || entry.panes.is_empty(),
                "pane preferences require version two"
            );
            let mut panes = BTreeSet::new();
            for pane in &entry.panes {
                ensure!(
                    pane.key.len() == 64 && pane.key.bytes().all(|byte| byte.is_ascii_hexdigit()),
                    "invalid pane zoom preference identity"
                );
                ensure!(
                    panes.insert(&pane.key),
                    "duplicate pane zoom preference identity"
                );
                pane.preferences.validate()?;
            }
        }
        Ok(document)
    }
}

/// Sync each newly created directory's parent entry as well as the eventual
/// state file; otherwise a successful first save could lose its entire directory
/// after a power failure despite syncing the file and its immediate parent.
fn create_directory_durably(path: &Path) -> Result<()> {
    let mut missing = Vec::new();
    for ancestor in path.ancestors() {
        match std::fs::metadata(ancestor) {
            Ok(metadata) => {
                ensure!(
                    metadata.is_dir(),
                    "zoom preference ancestor is not a directory"
                );
                break;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => missing.push(ancestor),
            Err(error) => return Err(error).context("inspect zoom preference directory"),
        }
    }
    for directory in missing.into_iter().rev() {
        let mut builder = std::fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt as _;
            builder.mode(0o700);
        }
        match builder.create(directory) {
            Ok(()) => {}
            Err(error)
                if error.kind() == std::io::ErrorKind::AlreadyExists && directory.is_dir() => {}
            Err(error) => return Err(error).context("create zoom preference directory"),
        }
        File::open(directory)?
            .sync_all()
            .context("sync new zoom preference directory")?;
        if let Some(parent) = directory.parent() {
            File::open(parent)?
                .sync_all()
                .context("sync zoom preference ancestor")?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::configuration::{ConfigOptions, SourcePolicy};
    use crate::runtime_paths::BtermPaths;

    fn configuration(root: &Path) -> ResolvedConfiguration {
        ConfigOptions {
            policy: SourcePolicy::Explicit,
            ..Default::default()
        }
        .capture_selected(
            BtermPaths::from_root(root.to_path_buf()).unwrap(),
            root.to_path_buf(),
        )
        .unwrap()
        .resolve()
        .unwrap()
    }

    #[test]
    fn isolated_read_is_side_effect_free_and_save_roundtrips() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("sandbox");
        let store = PreferenceStore::for_configuration(&configuration(&root), "runtime-a").unwrap();
        assert!(store.path().starts_with(root.join("state")));
        assert_eq!(store.load().unwrap(), None);
        assert!(!root.exists());
        let expected = ZoomPreferences {
            target_scale: 1.2345,
        };
        store.save(expected).unwrap();
        assert_eq!(store.load().unwrap(), Some(expected));
        assert!(!store.path.with_extension("tmp").exists());
        let other = PreferenceStore::for_configuration(&configuration(&root), "runtime-b").unwrap();
        assert_eq!(other.load().unwrap(), None);
    }

    #[test]
    fn identity_ignores_mutable_native_content_but_includes_source_selection() {
        let temp = tempfile::tempdir().unwrap();
        let mut config = configuration(temp.path());
        let original = PreferenceStore::for_configuration(&config, "runtime").unwrap();
        config.native.typography.size = 17.5;
        config.native.window.decorations = false;
        config.generation = "different".into();
        assert_eq!(
            original.key,
            PreferenceStore::for_configuration(&config, "runtime")
                .unwrap()
                .key
        );
        config.inputs.options.forced_profile = Some("other".into());
        assert_ne!(
            original.key,
            PreferenceStore::for_configuration(&config, "runtime")
                .unwrap()
                .key
        );
        config.inputs.options.forced_profile = None;
        config
            .inputs
            .bterm
            .documents
            .push(bmux_config::ConfigDocument {
                source: bmux_config::ConfigSource {
                    path: temp.path().join("other.toml"),
                    required: true,
                },
                value: None,
                local: false,
                inline_origin: None,
            });
        assert_ne!(
            original.key,
            PreferenceStore::for_configuration(&config, "runtime")
                .unwrap()
                .key
        );
        let selected = PreferenceStore::for_configuration(&config, "runtime").unwrap();
        config.inputs.bterm.documents[0].value =
            Some(toml::from_str("[bterm.typography]\nsize = 18.5").unwrap());
        assert_eq!(
            selected.key,
            PreferenceStore::for_configuration(&config, "runtime")
                .unwrap()
                .key
        );
    }

    #[test]
    fn corruption_and_unsupported_versions_are_not_overwritten() {
        let temp = tempfile::tempdir().unwrap();
        let store =
            PreferenceStore::for_configuration(&configuration(temp.path()), "runtime").unwrap();
        std::fs::create_dir_all(store.path.parent().unwrap()).unwrap();
        for bytes in [
            b"bad".as_slice(),
            br#"{"version":3,"entries":[]}"#.as_slice(),
            br#"{"version":1,"entries":[],"extra":true}"#.as_slice(),
        ] {
            std::fs::write(store.path(), bytes).unwrap();
            assert!(store.load().is_err());
            assert!(store.save(ZoomPreferences { target_scale: 1.0 }).is_err());
            assert_eq!(std::fs::read(store.path()).unwrap(), bytes);
        }
        std::fs::write(
            store.path(),
            vec![b' '; usize::try_from(MAX_BYTES + 1).unwrap()],
        )
        .unwrap();
        assert!(store.load().is_err());
    }

    #[test]
    fn retention_and_values_are_bounded() {
        let temp = tempfile::tempdir().unwrap();
        let config = configuration(temp.path());
        for index in 0..=MAX_ENTRIES {
            PreferenceStore::for_configuration(&config, &format!("runtime-{index}"))
                .unwrap()
                .save(ZoomPreferences { target_scale: 1.25 })
                .unwrap();
        }
        let first = PreferenceStore::for_configuration(&config, "runtime-0").unwrap();
        assert_eq!(first.load().unwrap(), None);
        assert_eq!(first.read_document().unwrap().entries.len(), MAX_ENTRIES);
        for scale in [f64::NAN, f64::INFINITY, 0.0, -1.0] {
            assert!(
                first
                    .save(ZoomPreferences {
                        target_scale: scale
                    })
                    .is_err()
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn sandbox_state_does_not_follow_file_symlinks() {
        use std::os::unix::fs::symlink;
        let temp = tempfile::tempdir().unwrap();
        let store = PreferenceStore::for_configuration(
            &configuration(&temp.path().join("sandbox")),
            "../external-target",
        )
        .unwrap();
        std::fs::create_dir_all(store.path.parent().unwrap()).unwrap();
        let outside = temp.path().join("outside.json");
        std::fs::write(&outside, b"outside").unwrap();
        symlink(&outside, store.path()).unwrap();
        assert!(store.load().is_err());
        assert!(store.save(ZoomPreferences { target_scale: 1.5 }).is_err());
        assert_eq!(std::fs::read(&outside).unwrap(), b"outside");
        std::fs::remove_file(store.path()).unwrap();
        std::fs::remove_file(store.path.with_extension("lock")).unwrap();
        symlink(&outside, store.path.with_extension("lock")).unwrap();
        assert!(store.save(ZoomPreferences { target_scale: 1.5 }).is_err());
        assert_eq!(std::fs::read(&outside).unwrap(), b"outside");
    }

    #[test]
    fn window_and_pane_updates_preserve_each_other_and_migrate_legacy() {
        use crate::pane_zoom::PaneViewBinding;
        let temp = tempfile::tempdir().unwrap();
        let store =
            PreferenceStore::for_configuration(&configuration(temp.path()), "runtime").unwrap();
        std::fs::create_dir_all(store.path.parent().unwrap()).unwrap();
        let legacy = serde_json::json!({
            "version": 1,
            "entries": [{ "key": store.key, "preferences": { "target_scale": 1.25 } }]
        });
        std::fs::write(store.path(), serde_json::to_vec(&legacy).unwrap()).unwrap();
        assert_eq!(store.load().unwrap().unwrap().target_scale, 1.25);
        let binding = PaneViewBinding::new("verified/view-a", "authority/pane").unwrap();
        let token = binding.verify_restore(&binding).unwrap();
        assert!(store.load_pane(&token).unwrap().is_none());
        store
            .save_pane(&token, ZoomPreferences { target_scale: 2.0 })
            .unwrap();
        assert_eq!(store.load().unwrap().unwrap().target_scale, 1.25);
        store.save(ZoomPreferences { target_scale: 1.75 }).unwrap();
        assert_eq!(store.load_pane(&token).unwrap().unwrap().target_scale, 2.0);
        let second = PaneViewBinding::new("verified/view-b", "authority/pane").unwrap();
        let second_token = second.verify_restore(&second).unwrap();
        assert!(store.load_pane(&second_token).unwrap().is_none());
        store
            .save_pane(&second_token, ZoomPreferences { target_scale: 0.75 })
            .unwrap();
        assert_eq!(store.load().unwrap().unwrap().target_scale, 1.75);
        assert_eq!(store.load_pane(&token).unwrap().unwrap().target_scale, 2.0);
        assert_eq!(store.read_document().unwrap().version, VERSION);
        let other =
            PreferenceStore::for_configuration(&configuration(temp.path()), "other-runtime")
                .unwrap();
        assert!(other.load_pane(&token).unwrap().is_none());
    }

    #[test]
    fn pane_preference_retention_is_bounded_and_corruption_is_not_overwritten() {
        use crate::pane_zoom::PaneViewBinding;
        let temp = tempfile::tempdir().unwrap();
        let store =
            PreferenceStore::for_configuration(&configuration(temp.path()), "runtime").unwrap();
        let token = |index| {
            let binding =
                PaneViewBinding::new(&format!("verified/view-{index}"), "authority/pane").unwrap();
            binding.verify_restore(&binding).unwrap()
        };
        for index in 0..=MAX_PANE_ENTRIES {
            store
                .save_pane(&token(index), ZoomPreferences { target_scale: 1.5 })
                .unwrap();
        }
        assert!(store.load_pane(&token(0)).unwrap().is_none());
        assert!(store.load_pane(&token(MAX_PANE_ENTRIES)).unwrap().is_some());
        let mut document = store.read_document().unwrap();
        document.entries[0].panes[0].preferences.target_scale = -1.0;
        let corrupt = serde_json::to_vec(&document).unwrap();
        std::fs::write(store.path(), &corrupt).unwrap();
        assert!(store.load_pane(&token(1)).is_err());
        assert!(store.save(ZoomPreferences { target_scale: 2.0 }).is_err());
        assert_eq!(std::fs::read(store.path()).unwrap(), corrupt);
    }

    #[test]
    fn malformed_entry_shapes_are_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let store =
            PreferenceStore::for_configuration(&configuration(temp.path()), "runtime").unwrap();
        std::fs::create_dir_all(store.path.parent().unwrap()).unwrap();
        let entry = || Entry {
            key: store.key.clone(),
            preferences: ZoomPreferences { target_scale: 1.0 },
            panes: Vec::new(),
        };
        let mut invalid_scale = entry();
        invalid_scale.preferences.target_scale = 0.0;
        let mut invalid_key = entry();
        invalid_key.key = "bad".into();
        for entries in [
            vec![entry(), entry()],
            vec![invalid_scale],
            vec![invalid_key],
            (0..=MAX_ENTRIES).map(|_| entry()).collect(),
        ] {
            std::fs::write(
                store.path(),
                serde_json::to_vec(&Document {
                    version: VERSION,
                    entries,
                })
                .unwrap(),
            )
            .unwrap();
            assert!(store.load().is_err());
        }
    }

    #[test]
    fn interrupted_scratch_is_repaired_and_lock_contention_is_explicit() {
        let temp = tempfile::tempdir().unwrap();
        let store =
            PreferenceStore::for_configuration(&configuration(temp.path()), "runtime").unwrap();
        std::fs::create_dir_all(store.path.parent().unwrap()).unwrap();
        std::fs::write(store.path.with_extension("tmp"), b"interrupted").unwrap();
        store.save(ZoomPreferences { target_scale: 1.5 }).unwrap();
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .open(store.path.with_extension("lock"))
            .unwrap();
        lock.try_lock().unwrap();
        assert!(store.save(ZoomPreferences { target_scale: 2.0 }).is_err());
        assert_eq!(
            store.load().unwrap(),
            Some(ZoomPreferences { target_scale: 1.5 })
        );
    }
}
