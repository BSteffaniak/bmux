use anyhow::Result;
use bmux_client::BmuxClient;
use tokio::sync::oneshot;

pub use crate::runtime::{
    ActionDispatchError, ActionDispatchRequest, AttachExitReason, AttachRunOutcome, PromptEvent,
    PromptField, PromptOption, PromptPolicy, PromptRequest, PromptResponse, PromptSubmitError,
    PromptValidation, PromptValue, PromptWidth,
};

/// Run the attach TUI using an already-connected client.
///
/// # Errors
/// Returns an error when attach setup or runtime processing fails.
pub async fn run_with_client(
    client: BmuxClient,
    target: Option<&str>,
    follow: Option<&str>,
    global: bool,
) -> Result<AttachRunOutcome> {
    crate::runtime::run_attach_with_client(client, target, follow, global).await
}

/// Submit a prompt request to the active attach host.
///
/// # Errors
/// Returns an error if no prompt host is available or if host delivery fails.
pub fn submit_prompt(
    request: PromptRequest,
) -> std::result::Result<oneshot::Receiver<PromptResponse>, PromptSubmitError> {
    crate::runtime::submit_prompt_request(request)
}

/// Submit a prompt request and receive live prompt events.
///
/// # Errors
/// Returns an error if no prompt host is available or if host delivery fails.
pub fn submit_prompt_with_events(
    request: PromptRequest,
) -> std::result::Result<
    (
        oneshot::Receiver<PromptResponse>,
        tokio::sync::mpsc::UnboundedReceiver<PromptEvent>,
    ),
    PromptSubmitError,
> {
    crate::runtime::submit_prompt_request_with_events(request)
}

/// Submit a prompt request and wait for its response.
///
/// # Errors
/// Returns an error if no prompt host is available or if host delivery fails.
pub async fn request_prompt(
    request: PromptRequest,
) -> std::result::Result<PromptResponse, PromptSubmitError> {
    crate::runtime::request_prompt_response(request).await
}

/// Submit a prompt request, wait for its response, and return live events.
///
/// # Errors
/// Returns an error if no prompt host is available or if host delivery fails.
pub async fn request_prompt_with_events(
    request: PromptRequest,
) -> std::result::Result<
    (
        PromptResponse,
        tokio::sync::mpsc::UnboundedReceiver<PromptEvent>,
    ),
    PromptSubmitError,
> {
    crate::runtime::request_prompt_response_with_events(request).await
}

/// Dispatch a runtime action string to the attach loop.
///
/// The action string uses the same format as keybinding action values
/// (e.g. `"focus_next_pane"`, `"plugin:bmux.windows:goto-window 1"`).
///
/// This is intended for async plugin code that has collected parameters
/// (e.g. via prompts) and needs to execute an action with those values.
///
/// # Errors
/// Returns an error if no dispatch host is registered or the channel is closed.
pub fn dispatch_action(action: impl Into<String>) -> std::result::Result<(), ActionDispatchError> {
    crate::runtime::dispatch_action(action)
}
