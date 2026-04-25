//! Prompt host registration and async submission.
//!
//! This module re-exports the prompt system from [`bmux_plugin::prompt`],
//! which provides the process-global host channel.  Plugin code and the
//! attach loop both use the same channel via `bmux_plugin`.

pub use bmux_plugin::prompt::{
    PromptEvent, PromptField, PromptHostRequest, PromptOption, PromptPolicy, PromptRequest,
    PromptResponse, PromptSubmitError, PromptValidation, PromptValue, PromptWidth, register_host,
    request, request_with_events, submit, submit_with_events,
};
