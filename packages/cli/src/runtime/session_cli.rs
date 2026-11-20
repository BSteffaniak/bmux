use bmux_client::ClientError;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum DestructiveOpErrorKind {
    SessionPolicyDenied,
    ForceLocalUnauthorized,
    NotFound,
    Other,
}

pub(super) fn classify_destructive_op_error(error: &ClientError) -> DestructiveOpErrorKind {
    match error {
        ClientError::ServerError { code, message } => match code {
            bmux_ipc::ErrorCode::InvalidRequest
                if message.contains("session policy denied for this operation") =>
            {
                DestructiveOpErrorKind::SessionPolicyDenied
            }
            bmux_ipc::ErrorCode::InvalidRequest
                if message
                    .contains("force-local is only allowed for the server control principal") =>
            {
                DestructiveOpErrorKind::ForceLocalUnauthorized
            }
            bmux_ipc::ErrorCode::NotFound => DestructiveOpErrorKind::NotFound,
            _ => DestructiveOpErrorKind::Other,
        },
        _ => DestructiveOpErrorKind::Other,
    }
}

pub(super) fn attach_quit_failure_status(error: &ClientError) -> &'static str {
    match classify_destructive_op_error(error) {
        DestructiveOpErrorKind::SessionPolicyDenied => "quit blocked by session policy",
        DestructiveOpErrorKind::ForceLocalUnauthorized => {
            "quit requires server control principal for --force-local"
        }
        DestructiveOpErrorKind::NotFound => "quit failed: session not found",
        DestructiveOpErrorKind::Other => "quit failed",
    }
}
