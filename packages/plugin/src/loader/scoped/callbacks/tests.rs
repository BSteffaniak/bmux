use super::*;
fn binding(execution: &Arc<LoadedExecution>) -> CallbackBinding {
    let invocation = execution.activation().unwrap().begin_invocation().unwrap();
    execution
        .bind_root_callback(invocation, ServiceCancellation::new())
        .unwrap()
}
#[test]
fn callback_entry_rejects_foreign_scope_and_preserves_actual_cross_instance_ancestry() {
    let first = crate::loader::scoped::tests::fixture();
    let other = crate::loader::scoped::tests::fixture();
    let root = binding(&first);
    assert!(root.active_entry().is_err());
    assert!(root.enter_adapter_entry(other.scope.id()).is_err());
    let entry = root.enter().unwrap();
    let invocation = other.activation().unwrap().begin_invocation().unwrap();
    let child = other
        .bind_child_callback(&entry, invocation, ServiceCancellation::new())
        .unwrap();
    let child_entry = child.enter().unwrap();
    let back = first.activation().unwrap().begin_invocation().unwrap();
    let back = first
        .bind_child_callback(&child_entry, back, ServiceCancellation::new())
        .unwrap();
    // While the parent entry runs, re-entering its instance is rejected.
    assert!(back.enter().is_err());
    drop(entry);
    // A child that outlives its parent call (e.g. retained by the callee) no
    // longer pins the parent: otherwise the parent invocation could never drain.
    assert!(root.active_entry().is_err());
    drop(back);
    drop(child_entry);
    drop(child);
    assert!(root.active_entry().is_err());
}
#[test]
fn sibling_entries_are_independent_and_closed_scope_cannot_mint_authority() {
    let execution = crate::loader::scoped::tests::fixture();
    let first = binding(&execution);
    let second = binding(&execution);
    let first_entry = first.enter().unwrap();
    let second_entry = second.enter().unwrap();
    assert!(first_entry.validate_binding(&first).is_ok());
    assert!(first_entry.validate_binding(&second).is_err());
    assert!(second_entry.validate_binding(&first).is_err());
    assert!(first.enter().is_err());
    first.invocation().close().unwrap();
    assert!(first.active_entry().is_err());
    assert!(second.active_entry().is_ok());
    drop(first_entry);
    drop(second_entry);
}
#[test]
fn recovery_v2_forwarding_preserves_rejection_and_live_entry_admission() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    struct Dispatch(Arc<AtomicUsize>);
    impl bmux_plugin_sdk::HostServiceDispatch for Dispatch {
        fn invoke(
            &self,
            _: &InvocationScope,
            _: bmux_plugin_sdk::HostServiceRequest<'_>,
        ) -> bmux_plugin_sdk::Result<Vec<u8>> {
            panic!("retained open must not invoke service work")
        }
        fn open_retained_host(
            &self,
            _: &InvocationScope,
        ) -> bmux_plugin_sdk::Result<bmux_plugin_sdk::abi::retained::RetainedHostPort> {
            panic!("V2 must not fall back to V1")
        }
        fn open_retained_host_v2(
            &self,
            _: &InvocationScope,
        ) -> bmux_plugin_sdk::Result<bmux_plugin_sdk::abi::retained::RetainedHostPort> {
            self.0.fetch_add(1, Ordering::AcqRel);
            Err(bmux_plugin_sdk::PluginError::UnsupportedHostOperation {
                operation: "chosen_recovery_v2",
            })
        }
    }
    let execution = crate::loader::scoped::tests::fixture();
    let binding = binding(&execution);
    let calls = Arc::new(AtomicUsize::new(0));
    let observed = calls.clone();
    let dispatch =
        binding.host_dispatch(Arc::new(move |_| Ok(Arc::new(Dispatch(observed.clone())))));
    let port = bmux_plugin_sdk::BoundHostServicePort::new(binding.invocation().clone(), dispatch);
    let services = LoadedExecution::bound_host_services(port)
        .with_kernel(|_| Ok(vec![]))
        .with_registrations(|_| Ok(vec![]));
    assert!(services.open_retained_host_v2().is_err());
    assert_eq!(calls.load(Ordering::Acquire), 0);
    let entry = binding.enter().unwrap();
    assert!(matches!(
        services.open_retained_host_v2(),
        Err(bmux_plugin_sdk::PluginError::UnsupportedHostOperation {
            operation: "chosen_recovery_v2"
        })
    ));
    assert_eq!(calls.load(Ordering::Acquire), 1);
    drop(entry);
    assert!(services.open_retained_host_v2().is_err());
    let activation = execution.activation().unwrap();
    let services = LoadedExecution::activation_owned_services(
        activation.clone(),
        Arc::new(Dispatch(calls.clone())),
    );
    assert!(matches!(
        services.open_retained_host_v2(),
        Err(bmux_plugin_sdk::PluginError::UnsupportedHostOperation {
            operation: "chosen_recovery_v2"
        })
    ));
    assert_eq!(calls.load(Ordering::Acquire), 2);
    activation.close().unwrap();
    assert!(services.open_retained_host_v2().is_err());
    assert_eq!(calls.load(Ordering::Acquire), 2);
}

mod recovery_v2_fixture {
    use super::*;
    use bmux_plugin_sdk::abi::retained::{
        NativeRetainedReservationV1, RetainedHostDispatch, RetainedHostObservation,
        RetainedHostPort,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};
    struct Ledger(Arc<AtomicUsize>);
    impl RetainedHostDispatch for Ledger {
        fn begin(
            &self,
            _: u64,
            _: &bmux_plugin_sdk::HostServiceRequest<'_>,
            _: NativeRetainedReservationV1,
        ) -> bmux_plugin_sdk::Result<u64> {
            panic!("lookup must not begin or replay producer work")
        }
        fn lookup(
            &self,
            id: u64,
            request: &bmux_plugin_sdk::HostServiceRequest<'_>,
            reservation: NativeRetainedReservationV1,
        ) -> bmux_plugin_sdk::Result<Option<u64>> {
            assert_eq!(id, 41);
            assert_eq!(request.payload, b"original");
            assert_eq!(reservation.max_outcome_bytes, 64);
            self.0.fetch_add(1, Ordering::AcqRel);
            Ok(None)
        }
        fn observe(&self, _: u64, _: u64) -> bmux_plugin_sdk::Result<RetainedHostObservation> {
            panic!("no operation admitted")
        }
        fn control(&self, _: u64, _: u32) -> bmux_plugin_sdk::Result<bool> {
            Ok(true)
        }
        fn acknowledge(&self, _: u64, _: u64) -> bmux_plugin_sdk::Result<()> {
            panic!("no operation admitted")
        }
        fn release(&self) -> bmux_plugin_sdk::Result<bool> {
            Ok(true)
        }
    }
    pub(super) struct Dispatch {
        pub(super) calls: Arc<AtomicUsize>,
        pub(super) opens: Arc<AtomicUsize>,
    }
    impl bmux_plugin_sdk::HostServiceDispatch for Dispatch {
        fn invoke(
            &self,
            _: &InvocationScope,
            _: bmux_plugin_sdk::HostServiceRequest<'_>,
        ) -> bmux_plugin_sdk::Result<Vec<u8>> {
            panic!("open is not service execution")
        }
        fn open_retained_host(
            &self,
            _: &InvocationScope,
        ) -> bmux_plugin_sdk::Result<RetainedHostPort> {
            panic!("V2 cannot substitute V1")
        }
        fn open_retained_host_v2(
            &self,
            invocation: &InvocationScope,
        ) -> bmux_plugin_sdk::Result<RetainedHostPort> {
            assert!(!invocation.is_cancelled().unwrap());
            self.opens.fetch_add(1, Ordering::AcqRel);
            Ok(RetainedHostPort::new(Arc::new(Ledger(self.calls.clone()))))
        }
    }
}

#[test]
fn recovery_v2_opens_lookup_capable_owner_through_bound_decorators() {
    use bmux_plugin_sdk::abi::retained::NativeRetainedReservationV1;
    use recovery_v2_fixture::Dispatch;
    use std::sync::atomic::{AtomicUsize, Ordering};
    let execution = crate::loader::scoped::tests::fixture();
    let binding = binding(&execution);
    let calls = Arc::new(AtomicUsize::new(0));
    let opens = Arc::new(AtomicUsize::new(0));
    let dispatch = Arc::new(Dispatch {
        calls: calls.clone(),
        opens: opens.clone(),
    });
    let selected = dispatch.clone();
    let entry = binding.enter().unwrap();
    let host = binding.host_dispatch(Arc::new(move |_entry| Ok(selected.clone())));
    let services = execution
        .decorate_host_services(
            Some(bmux_plugin_sdk::BoundHostServicePort::new(
                binding.invocation().clone(),
                host,
            )),
            None,
        )
        .unwrap()
        .unwrap()
        .with_kernel(|_| Ok(vec![]))
        .with_registrations(|_| Ok(vec![]));
    let owner = services.open_retained_host_v2().unwrap();
    assert_eq!(opens.load(Ordering::Acquire), 1);
    drop(entry);
    binding.invocation().close().unwrap();
    assert!(services.open_retained_host_v2().is_err());
    let request = bmux_plugin_sdk::HostServiceRequest {
        capability: "example.read",
        kind: bmux_plugin_sdk::ServiceKind::Query,
        interface: "example/v1",
        operation: "read",
        payload: b"original",
    };
    let reservation = NativeRetainedReservationV1 {
        size: u32::try_from(std::mem::size_of::<NativeRetainedReservationV1>()).unwrap(),
        version: 1,
        max_outcome_bytes: 64,
        operation_slots: 1,
        reserved: 0,
    };
    assert!(owner.lookup(41, &request, reservation).unwrap().is_none());
    assert!(owner.release().unwrap());
    assert_eq!(calls.load(Ordering::Acquire), 1);
    assert_eq!(opens.load(Ordering::Acquire), 1);
    let activation = execution.activation().unwrap();
    let services = LoadedExecution::activation_owned_services(activation.clone(), dispatch);
    let owner = services.open_retained_host_v2().unwrap();
    activation.close().unwrap();
    assert!(services.open_retained_host_v2().is_err());
    assert!(owner.lookup(41, &request, reservation).unwrap().is_none());
    assert!(owner.release().unwrap());
    assert_eq!(calls.load(Ordering::Acquire), 2);
    assert_eq!(opens.load(Ordering::Acquire), 2);
}

#[tokio::test]
async fn reusable_scope_host_pins_code_through_entered_settlement_without_loaded_execution() {
    use std::sync::atomic::{AtomicBool, Ordering};
    struct Code(Arc<AtomicBool>);
    impl Drop for Code {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Release);
        }
    }
    let execution = crate::loader::scoped::tests::fixture();
    let activation = execution.activation().unwrap();
    let invocation = activation.begin_invocation().unwrap();
    let released = Arc::new(AtomicBool::new(false));
    let host = LoaderCallbackHost::new(
        execution.scope.clone(),
        Arc::new(Code(released.clone())),
        tokio::runtime::Handle::current(),
    );
    let binding = host
        .bind_root(&activation, invocation.clone(), ServiceCancellation::new())
        .unwrap();
    assert!(binding.execution().is_none());
    let context = binding.context();
    let entry = context.enter_adapter_entry(execution.scope.id()).unwrap();
    let (started, observing) = tokio::sync::oneshot::channel();
    let (finish, waiting) = std::sync::mpsc::channel();
    let mut operation = context.execute_entered(
        execution.scope.id(),
        entry,
        Box::new(move || {
            let _ = started.send(());
            waiting.recv().unwrap();
            Ok(())
        }),
    );
    assert!(
        std::future::poll_fn(|cx| std::task::Poll::Ready(operation.as_mut().poll(cx)))
            .await
            .is_pending()
    );
    observing.await.unwrap();
    drop(host);
    drop(binding);
    drop(context);
    drop(operation);
    assert!(!released.load(Ordering::Acquire));
    finish.send(()).unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    while !released.load(Ordering::Acquire) {
        assert!(std::time::Instant::now() < deadline);
        tokio::task::yield_now().await;
    }
    invocation.close().unwrap();
    invocation.finish().unwrap();
}

#[tokio::test]
async fn entered_executor_rejects_other_authority_without_using_active_entry() {
    let execution = crate::loader::scoped::tests::fixture();
    let binding = binding(&execution);
    let selected = binding.context();
    // Same invocation and same currently active entry, but a distinct
    // authority allocation: the selected executor must reject this guard.
    let foreign = binding.context();
    let guard = foreign.enter_adapter_entry(execution.scope.id()).unwrap();
    assert!(binding.active_entry().is_ok());
    let error = selected
        .execute_entered(
            execution.scope.id(),
            guard,
            Box::new(|| panic!("foreign prepared guard must never execute")),
        )
        .await
        .unwrap_err();
    assert!(error.message.contains("authority"));
    assert!(binding.active_entry().is_err());
    let guard = selected.enter_adapter_entry(execution.scope.id()).unwrap();
    selected
        .execute_entered(execution.scope.id(), guard, Box::new(|| Ok(())))
        .await
        .unwrap();
}

#[tokio::test]
async fn operation_cancel_wakes_prepared_callback_scope_without_settlement() {
    let execution = crate::loader::scoped::tests::fixture();
    let binding = binding(&execution);
    let invocation = binding.invocation().clone();
    let context = binding.context();
    let id = execution
        .begin_with_callback_dispatch(
            bmux_plugin_sdk::abi::NativeOperation::Extension {
                invocation_id: bmux_plugin_sdk::PluginInvocationId::new(),
                contract: "example/v1".into(),
                payload: vec![],
            },
            binding,
            None,
            None,
        )
        .unwrap();
    let mut cancelled = context.cancelled();
    assert!(
        std::future::poll_fn(|cx| std::task::Poll::Ready(cancelled.as_mut().poll(cx)))
            .await
            .is_pending()
    );
    execution.cancel(id).unwrap();
    cancelled.await.unwrap();
    assert!(invocation.is_cancelled().unwrap());
    assert!(execution.call(id).is_ok());
}

#[tokio::test]
async fn dropping_queued_prepared_entry_never_executes_or_retains_admission() {
    let execution = crate::loader::scoped::tests::fixture();
    let binding = binding(&execution);
    let context = binding.context();
    let held = execution
        .callback_owner
        .slots
        .clone()
        .acquire_many_owned(u32::try_from(MAX_ACCEPTED).unwrap())
        .await
        .unwrap();
    let entry = context.enter_adapter_entry(execution.scope.id()).unwrap();
    let mut operation = context.execute_entered(
        execution.scope.id(),
        entry,
        Box::new(|| panic!("dropped queued callback must never execute")),
    );
    assert!(
        std::future::poll_fn(|cx| std::task::Poll::Ready(operation.as_mut().poll(cx)))
            .await
            .is_pending()
    );
    assert!(binding.active_entry().is_ok());
    drop(operation);
    assert!(binding.active_entry().is_err());
    assert_eq!(
        execution
            .callback_owner
            .waiting
            .load(std::sync::atomic::Ordering::Acquire),
        0
    );
    drop(held);
}

#[tokio::test]
async fn accepted_callback_survives_observer_loss_and_runs_off_executor_thread() {
    let execution = crate::loader::scoped::tests::fixture();
    let binding = binding(&execution);
    let context = binding.context();
    let entry = context.enter_adapter_entry(execution.scope.id()).unwrap();
    let origin = std::thread::current().id();
    let (started, observed) = tokio::sync::oneshot::channel();
    let (resume, wait) = std::sync::mpsc::channel();
    let (settled, complete) = tokio::sync::oneshot::channel();
    let mut operation = context.execute_entered(
        execution.scope.id(),
        entry,
        Box::new(move || {
            assert_ne!(std::thread::current().id(), origin);
            let _ = started.send(());
            wait.recv().unwrap();
            // The retained worker still enters the selected reactor after its
            // observer is gone; the caller keeps driving that actual runtime.
            tokio::runtime::Handle::current().block_on(async {
                tokio::time::sleep(std::time::Duration::from_millis(1)).await;
                tokio::spawn(async {}).await.unwrap();
            });
            let _ = settled.send(());
            Ok(())
        }),
    );
    assert!(
        std::future::poll_fn(|cx| std::task::Poll::Ready(operation.as_mut().poll(cx)))
            .await
            .is_pending()
    );
    observed.await.unwrap();
    drop(operation);
    assert!(binding.active_entry().is_ok());
    resume.send(()).unwrap();
    complete.await.unwrap();
}

#[test]
fn callback_workers_use_the_selected_reactor_not_the_observers() {
    let observer = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    for name in ["selected-first", "selected-second"] {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .thread_name(name)
            .enable_all()
            .build()
            .unwrap();
        let execution = crate::loader::scoped::tests::fixture();
        let activation = execution.activation().unwrap();
        let invocation = activation.begin_invocation().unwrap();
        let host = LoaderCallbackHost::new(
            execution.scope.clone(),
            execution.clone(),
            runtime.handle().clone(),
        );
        let binding = host
            .bind_root(&activation, invocation, ServiceCancellation::new())
            .unwrap();
        let context = binding.context();
        let entry = context.enter_adapter_entry(execution.scope.id()).unwrap();
        let (resume, wait) = std::sync::mpsc::channel();
        let operation = context.execute_entered(
            execution.scope.id(),
            entry,
            Box::new(move || {
                // A blocking-pool thread of the selected runtime, not a
                // thread created for this callback.
                assert_eq!(std::thread::current().name(), Some(name));
                wait.recv_timeout(std::time::Duration::from_secs(5))
                    .unwrap();
                tokio::runtime::Handle::current().block_on(async move {
                    tokio::time::sleep(std::time::Duration::from_millis(1)).await;
                    let actual =
                        tokio::spawn(async { std::thread::current().name().unwrap().to_owned() })
                            .await
                            .unwrap();
                    assert_eq!(actual, name);
                });
                Ok(())
            }),
        );
        observer.block_on(async {
            let (result, ()) = tokio::join!(operation, async move {
                // This reactor must progress while the selected worker waits.
                tokio::time::sleep(std::time::Duration::from_millis(1)).await;
                resume.send(()).unwrap();
            });
            result.unwrap();
        });
        assert!(binding.active_entry().is_err());
        drop(binding);
        drop(host);
        // Only settled work is left before this selected runtime owner exits.
        drop(runtime);
    }
}

#[test]
fn callback_without_selected_reactor_rejects_before_author_effects() {
    let execution = crate::loader::scoped::tests::fixture();
    let binding = binding(&execution);
    let context = binding.context();
    let entry = context.enter_adapter_entry(execution.scope.id()).unwrap();
    let observer = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let error = observer
        .block_on(context.execute_entered(
            execution.scope.id(),
            entry,
            Box::new(|| panic!("missing selected reactor must reject before author work")),
        ))
        .unwrap_err();
    assert!(
        error
            .message
            .contains("selected callback runtime unavailable")
    );
    assert!(binding.active_entry().is_err());
}
#[test]
fn activation_dispatch_routes_by_admitted_invocation_and_live_entry() {
    struct Tagged(u8);
    impl bmux_plugin_sdk::HostServiceDispatch for Tagged {
        fn invoke(
            &self,
            _: &InvocationScope,
            _: bmux_plugin_sdk::HostServiceRequest<'_>,
        ) -> super::super::Result<Vec<u8>> {
            Ok(vec![self.0])
        }
    }
    let request = || bmux_plugin_sdk::HostServiceRequest {
        capability: "test",
        kind: bmux_plugin_sdk::ServiceKind::Query,
        interface: "test/v1",
        operation: "read",
        payload: &[],
    };
    let execution = crate::loader::scoped::tests::fixture();
    let root = binding(&execution);
    let own = root.invocation().clone();
    let fresh = execution.activation().unwrap().begin_invocation().unwrap();
    let dispatch = root.activation_dispatch(Arc::new(Tagged(1)), Some(Arc::new(Tagged(2))));
    // Outside the author entry, even the lifecycle invocation uses the
    // selected instance dispatch.
    assert_eq!(dispatch.invoke(&own, request()).unwrap(), vec![2]);
    let entry = root.enter().unwrap();
    // Inside the entry, the lifecycle invocation keeps its entry ancestry...
    assert_eq!(dispatch.invoke(&own, request()).unwrap(), vec![1]);
    // ...while a freshly minted activation invocation never borrows it.
    assert_eq!(dispatch.invoke(&fresh, request()).unwrap(), vec![2]);
    drop(entry);
    assert_eq!(dispatch.invoke(&own, request()).unwrap(), vec![2]);
    // Without a selected instance dispatch nothing outside the entry is routed.
    let unbound = root.activation_dispatch(Arc::new(Tagged(1)), None);
    assert!(unbound.invoke(&fresh, request()).is_err());
    assert!(unbound.invoke(&own, request()).is_err());
}
/// Deterministic interleaving of the race between background work using the
/// activation services and the author leaving `activate`: the author entry
/// ends after the route chose the entered dispatch but before that dispatch
/// resolves the entry. The chosen route must stay valid for the whole call.
#[test]
fn activation_dispatch_keeps_the_chosen_entry_route_when_the_author_leaves() {
    struct LeavesThenResolves {
        root: CallbackBinding,
        author: Mutex<Option<CallbackEntryLease>>,
    }
    impl bmux_plugin_sdk::HostServiceDispatch for LeavesThenResolves {
        fn invoke(
            &self,
            _: &InvocationScope,
            _: bmux_plugin_sdk::HostServiceRequest<'_>,
        ) -> super::super::Result<Vec<u8>> {
            // The author returns from `activate` right now...
            drop(self.author.lock().unwrap().take());
            // ...and the entered dispatch resolves the entry, as
            // `EntryDispatch` does, after the route was already chosen.
            self.root.active_entry()?;
            Ok(vec![1])
        }
    }
    let execution = crate::loader::scoped::tests::fixture();
    let root = binding(&execution);
    let own = root.invocation().clone();
    let author = root.enter().unwrap();
    let entered = Arc::new(LeavesThenResolves {
        root: root.clone(),
        author: Mutex::new(Some(author)),
    });
    let dispatch = root.activation_dispatch(entered, None);
    let request = bmux_plugin_sdk::HostServiceRequest {
        capability: "test",
        kind: bmux_plugin_sdk::ServiceKind::Query,
        interface: "test/v1",
        operation: "read",
        payload: &[],
    };
    assert_eq!(dispatch.invoke(&own, request).unwrap(), vec![1]);
    // Once that call returned, the entry is gone: later calls route outside it.
    assert!(root.active_entry().is_err());
}
/// A synchronous author callback runs inside the instance's activation scope,
/// so plug-rt work it spawns is owned by the activation and torn down with
/// it instead of escaping as an untracked runtime task.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sync_callback_spawns_join_the_activation_scope() {
    // The fixture binds the current runtime as the selected callback runtime.
    let execution = crate::loader::scoped::tests::fixture();
    let root = binding(&execution);
    let context = root.context();
    let entry = context.enter_adapter_entry(execution.scope.id()).unwrap();
    context
        .execute_entered(
            execution.scope.id(),
            entry,
            Box::new(|| {
                drop(bmux_plugin_runtime::rt::spawn_named(
                    "callback-spawned",
                    std::future::pending::<()>(),
                ));
                Ok(())
            }),
        )
        .await
        .unwrap();
    assert!(
        execution
            .pending_background_work()
            .contains(&"callback-spawned".to_string())
    );
    execution.drain_background_tasks().await.unwrap();
    assert_eq!(execution.pending_background_work(), Vec::<String>::new());
}
#[test]
fn reentrant_query_binding_enters_same_instance_while_default_binding_is_rejected() {
    let first = crate::loader::scoped::tests::fixture();
    let other = crate::loader::scoped::tests::fixture();
    let root = binding(&first);
    let entry = root.enter().unwrap();
    assert!(!entry.is_reentrant());
    let hop = other.activation().unwrap().begin_invocation().unwrap();
    let hop = other
        .bind_child_callback(&entry, hop, ServiceCancellation::new())
        .unwrap();
    let hop_entry = hop.enter().unwrap();
    // Cross-instance hops never count as re-entry.
    assert!(!hop_entry.is_reentrant());

    let rejected = first.activation().unwrap().begin_invocation().unwrap();
    let rejected = first
        .bind_child_callback(&hop_entry, rejected, ServiceCancellation::new())
        .unwrap();
    let Err(error) = rejected.enter() else {
        panic!("default binding must reject same-instance re-entry");
    };
    assert!(
        error
            .message
            .contains("recursive callback dependency rejected")
    );

    let allowed = first.activation().unwrap().begin_invocation().unwrap();
    let allowed = first
        .bind_child_callback_with(
            &hop_entry,
            allowed,
            ServiceCancellation::new(),
            CallbackReentry::AllowQuery,
        )
        .unwrap();
    let reentered = allowed.enter().expect("reentrant query binding may enter");
    assert!(reentered.is_reentrant());
    // The opt-in skips only the ancestor rejection: one author entry per
    // invocation still holds, and the adapter guard carries the marker.
    assert!(allowed.enter().is_err());
    drop(reentered);
    let guard = allowed.enter_adapter_entry(first.scope.id()).unwrap();
    assert!(guard.is_reentrant());
    drop(guard);

    // Without a live same-instance ancestor an allowed binding is ordinary.
    drop(hop_entry);
    drop(entry);
    let fresh_parent = binding(&other);
    let fresh_parent_entry = fresh_parent.enter().unwrap();
    let ordinary = first.activation().unwrap().begin_invocation().unwrap();
    let ordinary = first
        .bind_child_callback_with(
            &fresh_parent_entry,
            ordinary,
            ServiceCancellation::new(),
            CallbackReentry::AllowQuery,
        )
        .unwrap();
    assert!(!ordinary.enter().unwrap().is_reentrant());
}
#[test]
fn reentrant_binding_keeps_ancestry_depth_limit() {
    let first = crate::loader::scoped::tests::fixture();
    let other = crate::loader::scoped::tests::fixture();
    let mut bindings = vec![binding(&first)];
    let mut entries = vec![bindings[0].enter().unwrap()];
    for depth in 0..MAX_ANCESTORS {
        let execution = if depth % 2 == 0 { &other } else { &first };
        let invocation = execution.activation().unwrap().begin_invocation().unwrap();
        let child = execution
            .bind_child_callback_with(
                entries.last().unwrap(),
                invocation,
                ServiceCancellation::new(),
                CallbackReentry::AllowQuery,
            )
            .unwrap();
        entries.push(child.enter().unwrap());
        bindings.push(child);
    }
    let invocation = first.activation().unwrap().begin_invocation().unwrap();
    assert!(
        first
            .bind_child_callback_with(
                entries.last().unwrap(),
                invocation,
                ServiceCancellation::new(),
                CallbackReentry::AllowQuery,
            )
            .is_err(),
        "re-entry opt-in must not lift the ancestry depth limit"
    );
}
