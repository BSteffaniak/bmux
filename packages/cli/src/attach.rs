use anyhow::Result;
use bmux_client::BmuxClient;
use tokio::sync::oneshot;

pub use crate::runtime::{
    AttachExitReason, AttachRunOutcome, PromptField, PromptOption, PromptPolicy, PromptRequest,
    PromptResponse, PromptSubmitError, PromptValue, PromptWidth,
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

/// Submit a prompt request and wait for its response.
///
/// # Errors
/// Returns an error if no prompt host is available or if host delivery fails.
pub async fn request_prompt(
    request: PromptRequest,
) -> std::result::Result<PromptResponse, PromptSubmitError> {
    crate::runtime::request_prompt_response(request).await
}
