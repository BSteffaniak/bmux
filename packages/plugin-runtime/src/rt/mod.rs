//! plug-rt: the author async facade, served by the host's single runtime.
//!
//! Plugin code spawns and sleeps through this module and never builds a
//! runtime. Author source is identical in both packaging modes:
//!
//! - **Bundled** (the default build): thin Tokio re-exports. Tasks spawned
//!   while the host polls plugin code join the activation's
//!   [`BackgroundTasks`](crate::background::BackgroundTasks) scope.
//! - **Dynamic** (`--cfg bmux_plugin_dylib`, set by the plugin packager in its
//!   own target directory and never a Cargo feature, so a workspace build can
//!   never unify it into the host): every call goes through the host function
//!   table ([`abi::PlugRtV1`]) the host installed into the library. Plugin
//!   futures cross as `{data, poll, drop}`, timers are host timers by id, and
//!   real wakers flow both ways ([`abi::RtCx`]).
//!
//! `tokio::sync`, `select!`, `join!` and other runtime-agnostic primitives are
//! used directly in both modes. Net and process IO are not proxied yet: a
//! plugin that needs them declares itself bundled-only, and the dynamic loader
//! refuses it with a typed error instead of letting it panic at runtime.
//! Building a runtime, `block_in_place` and nested `block_on` are forbidden in
//! plugin code.

pub mod abi;
pub mod guest;
#[cfg(not(bmux_plugin_dylib))]
pub mod host;
pub(crate) mod scope;
mod task;
pub mod time;
mod waker;

pub use scope::{ScopeGuard, enter_instance};
pub use task::{
    JoinError, JoinHandle, spawn, spawn_blocking, spawn_blocking_named, spawn_named, yield_now,
};

/// Whether the host runtime is reachable from the calling thread: inside a
/// runtime context (bundled) or with an installed host table (dynamic). When
/// it is not, [`spawn`] returns a handle that fails with an unavailable error.
#[must_use]
pub fn runtime_available() -> bool {
    #[cfg(bmux_plugin_dylib)]
    {
        guest::installed().is_some()
    }
    #[cfg(not(bmux_plugin_dylib))]
    {
        tokio::runtime::Handle::try_current().is_ok()
    }
}

/// Whether this build of the plugin runtime routes plug-rt through a host
/// table (a dynamic plugin build) rather than Tokio directly.
#[must_use]
pub const fn is_dynamic_build() -> bool {
    cfg!(bmux_plugin_dylib)
}

#[cfg(test)]
mod tests;
