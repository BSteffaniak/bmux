//! Test-support scaffolding for plugin-level unit tests.
//!
//! Plugin unit tests frequently construct a
//! [`bmux_plugin_sdk::NativeServiceContext`] directly and invoke the
//! plugin's handlers, bypassing the real plugin-loader/registry. That
//! bypass leaves cross-plugin `call_service_raw` dispatch unrouted —
//! there's no `ProviderId::Plugin(...)` in the context's `services`
//! list, and production `handle_core_service_call` only speaks to
//! core interfaces like `storage-query/v1`.
//!
//! [`install_test_service_router`] lets a test install a thread-local
//! closure that intercepts every `call_service_raw` call on the
//! current thread. Production code never installs one, so there is no
//! runtime cost outside tests.
//!
//! # Usage
//!
//! ```no_run
//! use bmux_plugin::test_support::{install_test_service_router, TestServiceRouter};
//! use bmux_plugin_sdk::{PluginError, Result, ServiceKind};
//!
//! let router: TestServiceRouter = std::sync::Arc::new(
//!     |_caller_plugin, _caller_client, _capability, _kind, interface, operation, _payload| {
//!         match (interface, operation) {
//!             ("contexts-state", "list-contexts") => Ok(b"<serialized payload>".to_vec()),
//!             _ => Err(PluginError::UnsupportedHostOperation {
//!                 operation: "test_router",
//!             }),
//!         }
//!     },
//! );
//! let _guard = install_test_service_router(router);
//! // ... run the plugin handler here ...
//! // guard drops at end of scope; router uninstalled.
//! ```

use bmux_plugin_sdk::{Result, ServiceKind};
use std::cell::RefCell;
use std::sync::Arc;

/// Closure signature for the test service router.
///
/// Receives the same information that `call_service_raw` would pass
/// into its capability/service-lookup logic. The closure returns the
/// raw byte payload the caller expects (same shape as
/// `encode_service_message` output), or a [`PluginError`] on failure.
///
/// Parameter order:
/// 1. `caller_plugin_id` — which plugin made the call.
/// 2. `caller_client_id` — the end-user client id, when known.
/// 3. `capability` — the capability string the caller is using.
/// 4. `kind` — Query / Command / Event.
/// 5. `interface_id` — the target interface id.
/// 6. `operation` — the operation name within the interface.
/// 7. `payload` — serialized request bytes.
#[allow(clippy::type_complexity)]
pub type TestServiceRouter = Arc<
    dyn Fn(&str, Option<uuid::Uuid>, &str, ServiceKind, &str, &str, Vec<u8>) -> Result<Vec<u8>>
        + Send
        + Sync,
>;

thread_local! {
    static CURRENT_ROUTER: RefCell<Option<TestServiceRouter>> = const { RefCell::new(None) };
}

/// RAII guard that uninstalls the thread-local test service router
/// when dropped.
///
/// Always scope the guard to the lifetime of a single test. Never
/// `std::mem::forget` the guard — doing so permanently installs the
/// router on the thread and will leak behavior into subsequent tests.
pub struct TestServiceRouterGuard {
    previous: Option<TestServiceRouter>,
}

impl Drop for TestServiceRouterGuard {
    fn drop(&mut self) {
        CURRENT_ROUTER.with(|slot| {
            *slot.borrow_mut() = self.previous.take();
        });
    }
}

/// Install a thread-local test service router for the duration of
/// the returned guard's lifetime.
///
/// Replaces any previously-installed router; the previous one is
/// restored when the guard drops.
pub fn install_test_service_router(router: TestServiceRouter) -> TestServiceRouterGuard {
    let previous = CURRENT_ROUTER.with(|slot| slot.replace(Some(router)));
    TestServiceRouterGuard { previous }
}

/// Retrieve the currently-installed test service router, if any.
///
/// Used internally by `call_service_raw`; plugin authors normally
/// don't need to call this directly.
#[must_use]
pub fn test_service_router() -> Option<TestServiceRouter> {
    CURRENT_ROUTER.with(|slot| slot.borrow().clone())
}
