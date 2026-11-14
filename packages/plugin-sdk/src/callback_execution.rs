//! Explicit selected callback execution contracts, not an executor or work ledger.
//!
//! Static hosts and negotiated native adapters supply actual authority and a
//! bounded blocking executor. This module discovers no runtime and creates no
//! cancellation state. Rust closures never form a portable ABI representation.

use crate::definition::{PluginFuture, PluginResult};
use bmux_plugin_runtime::PluginInstanceId;
use std::{any::Any, fmt, sync::Arc};

/// One author callback, transferred exactly once to a selected executor.
pub type PluginBlockingCallback = Box<dyn FnOnce() -> PluginResult<()> + Send + 'static>;

/// Actual callback settlement, not merely acceptance into a worker queue.
pub type PluginCallbackSettlement = PluginFuture<'static, PluginResult<()>>;

/// Host-authenticated invocation authority and wake-driven cancellation source.
///
/// Implementations bind actual admission and immutable authenticated ancestors,
/// excluding the current callback entry. Target IDs are not grants. No caller
/// string, thread identity or serialized ancestor list establishes this authority.
pub trait PluginCallbackAuthoritySource: Send + Sync {
    /// Check current admission, cancellation and fencing authority.
    ///
    /// # Errors
    /// Rejects closed, cancelled, stale or otherwise unavailable authority.
    fn check_live(&self) -> PluginResult<()>;

    /// Atomically validate and retain an actual callback dependency.
    ///
    /// Independent contention must wait at the adapter gate, not be classified
    /// as recursion. The returned guard spans author execution and unwinding.
    ///
    /// # Errors
    /// Rejects recursive ancestor entry, foreign targets and unavailable authority.
    fn enter_adapter_entry(
        &self,
        instance: PluginInstanceId,
    ) -> PluginResult<PluginCallbackEntryGuard>;

    /// Observe existing host cancellation/closure without polling timers.
    ///
    /// # Errors
    /// Observation failure is explicit, never interpreted as live authority.
    fn cancelled(&self) -> PluginCallbackSettlement;
}

/// Non-cloneable owned host dependency retained through callback settlement.
///
/// Dropping this guard releases that dependency; it does not acknowledge an
/// invocation, cancel work, unload an image or claim successful cleanup.
#[must_use = "retain the entry guard through author callback execution"]
pub struct PluginCallbackEntryGuard {
    _owner: Box<dyn Any + Send>,
    binding: Option<(Arc<dyn PluginCallbackAuthoritySource>, PluginInstanceId)>,
}

impl PluginCallbackEntryGuard {
    /// Wrap an actual host-owned dependency, never a payload-provided identity.
    pub fn from_host<T: Send + 'static>(guard: T) -> Self {
        Self {
            _owner: Box::new(guard),
            binding: None,
        }
    }
    /// Validate this exact privately stamped dependency without entering again.
    ///
    /// # Errors
    /// Rejects raw guards, another authority allocation, a different target, or
    /// closed/cancelled authority. The instance alone never grants execution.
    pub fn validate_for(
        &self,
        authority: &PluginCallbackAuthority,
        instance: PluginInstanceId,
    ) -> PluginResult<()> {
        let Some((source, target)) = &self.binding else {
            return Err(crate::PluginCommandError::unavailable(
                "callback entry guard is not authority-bound",
            ));
        };
        if !Arc::ptr_eq(source, &authority.0) || *target != instance {
            return Err(crate::PluginCommandError::unavailable(
                "callback entry authority or target mismatch",
            ));
        }
        authority.check_live()
    }
}

impl fmt::Debug for PluginCallbackEntryGuard {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("PluginCallbackEntryGuard(bound)")
    }
}

/// Opaque host-issued authority. There is no constructor from an instance ID.
#[derive(Clone)]
pub struct PluginCallbackAuthority(Arc<dyn PluginCallbackAuthoritySource>);

impl PluginCallbackAuthority {
    /// Trusted host/negotiated adapter construction only.
    #[must_use]
    pub fn from_host(source: Arc<dyn PluginCallbackAuthoritySource>) -> Self {
        Self(source)
    }

    /// # Errors
    /// Preserves the host's cancellation, fencing and admission failures.
    pub fn check_live(&self) -> PluginResult<()> {
        self.0.check_live()
    }

    /// # Errors
    /// Preserves authoritative entry rejection without a check-only fallback.
    pub fn enter_adapter_entry(
        &self,
        instance: PluginInstanceId,
    ) -> PluginResult<PluginCallbackEntryGuard> {
        let mut entry = self.0.enter_adapter_entry(instance)?;
        // Only this wrapper stamps source identity after successful host entry.
        // Reject source reuse of an already stamped guard rather than rebinding it.
        if entry.binding.is_some() {
            return Err(crate::PluginCommandError::unavailable(
                "callback source returned an already bound entry",
            ));
        }
        entry.binding = Some((self.0.clone(), instance));
        Ok(entry)
    }

    /// Observe the existing host wake source lazily.
    ///
    /// # Errors
    /// Preserves observation failure; does not create another cancellation ledger.
    pub fn cancelled(&self) -> PluginCallbackSettlement {
        let authority = self.clone();
        Box::pin(async move { authority.0.cancelled().await })
    }
}

impl fmt::Debug for PluginCallbackAuthority {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("PluginCallbackAuthority(bound)")
    }
}

/// Selected bounded blocking execution, supplied explicitly by the host.
///
/// Returned futures must be lazy. Before acceptance, cancellation/drop removes
/// queued work without author entry. After acceptance, the job owns admission,
/// implementation/image, adapter permit and dependency guard until settlement,
/// even when its response observer disappears. Queue capacity is bounded.
///
/// For ordinary `execute`, immediately before author execution the executor validates that authority
/// belongs to its selected scope and calls `enter_adapter_entry`. It retains the
/// returned guard across callback return and panic unwinding. No author code may
/// execute while constructing or polling the future; execution occurs only on
/// the selected blocking worker. Panics/worker loss are explicit errors with
/// uncertain cleanup retained by the host, never permission to unload or retry.
pub trait PluginCallbackExecution: Send + Sync {
    /// Schedule exactly once and observe actual settlement.
    ///
    /// # Errors
    /// Reports unsupported execution, rejected admission, cancellation, capacity,
    /// callback failure or worker failure. No ambient/synchronous fallback exists.
    fn execute(
        &self,
        authority: PluginCallbackAuthority,
        instance: PluginInstanceId,
        callback: PluginBlockingCallback,
    ) -> PluginCallbackSettlement;
    /// Execute an already entered dependency without a second lease acquisition.
    ///
    /// The executor must validate `entry` before queue acceptance and immediately
    /// before author code, then own it through settlement/unwind/observer loss.
    /// It must project this exact dependency into native entry ports; looking up
    /// another active entry by instance is forbidden. Cancellation before
    /// acceptance drops this guard without invoking the callback.
    ///
    /// # Errors
    /// Default is unsupported, not a fallback to `execute` or another entry.
    fn execute_entered(
        &self,
        _authority: PluginCallbackAuthority,
        _instance: PluginInstanceId,
        _entry: PluginCallbackEntryGuard,
        _callback: PluginBlockingCallback,
    ) -> PluginCallbackSettlement {
        Box::pin(async {
            Err(crate::PluginCommandError::unavailable(
                "entered callback execution unsupported",
            ))
        })
    }
}

/// Opaque selected executor handle; does not allocate a runtime or worker pool.
#[derive(Clone)]
pub struct PluginCallbackExecutor(Arc<dyn PluginCallbackExecution>);

impl PluginCallbackExecutor {
    /// Trusted host/negotiated component-local executor construction only.
    #[must_use]
    pub fn from_host(executor: Arc<dyn PluginCallbackExecution>) -> Self {
        Self(executor)
    }

    /// Delegate lazily to the selected executor, never execute the callback here.
    ///
    /// # Errors
    /// Preserves selected execution failures; no default executor is chosen.
    pub fn execute(
        &self,
        authority: PluginCallbackAuthority,
        instance: PluginInstanceId,
        callback: PluginBlockingCallback,
    ) -> PluginCallbackSettlement {
        let executor = self.clone();
        Box::pin(async move { executor.0.execute(authority, instance, callback).await })
    }
    /// Move a prequeue entry into the selected executor without entering twice.
    ///
    /// # Errors
    /// Rejects invalid entry binding or selected execution failure. The host
    /// executor must revalidate immediately before author execution as well.
    pub fn execute_entered(
        &self,
        authority: PluginCallbackAuthority,
        instance: PluginInstanceId,
        entry: PluginCallbackEntryGuard,
        callback: PluginBlockingCallback,
    ) -> PluginCallbackSettlement {
        let executor = self.clone();
        Box::pin(async move {
            entry.validate_for(&authority, instance)?;
            executor
                .0
                .execute_entered(authority, instance, entry, callback)
                .await
        })
    }
}

impl fmt::Debug for PluginCallbackExecutor {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("PluginCallbackExecutor(bound)")
    }
}

/// One selected executor and its actual opaque authority; never serialized.
#[derive(Debug, Clone)]
pub struct PluginCallbackContext {
    authority: PluginCallbackAuthority,
    executor: PluginCallbackExecutor,
}

impl PluginCallbackContext {
    /// Bind the matching selected authority and executor through trusted wiring.
    /// The executor must independently validate their compatibility before entry.
    #[must_use]
    pub const fn from_host(
        authority: PluginCallbackAuthority,
        executor: PluginCallbackExecutor,
    ) -> Self {
        Self {
            authority,
            executor,
        }
    }

    /// # Errors
    /// Preserves authoritative cancellation, admission and fencing failures.
    pub fn check_live(&self) -> PluginResult<()> {
        self.authority.check_live()
    }

    /// Atomically enter the authenticated adapter dependency before gate queueing.
    ///
    /// Retain the returned guard through queueing and actual callback settlement.
    /// This is a direct authority delegation, not a check-only recursion test or
    /// a new authority derived from the target identity.
    ///
    /// # Errors
    /// Preserves recursive-entry, cancellation, fencing and admission failures.
    pub fn enter_adapter_entry(
        &self,
        instance: PluginInstanceId,
    ) -> PluginResult<PluginCallbackEntryGuard> {
        self.authority.enter_adapter_entry(instance)
    }

    /// # Errors
    /// Preserves host wake-observation failure without guessing liveness.
    pub fn cancelled(&self) -> PluginCallbackSettlement {
        self.authority.cancelled()
    }

    /// Schedule through the selected executor; target identity is not authority.
    ///
    /// # Errors
    /// Preserves selected admission, callback and settlement errors.
    pub fn execute(
        &self,
        instance: PluginInstanceId,
        callback: PluginBlockingCallback,
    ) -> PluginCallbackSettlement {
        self.executor
            .execute(self.authority.clone(), instance, callback)
    }
    /// Submit the exact guard acquired before waiting for the adapter gate.
    ///
    /// # Errors
    /// Rejects raw/foreign/stale guards or selected execution failures. Never
    /// consumes another entry lease and never invokes an inline fallback.
    pub fn execute_entered(
        &self,
        instance: PluginInstanceId,
        entry: PluginCallbackEntryGuard,
        callback: PluginBlockingCallback,
    ) -> PluginCallbackSettlement {
        self.executor
            .execute_entered(self.authority.clone(), instance, entry, callback)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::PluginCommandError;
    use std::{
        future::Future,
        pin::pin,
        sync::atomic::{AtomicUsize, Ordering},
        task::{Context, Poll, Waker},
    };

    struct LiveAuthority {
        live: Arc<std::sync::atomic::AtomicBool>,
        entries: Arc<AtomicUsize>,
        releases: Arc<AtomicUsize>,
    }
    impl PluginCallbackAuthoritySource for LiveAuthority {
        fn check_live(&self) -> PluginResult<()> {
            if self.live.load(Ordering::SeqCst) {
                Ok(())
            } else {
                Err(PluginCommandError::unavailable("closed"))
            }
        }
        fn enter_adapter_entry(
            &self,
            _: PluginInstanceId,
        ) -> PluginResult<PluginCallbackEntryGuard> {
            self.check_live()?;
            self.entries.fetch_add(1, Ordering::SeqCst);
            Ok(PluginCallbackEntryGuard::from_host(Owner(
                self.releases.clone(),
            )))
        }
        fn cancelled(&self) -> PluginCallbackSettlement {
            Box::pin(async { Err(PluginCommandError::unavailable("test wake unavailable")) })
        }
    }

    #[test]
    fn entry_binding_rejects_raw_cross_authority_wrong_target_and_closed_authority() {
        let live = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let source = Arc::new(LiveAuthority {
            live: live.clone(),
            entries: Arc::new(AtomicUsize::new(0)),
            releases: Arc::new(AtomicUsize::new(0)),
        });
        let authority = PluginCallbackAuthority::from_host(source.clone());
        let target = instance();
        let raw = PluginCallbackEntryGuard::from_host(());
        assert!(raw.validate_for(&authority, target).is_err());
        let first = authority.enter_adapter_entry(target).unwrap();
        let second = authority.enter_adapter_entry(target).unwrap();
        assert_eq!(source.entries.load(Ordering::SeqCst), 2);
        let cloned_authority = authority.clone();
        assert!(first.validate_for(&cloned_authority, target).is_ok());
        assert!(second.validate_for(&authority, target).is_ok());
        assert!(first.validate_for(&authority, instance()).is_err());
        let (other, _) = self::authority(false);
        assert!(first.validate_for(&other, target).is_err());
        live.store(false, Ordering::SeqCst);
        assert!(first.validate_for(&authority, target).is_err());
        assert!(authority.enter_adapter_entry(target).is_err());
        assert_eq!(source.entries.load(Ordering::SeqCst), 2);
        drop(first);
        drop(second);
        assert_eq!(source.releases.load(Ordering::SeqCst), 2);
    }

    struct EnteredExecutor(Arc<AtomicUsize>);
    impl PluginCallbackExecution for EnteredExecutor {
        fn execute(
            &self,
            _: PluginCallbackAuthority,
            _: PluginInstanceId,
            _: PluginBlockingCallback,
        ) -> PluginCallbackSettlement {
            panic!("entered execution must not fall back to ordinary execution");
        }
        fn execute_entered(
            &self,
            authority: PluginCallbackAuthority,
            target: PluginInstanceId,
            entry: PluginCallbackEntryGuard,
            _: PluginBlockingCallback,
        ) -> PluginCallbackSettlement {
            let calls = self.0.clone();
            Box::pin(async move {
                entry.validate_for(&authority, target)?;
                calls.fetch_add(1, Ordering::SeqCst);
                Err(PluginCommandError::unavailable(
                    "test executor declines author execution",
                ))
            })
        }
    }

    #[test]
    fn entered_handoff_is_lazy_validates_exact_guard_and_never_enters_twice() {
        let (authority, source) = authority(false);
        let calls = Arc::new(AtomicUsize::new(0));
        let context = PluginCallbackContext::from_host(
            authority,
            PluginCallbackExecutor::from_host(Arc::new(EnteredExecutor(calls.clone()))),
        );
        let target = instance();
        let entry = context.enter_adapter_entry(target).unwrap();
        let future =
            context.execute_entered(target, entry, Box::new(|| panic!("no inline callback")));
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert_eq!(source.releases.load(Ordering::SeqCst), 0);
        let mut future = pin!(future);
        assert!(matches!(
            future
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop())),
            Poll::Ready(Err(_))
        ));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(source.entries.load(Ordering::SeqCst), 1);
        assert_eq!(source.releases.load(Ordering::SeqCst), 1);
        let raw = PluginCallbackEntryGuard::from_host(());
        let mut invalid = pin!(context.execute_entered(
            target,
            raw,
            Box::new(|| panic!("raw entry cannot execute"))
        ));
        assert!(matches!(
            invalid
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop())),
            Poll::Ready(Err(_))
        ));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn unsupported_entered_execution_and_unpolled_drop_do_not_reenter_or_call_author() {
        let (authority, source) = authority(false);
        let calls = Arc::new(AtomicUsize::new(0));
        let context = PluginCallbackContext::from_host(
            authority,
            PluginCallbackExecutor::from_host(Arc::new(RejectExecutor(calls.clone()))),
        );
        let target = instance();
        let entry = context.enter_adapter_entry(target).unwrap();
        drop(context.execute_entered(
            target,
            entry,
            Box::new(|| panic!("unaccepted work must not execute")),
        ));
        assert_eq!(source.releases.load(Ordering::SeqCst), 1);
        let entry = context.enter_adapter_entry(target).unwrap();
        let mut future = pin!(context.execute_entered(
            target,
            entry,
            Box::new(|| panic!("unsupported work must not execute"))
        ));
        assert!(matches!(
            future
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop())),
            Poll::Ready(Err(_))
        ));
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert_eq!(source.entries.load(Ordering::SeqCst), 2);
        assert_eq!(source.releases.load(Ordering::SeqCst), 2);
    }

    struct Owner(Arc<AtomicUsize>);
    impl Drop for Owner {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    struct Authority {
        entries: Arc<AtomicUsize>,
        releases: Arc<AtomicUsize>,
        wakes: Arc<AtomicUsize>,
        reject: bool,
    }
    impl PluginCallbackAuthoritySource for Authority {
        fn check_live(&self) -> PluginResult<()> {
            if self.reject {
                Err(PluginCommandError::unavailable("selected authority closed"))
            } else {
                Ok(())
            }
        }
        fn enter_adapter_entry(
            &self,
            _: PluginInstanceId,
        ) -> PluginResult<PluginCallbackEntryGuard> {
            self.check_live()?;
            self.entries.fetch_add(1, Ordering::SeqCst);
            Ok(PluginCallbackEntryGuard::from_host(Owner(
                self.releases.clone(),
            )))
        }
        fn cancelled(&self) -> PluginCallbackSettlement {
            self.wakes.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Err(PluginCommandError::unavailable("wake source unavailable")) })
        }
    }

    struct RejectExecutor(Arc<AtomicUsize>);
    impl PluginCallbackExecution for RejectExecutor {
        fn execute(
            &self,
            _: PluginCallbackAuthority,
            _: PluginInstanceId,
            _: PluginBlockingCallback,
        ) -> PluginCallbackSettlement {
            self.0.fetch_add(1, Ordering::SeqCst);
            Box::pin(async {
                Err(PluginCommandError::unavailable(
                    "selected executor unsupported",
                ))
            })
        }
    }

    fn instance() -> PluginInstanceId {
        PluginInstanceId::from_uuid(uuid::Uuid::new_v4())
    }
    fn authority(reject: bool) -> (PluginCallbackAuthority, Arc<Authority>) {
        let source = Arc::new(Authority {
            entries: Arc::new(AtomicUsize::new(0)),
            releases: Arc::new(AtomicUsize::new(0)),
            wakes: Arc::new(AtomicUsize::new(0)),
            reject,
        });
        (PluginCallbackAuthority::from_host(source.clone()), source)
    }

    #[test]
    fn context_entry_delegates_once_and_retains_dependency_until_guard_drop() {
        let (bound_authority, source) = authority(false);
        let calls = Arc::new(AtomicUsize::new(0));
        let context = PluginCallbackContext::from_host(
            bound_authority,
            PluginCallbackExecutor::from_host(Arc::new(RejectExecutor(calls.clone()))),
        );
        let guard = context.enter_adapter_entry(instance()).unwrap();
        assert_eq!(source.entries.load(Ordering::SeqCst), 1);
        assert_eq!(source.releases.load(Ordering::SeqCst), 0);
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        drop(context);
        assert_eq!(source.releases.load(Ordering::SeqCst), 0);
        drop(guard);
        assert_eq!(source.releases.load(Ordering::SeqCst), 1);
        let (authority, rejected) = authority(true);
        let context = PluginCallbackContext::from_host(
            authority,
            PluginCallbackExecutor::from_host(Arc::new(RejectExecutor(calls))),
        );
        assert!(context.enter_adapter_entry(instance()).is_err());
        assert_eq!(rejected.entries.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn entry_guard_retains_actual_owner_and_drops_on_unwind() {
        let (authority, source) = authority(false);
        let guard = authority.enter_adapter_entry(instance()).unwrap();
        assert_eq!(source.entries.load(Ordering::SeqCst), 1);
        assert_eq!(source.releases.load(Ordering::SeqCst), 0);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            let _guard = guard;
            panic!("author panic");
        }));
        assert!(result.is_err());
        assert_eq!(source.releases.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn context_delegates_lazily_and_never_runs_callback_as_fallback() {
        let (authority, _) = authority(false);
        let calls = Arc::new(AtomicUsize::new(0));
        let context = PluginCallbackContext::from_host(
            authority,
            PluginCallbackExecutor::from_host(Arc::new(RejectExecutor(calls.clone()))),
        );
        let future = context.execute(instance(), Box::new(|| panic!("no inline fallback")));
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        drop(future);
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        let mut future = pin!(context.execute(instance(), Box::new(|| panic!("no fallback"))));
        assert!(matches!(
            future
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop())),
            Poll::Ready(Err(_))
        ));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn cancellation_delegates_existing_source_and_authority_errors_stay_errors() {
        let (authority, source) = authority(true);
        assert!(authority.check_live().is_err());
        assert!(authority.enter_adapter_entry(instance()).is_err());
        assert_eq!(source.entries.load(Ordering::SeqCst), 0);
        let future = authority.cancelled();
        assert_eq!(source.wakes.load(Ordering::SeqCst), 0);
        let mut future = pin!(future);
        assert!(matches!(
            future
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop())),
            Poll::Ready(Err(_))
        ));
        assert_eq!(source.wakes.load(Ordering::SeqCst), 1);
    }
}
