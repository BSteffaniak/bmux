//! bmux decoration plugin.
//!
//! Owns pane visual styling: borders, focus highlighting, and any other
//! chrome painted around pane content. Reads pane lifecycle state from
//! the windows plugin via [`bmux_windows_plugin_api::windows_events`]
//! and exposes its own typed API via [`bmux_decoration_plugin_api`].
//!
//! This initial revision provides the storage + typed API surface. The
//! actual decoration *painting* lives in core's renderer today (matching
//! the current ASCII `+ - |` border); a follow-up will migrate painting
//! into this plugin via a scene-layout protocol.

#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic)]
#![allow(clippy::module_name_repetitions)]

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Mutex;

use bmux_decoration_plugin_api::decoration_state::{
    BorderStyle, DecorationEvent, DecorationState, PaneDecoration, SetStyleError,
};
use uuid::Uuid;

/// In-memory state store.
#[derive(Debug)]
struct State {
    /// Per-pane overrides. Panes without an override fall through to
    /// [`State::default_border`].
    panes: HashMap<Uuid, PaneDecoration>,
    /// Global default, used for any pane without a specific override.
    default_border: BorderStyle,
}

impl Default for State {
    fn default() -> Self {
        Self {
            panes: HashMap::new(),
            default_border: default_border_style(),
        }
    }
}

/// Matches the current core renderer's output (ASCII `+ - |`). Chosen
/// explicitly rather than implicitly so changing the default is a
/// single-line visible change.
const fn default_border_style() -> BorderStyle {
    BorderStyle::Ascii
}

/// The decoration plugin's concrete implementation. Thread-safe via an
/// inner [`Mutex`]. Cloning the handle is a cheap `Arc` clone.
#[derive(Default)]
pub struct DecorationPlugin {
    state: Mutex<State>,
}

impl DecorationPlugin {
    /// Construct a fresh decoration plugin with the built-in ASCII default.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Borrow-internal helper: resolve a pane's effective decoration,
    /// pulling the global default if no override exists.
    fn resolve(&self, pane_id: Uuid) -> Option<PaneDecoration> {
        let state = self.state.lock().ok()?;
        if let Some(p) = state.panes.get(&pane_id) {
            return Some(p.clone());
        }
        Some(PaneDecoration {
            pane_id,
            border: state.default_border,
            focused: false,
        })
    }
}

impl DecorationState for DecorationPlugin {
    fn pane_decoration<'a>(
        &'a self,
        pane_id: Uuid,
    ) -> Pin<Box<dyn Future<Output = Option<PaneDecoration>> + Send + 'a>> {
        Box::pin(async move { self.resolve(pane_id) })
    }

    fn default_border_style<'a>(
        &'a self,
    ) -> Pin<Box<dyn Future<Output = BorderStyle> + Send + 'a>> {
        Box::pin(async move {
            self.state
                .lock()
                .map_or_else(|_| default_border_style(), |s| s.default_border)
        })
    }

    fn set_pane_border<'a>(
        &'a self,
        pane_id: Uuid,
        border: BorderStyle,
    ) -> Pin<Box<dyn Future<Output = Result<(), SetStyleError>> + Send + 'a>> {
        Box::pin(async move {
            let mut state = self
                .state
                .lock()
                .map_err(|_| SetStyleError::StyleUnsupported {
                    style: "<poisoned>".into(),
                })?;
            let entry = state
                .panes
                .entry(pane_id)
                .or_insert_with(|| PaneDecoration {
                    pane_id,
                    border,
                    focused: false,
                });
            entry.border = border;
            Ok(())
        })
    }

    fn set_default_border<'a>(
        &'a self,
        border: BorderStyle,
    ) -> Pin<Box<dyn Future<Output = Result<(), SetStyleError>> + Send + 'a>> {
        Box::pin(async move {
            let mut state = self
                .state
                .lock()
                .map_err(|_| SetStyleError::StyleUnsupported {
                    style: "<poisoned>".into(),
                })?;
            state.default_border = border;
            Ok(())
        })
    }
}

/// Re-export the [`BorderStyle`], [`PaneDecoration`], [`DecorationEvent`],
/// and [`SetStyleError`] types for callers that want them without
/// importing from the API crate separately.
pub use bmux_decoration_plugin_api::decoration_state;

/// Marker function used by tests to verify event-stream types round-trip.
#[must_use]
pub fn sample_event_for_pane(pane_id: Uuid) -> DecorationEvent {
    DecorationEvent::PaneRestyled { pane_id }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn block_on<F: Future>(fut: F) -> F::Output {
        // Tiny single-threaded executor for tests to avoid pulling a
        // runtime into this crate. Spinning is fine for the trivial
        // futures this module returns.
        use std::sync::Arc;
        use std::task::{Context, Poll, Wake, Waker};
        struct Noop;
        impl Wake for Noop {
            fn wake(self: Arc<Self>) {}
        }
        let waker = Waker::from(Arc::new(Noop));
        let mut cx = Context::from_waker(&waker);
        let mut pinned = Box::pin(fut);
        loop {
            match pinned.as_mut().poll(&mut cx) {
                Poll::Ready(v) => return v,
                Poll::Pending => {}
            }
        }
    }

    #[test]
    fn new_plugin_has_ascii_default_border() {
        let plugin = DecorationPlugin::new();
        let style = block_on(plugin.default_border_style());
        assert_eq!(style, BorderStyle::Ascii);
    }

    #[test]
    fn query_unknown_pane_returns_default_style() {
        let plugin = DecorationPlugin::new();
        let decoration = block_on(plugin.pane_decoration(Uuid::nil()))
            .expect("default decoration always present");
        assert_eq!(decoration.border, BorderStyle::Ascii);
        assert_eq!(decoration.pane_id, Uuid::nil());
    }

    #[test]
    fn set_pane_border_persists_override() {
        let plugin = DecorationPlugin::new();
        let pane = Uuid::from_u128(7);
        let res = block_on(plugin.set_pane_border(pane, BorderStyle::Double));
        assert!(res.is_ok());
        let decoration = block_on(plugin.pane_decoration(pane)).unwrap();
        assert_eq!(decoration.border, BorderStyle::Double);
    }

    #[test]
    fn set_default_border_changes_global_default() {
        let plugin = DecorationPlugin::new();
        let res = block_on(plugin.set_default_border(BorderStyle::None));
        assert!(res.is_ok());
        let default = block_on(plugin.default_border_style());
        assert_eq!(default, BorderStyle::None);
    }

    #[test]
    fn sample_event_constructs_tagged_variant() {
        let ev = sample_event_for_pane(Uuid::from_u128(1));
        if let DecorationEvent::PaneRestyled { pane_id } = ev {
            assert_eq!(pane_id, Uuid::from_u128(1));
        } else {
            panic!("expected pane_restyled variant");
        }
    }
}
