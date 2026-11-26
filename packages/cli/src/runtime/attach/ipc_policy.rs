//! The attach client's one policy for failed server requests.
//!
//! Every IPC-carrying step of the interactive attach loop runs through
//! [`attach_try!`], which hands a failure to [`absorb_attach_failure`]. That is
//! the only place deciding what a failure does to the attachment:
//!
//! - [`AttachIpcFailure::Gone`]: the attached runtime is gone; the attachment
//!   ends as [`AttachExitReason::StreamClosed`].
//! - [`AttachIpcFailure::Transient`]: the server is slow; the loop keeps
//!   running, says so in the status line and resyncs the view. On a link
//!   whose [`super::state::LinkLiveness`] presumes it lost after enough of
//!   them (a remote one), they become [`AttachIpcFailure::ConnectionLost`].
//! - [`AttachIpcFailure::ConnectionLost`]: the attach loop stops as
//!   [`AttachExitReason::ConnectionLost`]; an attachment that can reconnect
//!   does so in place (see `reconnect`), otherwise it ends cleanly.
//! - [`AttachIpcFailure::Rejected`] / [`AttachIpcFailure::Protocol`]: shown in
//!   the status line; the loop keeps running. Resyncs refused
//!   [`super::state::AttachLinkHealth::STALL_AFTER_REJECTED_RESYNCS`] times in
//!   a row stop being retried until the user asks (the connection banner).
//! - [`AttachIpcFailure::Local`]: a failure on this side (not the server's):
//!   stays fatal.

use super::state::{AttachExitReason, AttachViewState};
use crate::connection::ReadableClientError;
use bmux_client::{ClientError, ErrorClass};
use bmux_plugin_sdk::{TypedDispatchClientError, TypedServiceClientError};
use std::time::{Duration, Instant};

/// How long a policy status message stays up once presented.
const ATTACH_FAILURE_STATUS_TTL: Duration = Duration::from_secs(3);

/// What a failed server request means for the attachment.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AttachIpcFailure {
    /// The attached session runtime or the attach stream is gone.
    Gone,
    /// The server did not answer in time or was briefly unavailable; the
    /// connection is fine.
    Transient,
    /// The connection to the server failed.
    ConnectionLost,
    /// The server refused the request.
    Rejected,
    /// The peers disagree about the shape of a request or response.
    Protocol,
    /// The failure is on this side, not the server's.
    Local,
}

/// The attach step a failure happened in: names it in status messages and
/// says whether the user's input was lost with it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AttachOp {
    ServerEvent,
    ContextSwitch,
    FollowTarget,
    SessionRecovery,
    Resize,
    LayoutRefresh,
    Resync,
    OutputDrain,
    Input,
    Action,
    InputBarriers,
    Scrollback,
}

impl AttachOp {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::ServerEvent => "server event",
            Self::ContextSwitch => "context switch",
            Self::FollowTarget => "follow",
            Self::SessionRecovery => "session recovery",
            Self::Resize => "resize",
            Self::LayoutRefresh => "layout refresh",
            Self::Resync => "resync",
            Self::OutputDrain => "pane output",
            Self::Input => "input",
            Self::Action => "action",
            Self::InputBarriers => "input sync",
            Self::Scrollback => "scrollback",
        }
    }

    /// Whether a failure of this step drops input the user typed.
    #[must_use]
    pub const fn drops_input(self) -> bool {
        matches!(self, Self::Input)
    }
}

impl std::fmt::Display for AttachOp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
}

/// What the attach loop does after a failure.
#[must_use]
#[derive(Debug)]
pub enum AttachFailureDisposition {
    /// Keep running; the policy already updated the view state.
    Continue,
    /// Stop the attachment cleanly for this reason.
    Exit(AttachExitReason),
    /// Stop with this error.
    Fatal(anyhow::Error),
}

/// Classify a failed client request.
#[must_use]
pub fn classify_attach_ipc_error(error: &ClientError) -> AttachIpcFailure {
    if super::runtime::is_attach_stream_closed_error(error)
        || super::runtime::is_attach_not_attached_runtime_error(error)
    {
        return AttachIpcFailure::Gone;
    }
    match error.class() {
        ErrorClass::Transient => AttachIpcFailure::Transient,
        ErrorClass::ConnectionLost => AttachIpcFailure::ConnectionLost,
        ErrorClass::Rejected => AttachIpcFailure::Rejected,
        ErrorClass::Protocol => AttachIpcFailure::Protocol,
        ErrorClass::Local => AttachIpcFailure::Local,
    }
}

/// Call `inspect` with the client error inside `error`, in whichever form the
/// step reported it (also behind `context` layers or as another error's
/// source).
fn with_client_error<R>(
    error: &anyhow::Error,
    inspect: impl FnOnce(&ClientError) -> R,
) -> Option<R> {
    for cause in error.chain() {
        if let Some(client_error) = cause.downcast_ref::<ClientError>() {
            return Some(inspect(client_error));
        }
        if let Some(readable) = cause.downcast_ref::<ReadableClientError>() {
            return Some(inspect(readable.client_error()));
        }
        if let Some(typed) = cause.downcast_ref::<TypedServiceClientError>() {
            return Some(inspect(&ClientError::from_typed_service_error("", typed)));
        }
        if let Some(typed) = cause.downcast_ref::<TypedDispatchClientError>() {
            return Some(inspect(&ClientError::from_typed_dispatch_error("", typed)));
        }
    }
    None
}

/// Classify the failure of one attach step.
///
/// A client error anywhere in it decides; otherwise an I/O error means this
/// side failed (the terminal). A failure with neither came from a step that
/// talks to the server without a typed error (an invalid action, an
/// unexpected reply): reported, never fatal.
#[must_use]
pub fn classify_attach_error(error: &anyhow::Error) -> AttachIpcFailure {
    if let Some(failure) = with_client_error(error, classify_attach_ipc_error) {
        return failure;
    }
    if error
        .chain()
        .any(<dyn std::error::Error>::is::<std::io::Error>)
    {
        return AttachIpcFailure::Local;
    }
    AttachIpcFailure::Rejected
}

/// The text that says what went wrong, without the step labels added around it.
fn failure_text(error: &anyhow::Error) -> String {
    with_client_error(error, |client_error| match client_error {
        ClientError::ServerError { message, .. } => message.clone(),
        other => other.to_string(),
    })
    .unwrap_or_else(|| error.root_cause().to_string())
}

/// Apply the attach failure policy to the failure of step `op`.
pub fn absorb_attach_failure(
    view_state: &mut AttachViewState,
    op: AttachOp,
    error: anyhow::Error,
) -> AttachFailureDisposition {
    // A step may name a more specific one it failed in.
    let op = error.downcast_ref::<AttachOp>().copied().unwrap_or(op);
    let failure = classify_attach_error(&error);
    if op == AttachOp::Resync {
        if failure == AttachIpcFailure::Rejected {
            view_state
                .link
                .note_resync_rejected(format!("{op} failed: {}", failure_text(&error)));
        } else {
            view_state.link.note_resync_not_rejected();
        }
    }
    match failure {
        AttachIpcFailure::Gone => {
            tracing::info!(op = op.label(), error = %format!("{error:#}"), "attach.failure.gone");
            AttachFailureDisposition::Exit(AttachExitReason::StreamClosed)
        }
        AttachIpcFailure::ConnectionLost => {
            tracing::warn!(op = op.label(), error = %format!("{error:#}"), "attach.failure.connection_lost");
            AttachFailureDisposition::Exit(AttachExitReason::ConnectionLost)
        }
        AttachIpcFailure::Local => AttachFailureDisposition::Fatal(error),
        AttachIpcFailure::Transient => {
            // The runtime's clock, so a paused test clock drives the window.
            let now = tokio::time::Instant::now().into_std();
            let in_a_row = view_state.link.note_transient(now);
            if view_state.link.presumed_lost(now) {
                // A network path that died silently: reconnect in place.
                tracing::warn!(op = op.label(), in_a_row, liveness = ?view_state.link.liveness, error = %format!("{error:#}"), "attach.failure.presumed_lost");
                return AttachFailureDisposition::Exit(AttachExitReason::ConnectionLost);
            }
            view_state.link.request_resync();
            let consequence = if op.drops_input() {
                "input was not delivered"
            } else {
                "retrying sync"
            };
            let message = if view_state.link.unresponsive() {
                format!("server unresponsive: {op} timed out ({in_a_row} in a row); {consequence}")
            } else {
                format!("server slow: {op} timed out; {consequence}")
            };
            tracing::warn!(op = op.label(), in_a_row, error = %format!("{error:#}"), "attach.failure.transient");
            view_state.set_transient_status(message, Instant::now(), ATTACH_FAILURE_STATUS_TTL);
            AttachFailureDisposition::Continue
        }
        AttachIpcFailure::Rejected | AttachIpcFailure::Protocol => {
            if failure == AttachIpcFailure::Protocol {
                // The peers disagree about what was said: what this view
                // holds may not be what the server meant.
                view_state.link.request_resync();
            }
            let suffix = if op.drops_input() {
                " (input was not delivered)"
            } else {
                ""
            };
            tracing::warn!(op = op.label(), ?failure, error = %format!("{error:#}"), "attach.failure.reported");
            view_state.set_transient_status(
                format!("{op} failed: {}{suffix}", failure_text(&error)),
                Instant::now(),
                ATTACH_FAILURE_STATUS_TTL,
            );
            AttachFailureDisposition::Continue
        }
    }
}

/// Run one IPC-carrying step of the attach loop under the attach failure
/// policy: `Some(value)` on success, `None` when the failure was absorbed
/// (the loop keeps running), and otherwise it breaks out of `$label` with the
/// exit reason or returns the fatal error.
///
/// `attach_try!('attach_loop, view_state, exit_reason, AttachOp::Resize, call.await)`
macro_rules! attach_try {
    ($label:lifetime, $view_state:expr, $exit_reason:ident, $op:expr, $call:expr $(,)?) => {
        match $call {
            Ok(value) => Some(value),
            Err(error) => match $crate::runtime::attach::ipc_policy::absorb_attach_failure(
                &mut $view_state,
                $op,
                ::anyhow::Error::from(error),
            ) {
                $crate::runtime::attach::ipc_policy::AttachFailureDisposition::Continue => None,
                $crate::runtime::attach::ipc_policy::AttachFailureDisposition::Exit(reason) => {
                    $exit_reason = reason;
                    break $label;
                }
                $crate::runtime::attach::ipc_policy::AttachFailureDisposition::Fatal(error) => {
                    return Err(error);
                }
            },
        }
    };
}
pub(crate) use attach_try;
