//! Active-slot resolution for bmux-cli.
//!
//! Precedence:
//! 1. `BMUX_SLOT_NAME` env var.
//! 2. argv[0] basename parsed as `bmux-<slot>`.
//!
//! When neither is present, legacy single-install behavior is used and
//! callers should fall back to `ConfigPaths::default()`.
//!
//! Resolution is performed once at process startup and cached. Callers
//! obtain the resolved state via [`active_slot`], which also performs the
//! manifest load on first call. Failures are non-fatal: if the manifest
//! lookup fails for a referenced slot, we surface it to the caller.

use std::sync::OnceLock;

use bmux_slots::{SLOT_NAME_ENV, Slot, SlotManifest};

/// Outcome of active-slot resolution.
#[derive(Debug, Clone)]
#[allow(dead_code)] // Fields are consumed by slot subcommands landing in a follow-up.
pub enum ActiveSlotState {
    /// Neither argv[0] nor `BMUX_SLOT_NAME` indicates a slot. Legacy single-
    /// install behavior applies.
    None,

    /// A slot name was resolved and successfully looked up in the manifest.
    Resolved {
        /// Fully-resolved slot.
        slot: Box<Slot>,
        /// How the name was discovered (for diagnostics).
        source: SlotNameSource,
        /// The loaded manifest (for presentational default lookup, etc).
        manifest: Box<SlotManifest>,
    },

    /// A slot name was discovered but it is not in the manifest. The
    /// CLI should surface a clear error to the user.
    Unknown {
        /// The name that was discovered.
        name: String,
        /// How it was discovered.
        source: SlotNameSource,
        /// Names known by the manifest, for remediation.
        known: Vec<String>,
    },

    /// Manifest loading failed. Legacy behavior applies as a fallback;
    /// the error is surfaced for diagnostics.
    ManifestError {
        /// Stringified error.
        message: String,
    },
}

/// How the slot name was discovered.
#[derive(Debug, Clone, Copy)]
pub enum SlotNameSource {
    /// From `BMUX_SLOT_NAME` env var.
    Env,
    /// From argv[0] basename (`bmux-<slot>`).
    Argv0,
}

fn compute_active_slot() -> ActiveSlotState {
    let Some((name, source)) = discover_slot_name() else {
        return ActiveSlotState::None;
    };

    match SlotManifest::load_default() {
        Ok(manifest) => match manifest.get(&name) {
            Ok(slot) => ActiveSlotState::Resolved {
                slot: Box::new(slot.clone()),
                source,
                manifest: Box::new(manifest),
            },
            Err(_) => ActiveSlotState::Unknown {
                name,
                source,
                known: manifest.slots.keys().cloned().collect(),
            },
        },
        Err(e) => ActiveSlotState::ManifestError {
            message: format!("{e}"),
        },
    }
}

fn discover_slot_name() -> Option<(String, SlotNameSource)> {
    if let Ok(v) = std::env::var(SLOT_NAME_ENV) {
        let trimmed = v.trim();
        if !trimmed.is_empty() {
            return Some((trimmed.to_string(), SlotNameSource::Env));
        }
    }
    if let Some(argv0) = std::env::args_os().next()
        && let Some(basename) = std::path::Path::new(&argv0).file_name()
    {
        let s = basename.to_string_lossy();
        // Accept forms like `bmux-<slot>` but not bare `bmux`.
        if let Some(rest) = s.strip_prefix("bmux-")
            && !rest.is_empty()
            // Defensive: argv[0] could be "bmux-env" etc. which are not
            // slot binaries. Slot names validate against [A-Za-z0-9._-]+.
            && bmux_slots::validate_slot_name(rest).is_ok()
        {
            return Some((rest.to_string(), SlotNameSource::Argv0));
        }
    }
    None
}

/// Return the cached active-slot state, computing it on first call.
pub fn active_slot() -> &'static ActiveSlotState {
    static STATE: OnceLock<ActiveSlotState> = OnceLock::new();
    STATE.get_or_init(compute_active_slot)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn active_slot_state_is_sendable() {
        fn assert_send<T: Send + Sync>() {}
        assert_send::<ActiveSlotState>();
    }
}
