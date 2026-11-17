//! HUMAN service boundary: every execution-scoped operation is reauthorized.
//!
//! Wire these functions through the ordinary `route_service!` dispatcher so its
//! observer-connection guard applies as well. Capability discovery is public,
//! matching the existing presentation capability handlers; it grants no input.
use bmux_pane_runtime_plugin_api::pane_runtime_execution_v4::TerminalExecutionRef;
use bmux_pane_runtime_plugin_api::pane_runtime_human_input_v1::{
    HumanInputAck, HumanInputCapabilities, HumanInputError, HumanInputStream,
};
use bmux_plugin_sdk::NativeServiceContext;

pub(crate) fn capabilities() -> HumanInputCapabilities {
    crate::runtime::human_input::capabilities()
}

fn authorize(
    ctx: &NativeServiceContext,
    execution: &TerminalExecutionRef,
) -> Result<(), HumanInputError> {
    // Includes cancellation and authoritative session permission checks. Stream
    // UUID possession is never treated as a credential or permission grant.
    super::execution::authorize(ctx, execution, "pane.direct_input")
        .map_err(|error| HumanInputError::Authority { error })
}

pub(crate) fn open(
    host: &crate::runtime::PaneRuntimeHost,
    ctx: &NativeServiceContext,
    execution: TerminalExecutionRef,
) -> Result<HumanInputStream, HumanInputError> {
    authorize(ctx, &execution)?;
    crate::runtime::human_input::open(host, execution)
}

pub(crate) fn push(
    host: &crate::runtime::PaneRuntimeHost,
    ctx: &NativeServiceContext,
    stream: &HumanInputStream,
    sequence: u64,
    data: Vec<u8>,
) -> Result<HumanInputAck, HumanInputError> {
    authorize(ctx, &stream.execution)?;
    crate::runtime::human_input::push(
        host,
        stream,
        sequence,
        data,
        super::execution::input_authority(ctx, &stream.execution)
            .map_err(|error| HumanInputError::Authority { error })?,
    )
}

pub(crate) fn close(
    ctx: &NativeServiceContext,
    stream: &HumanInputStream,
) -> Result<(), HumanInputError> {
    authorize(ctx, &stream.execution)?;
    crate::runtime::human_input::close(stream)
}
