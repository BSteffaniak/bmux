use super::*;
use bmux_plugin_sdk::abi::retained::{
    RETAINED_HOST_CONTROL_CLOSE, RETAINED_HOST_CONTROL_JOIN, RetainedHostOutcome,
};
#[derive(Default)]
enum ReleasePolicy {
    EmptyAfterClose,
    #[default]
    RequireSettlementAndAck,
}

#[derive(Default)]
struct Ledger {
    closed: bool,
    done: bool,
    acknowledged: bool,
    request: Option<(u64, Vec<u8>)>,
    starts: usize,
    // Only the recovery fixture opts into a quiescent, operation-free owner.
    release_policy: ReleasePolicy,
}
#[derive(Default)]
struct Dispatch(Mutex<Ledger>);
impl RetainedHostDispatch for Dispatch {
    fn begin(
        &self,
        id: u64,
        request: &bmux_plugin_sdk::HostServiceRequest<'_>,
        _: NativeRetainedReservationV1,
    ) -> Result<u64> {
        let mut state = self.0.lock().unwrap();
        if let Some((prior, bytes)) = &state.request {
            return if *prior == id && bytes == request.payload {
                Ok(7)
            } else {
                Err(failure("conflicting request"))
            };
        }
        if state.closed {
            return Err(failure("closed"));
        }
        state.starts += 1;
        state.request = Some((id, request.payload.to_vec()));
        drop(state);
        Ok(7)
    }
    fn lookup(
        &self,
        id: u64,
        request: &bmux_plugin_sdk::HostServiceRequest<'_>,
        reservation: NativeRetainedReservationV1,
    ) -> Result<Option<u64>> {
        let state = self.0.lock().unwrap();
        let result = match &state.request {
            None => Ok(None),
            Some((prior, payload))
                if *prior == id
                    && payload == request.payload
                    && request.capability == "example.read"
                    && request.kind == bmux_plugin_sdk::ServiceKind::Query
                    && request.interface == "example/v1"
                    && request.operation == "read"
                    && reservation.size
                        == u32::try_from(std::mem::size_of::<NativeRetainedReservationV1>())
                            .unwrap()
                    && reservation.version == 1
                    && reservation.operation_slots == 1
                    && reservation.reserved == 0
                    && reservation.max_outcome_bytes == 32 =>
            {
                Ok(Some(7))
            }
            Some(_) => Err(failure("conflicting recovery facts")),
        };
        drop(state);
        result
    }
    fn observe(&self, operation: u64, _: u64) -> Result<RetainedHostObservation> {
        let state = self.0.lock().unwrap();
        if operation != 7 || state.acknowledged {
            return Err(failure("unknown outcome"));
        }
        Ok(if state.done {
            RetainedHostObservation::Terminal {
                receipt: 91,
                outcome: RetainedHostOutcome::ResponseLimitExceeded {
                    observed_bytes: 1000,
                },
            }
        } else {
            RetainedHostObservation::Pending
        })
    }
    fn control(&self, operation: u64, command: u32) -> Result<bool> {
        if operation != 0 && operation != 7 {
            return Err(failure("foreign operation"));
        }
        let mut state = self.0.lock().unwrap();
        match command {
            RETAINED_HOST_CONTROL_CLOSE => {
                state.closed = true;
                Ok(true)
            }
            RETAINED_HOST_CONTROL_JOIN => Ok(state.done),
            _ => Ok(true),
        }
    }
    fn acknowledge(&self, operation: u64, receipt: u64) -> Result<()> {
        let mut state = self.0.lock().unwrap();
        if operation != 7 || receipt != 91 || !state.done {
            return Err(failure("invalid receipt"));
        }
        state.acknowledged = true;
        drop(state);
        Ok(())
    }
    fn release(&self) -> Result<bool> {
        let state = self.0.lock().unwrap();
        Ok(state.closed
            && ((matches!(state.release_policy, ReleasePolicy::EmptyAfterClose)
                && state.request.is_none())
                || (state.done && state.acknowledged)))
    }
}
fn slice(value: &[u8]) -> NativeRetainedSliceV1 {
    NativeRetainedSliceV1 {
        pointer: value.as_ptr(),
        length: u32::try_from(value.len()).unwrap(),
    }
}
fn start_and_validate(
    table: &NativeRetainedHostPortV1,
    dispatch: &Dispatch,
) -> (NativeRetainedRequestV1, NativeRetainedReservationV1, u64) {
    let mut request = NativeRetainedRequestV1 {
        size: u32::try_from(std::mem::size_of::<NativeRetainedRequestV1>()).unwrap(),
        version: 1,
        kind: 0,
        capability: slice(b"example.read"),
        interface: slice(b"example/v1"),
        operation: slice(b"read"),
        payload: slice(b"payload"),
    };
    let mut budget = NativeRetainedReservationV1 {
        size: u32::try_from(std::mem::size_of::<NativeRetainedReservationV1>()).unwrap(),
        version: 1,
        max_outcome_bytes: 32,
        operation_slots: 1,
        reserved: 0,
    };
    let mut operation = 99;
    request.size = 8;
    assert_eq!(
        unsafe {
            (table.begin)(
                table.handle,
                1,
                &raw const request,
                &raw const budget,
                &raw mut operation,
            )
        },
        abi::UNSUPPORTED
    );
    request.size = u32::try_from(std::mem::size_of_val(&request)).unwrap();
    budget.version = 2;
    assert_eq!(
        unsafe {
            (table.begin)(
                table.handle,
                1,
                &raw const request,
                &raw const budget,
                &raw mut operation,
            )
        },
        abi::UNSUPPORTED
    );
    assert_eq!(dispatch.0.lock().unwrap().starts, 0);
    budget.version = 1;
    assert_eq!(
        unsafe {
            (table.begin)(
                table.handle,
                1,
                &raw const request,
                &raw const budget,
                &raw mut operation,
            )
        },
        abi::OK
    );
    (request, budget, operation)
}

fn mark_recovery_fixture_settled(dispatch: &Dispatch) {
    let mut state = dispatch.0.lock().unwrap();
    state.done = true;
    state.acknowledged = true;
    drop(state);
}

#[test]
fn recovery_v2_keeps_owner_after_close_without_restarting() {
    let execution = super::super::tests::fixture();
    let dispatch = Arc::new(Dispatch::default());
    let table = table_v2(open(execution, dispatch.clone()).unwrap());
    let (mut request, mut budget, id) = start_and_validate(&table.base, &dispatch);
    assert_eq!(
        unsafe { (table.base.control)(table.base.handle, 0, RETAINED_HOST_CONTROL_CLOSE) },
        abi::OK
    );
    let mut recovered = 99;
    assert_eq!(
        unsafe {
            (table.lookup)(
                table.base.handle,
                1,
                &raw const request,
                &raw const budget,
                &raw mut recovered,
            )
        },
        abi::OK
    );
    assert_eq!(recovered, id);
    budget.max_outcome_bytes += 1;
    assert_eq!(
        unsafe {
            (table.lookup)(
                table.base.handle,
                1,
                &raw const request,
                &raw const budget,
                &raw mut recovered,
            )
        },
        abi::FAILED
    );
    assert_eq!(recovered, 0);
    budget.max_outcome_bytes -= 1;
    request.kind = 99;
    assert_eq!(
        unsafe {
            (table.lookup)(
                table.base.handle,
                1,
                &raw const request,
                &raw const budget,
                &raw mut recovered,
            )
        },
        abi::INVALID_ARGUMENT
    );
    assert_eq!(recovered, 0);
    assert_eq!(dispatch.0.lock().unwrap().starts, 1);
    mark_recovery_fixture_settled(&dispatch);
    assert_eq!(unsafe { (table.base.release)(table.base.handle) }, abi::OK);
    let empty = Arc::new(Dispatch(Mutex::new(Ledger {
        release_policy: ReleasePolicy::EmptyAfterClose,
        ..Ledger::default()
    })));
    let table = table_v2(open(super::super::tests::fixture(), empty.clone()).unwrap());
    request.kind = 0;
    recovered = 99;
    assert_eq!(
        unsafe {
            (table.lookup)(
                table.base.handle,
                1,
                &raw const request,
                &raw const budget,
                &raw mut recovered,
            )
        },
        abi::NOT_FOUND
    );
    assert_eq!(recovered, 0);
    assert_eq!(empty.0.lock().unwrap().starts, 0);
    assert_eq!(
        unsafe { (table.base.release)(table.base.handle) },
        abi::BUSY
    );
    assert_eq!(
        unsafe { (table.base.control)(table.base.handle, 0, RETAINED_HOST_CONTROL_CLOSE) },
        abi::OK
    );
    assert_eq!(
        unsafe {
            (table.lookup)(
                table.base.handle,
                1,
                &raw const request,
                &raw const budget,
                &raw mut recovered,
            )
        },
        abi::NOT_FOUND
    );
    assert_eq!(recovered, 0);
    assert_eq!(empty.0.lock().unwrap().starts, 0);
    assert_eq!(unsafe { (table.base.release)(table.base.handle) }, abi::OK);
}

#[test]
fn recovery_and_factory_release_contracts_are_independent_in_parallel() {
    std::thread::scope(|scope| {
        let recovery = scope.spawn(recovery_v2_keeps_owner_after_close_without_restarting);
        let factory = scope.spawn(factory_is_once_bound_and_receives_actual_activation);
        recovery.join().unwrap();
        factory.join().unwrap();
    });
}

#[test]
fn factory_is_once_bound_and_receives_actual_activation() {
    let execution = super::super::tests::fixture_unactivated();
    assert!(execution.open_retained_host().is_err());
    let dispatch = Arc::new(Dispatch::default());
    let producer = dispatch.clone();
    let seen = Arc::new(Mutex::new(None));
    let observed = seen.clone();
    execution
        .bind_retained_host_factory(Arc::new(move |activation| {
            *observed.lock().unwrap() = Some((
                activation.runtime_id(),
                activation.instance_id(),
                activation.id(),
            ));
            Ok(dispatch.clone())
        }))
        .unwrap();
    assert!(
        execution
            .bind_retained_host_factory(Arc::new(|_| Err(failure("declined"))))
            .is_err()
    );
    let activation = execution
        .scope
        .begin_activation(bmux_plugin_runtime::PluginConcurrencyConfig::Concurrent)
        .unwrap();
    execution.bind_retained_activation(&activation).unwrap();
    assert_eq!(
        *seen.lock().unwrap(),
        Some((
            execution.scope.runtime_id(),
            execution.scope.id(),
            activation.id()
        ))
    );
    execution.state.lock().unwrap().activation = Some(activation.clone());
    assert!(
        execution
            .bind_retained_host_factory(Arc::new(|_| Err(failure("late"))))
            .is_err()
    );
    activation.mark_active().unwrap();
    let port = execution.open_retained_host().unwrap();
    assert!(!port.release().unwrap());
    drop(port);
    assert!(has_owner(&execution).unwrap());
    let dispatch = execution.retained_dispatch.lock().unwrap().clone().unwrap();
    dispatch.control(0, RETAINED_HOST_CONTROL_CLOSE).unwrap();
    assert!(!execution.retry_release_retained_host().unwrap());
    {
        let mut state = producer.0.lock().unwrap();
        state.done = true;
        state.acknowledged = true;
    }
    assert!(execution.retry_release_retained_host().unwrap());
    assert!(!has_owner(&execution).unwrap());
}

#[tokio::test]
async fn retained_native_table_survives_invocation_and_preserves_ledger_recovery() {
    let execution = super::super::tests::fixture();
    let invocation = execution.begin_invocation_scope().unwrap();
    let dispatch = Arc::new(Dispatch::default());
    let bound: Arc<dyn RetainedHostDispatch> = dispatch.clone();
    *execution.retained_dispatch.lock().unwrap() = Some(bound.clone());
    let table = open(execution.clone(), bound).unwrap();
    assert_eq!(unsafe { (table.retain)(table.handle) }, abi::OK);
    assert_eq!(unsafe { (table.release)(table.handle) }, abi::OK);
    let (request, budget, mut operation) = start_and_validate(&table, &dispatch);
    invocation.close().unwrap();
    assert!(has_owner(&execution).unwrap());
    assert!(execution.destroy().await.is_err());
    let mut length = 0;
    let mut receipt = 0;
    assert_eq!(
        unsafe {
            (table.read)(
                table.handle,
                operation,
                std::ptr::null_mut(),
                0,
                &raw mut length,
                &raw mut receipt,
            )
        },
        abi::PENDING
    );
    assert_eq!(unsafe { (table.release)(table.handle) }, abi::BUSY);
    assert_eq!(
        unsafe { (table.control)(table.handle, 0, RETAINED_HOST_CONTROL_CLOSE) },
        abi::OK
    );
    assert_eq!(
        unsafe { (table.control)(table.handle, 0, RETAINED_HOST_CONTROL_JOIN) },
        abi::PENDING
    );
    dispatch.0.lock().unwrap().done = true;
    assert_eq!(
        unsafe {
            (table.read)(
                table.handle,
                operation,
                std::ptr::null_mut(),
                0,
                &raw mut length,
                &raw mut receipt,
            )
        },
        abi::BUFFER_TOO_SMALL
    );
    assert_eq!(length, 16);
    let mut output = [0; 16];
    assert_eq!(
        unsafe {
            (table.read)(
                table.handle,
                operation,
                output.as_mut_ptr(),
                16,
                &raw mut length,
                &raw mut receipt,
            )
        },
        abi::OK
    );
    assert_eq!(
        RetainedHostOutcome::decode(&output).unwrap(),
        RetainedHostOutcome::ResponseLimitExceeded {
            observed_bytes: 1000
        }
    );
    assert_ne!(unsafe { (table.ack)(table.handle, operation, 92) }, abi::OK);
    assert_eq!(
        unsafe { (table.ack)(table.handle, operation, receipt) },
        abi::OK
    );
    assert_eq!(
        unsafe {
            (table.begin)(
                table.handle,
                1,
                &raw const request,
                &raw const budget,
                &raw mut operation,
            )
        },
        abi::OK
    );
    assert_eq!(dispatch.0.lock().unwrap().starts, 1);
    assert_eq!(
        unsafe { (table.control)(table.handle, 0, RETAINED_HOST_CONTROL_JOIN) },
        abi::OK
    );
    assert_eq!(unsafe { (table.release)(table.handle) }, abi::OK);
    assert!(!has_owner(&execution).unwrap());
}
