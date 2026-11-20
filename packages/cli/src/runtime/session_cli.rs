use bmux_client::ClientError;

use bmux_sessions_plugin_api::DestructiveOpErrorKind;

fn classify_destructive_op_error(error: &ClientError) -> DestructiveOpErrorKind {
    match error {
        ClientError::ServerError { code, message } => {
            bmux_sessions_plugin_api::classify_destructive_op_error(*code, message)
        }
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
