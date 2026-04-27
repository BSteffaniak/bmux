//! Neutral primitive crate for pane-runtime state.
//!
//! Hosts the pure-data types that describe pane runtime (layout tree,
//! pane identity records, floating surfaces, resurrection metadata)
//! plus the trait abstractions + handle newtypes that let core code
//! (`packages/server`) consume pane runtime owned by the
//! `bmux.pane_runtime` plugin without depending on the plugin impl
//! crate or on `portable-pty`/tokio/vt100.
//!
//! ## Layout
//!
//! - `layout`: layout tree types (`PaneLayoutNode`, split direction,
//!   rect, floating surface).
//! - `meta`: per-pane identity + launch + resurrection records.
//! - `attach`: `AttachViewport` record shared between server's attach
//!   path and the plugin's layout math.
//! - `output`: `PaneOutputReader` trait + `OutputRead` record +
//!   handle newtype.
//! - `error`: `SessionRuntimeError`.

#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic)]
#![allow(clippy::module_name_repetitions)]

pub mod attach;
pub mod error;
pub mod layout;
pub mod manager_handle;
pub mod meta;
pub mod output;

pub use attach::AttachViewport;
pub use error::SessionRuntimeError;
pub use layout::{
    FloatingSurfaceRuntime, LayoutRect, PaneLayoutNode, PaneResizeDirection, contains_pane,
};
pub use manager_handle::{
    AttachLayoutState, AttachPaneSnapshotState, AttachSnapshotState, NoopSessionRuntimeManager,
    RemovedRuntimeInfo, SessionRuntimeManagerApi, SessionRuntimeManagerHandle,
    SessionRuntimeSnapshot,
};
pub use meta::{PaneCommandSource, PaneLaunchSpec, PaneResurrectionSnapshot, PaneRuntimeMeta};
pub use output::{OutputRead, PaneOutputReader, PaneOutputReaderHandle};
