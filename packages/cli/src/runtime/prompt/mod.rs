#![allow(dead_code)]
#![allow(clippy::missing_const_for_fn)]

use std::error::Error;
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

use tokio::sync::{mpsc, oneshot};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptPolicy {
    Enqueue,
    ReplaceActive,
    RejectIfBusy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PromptWidth {
    pub min: u16,
    pub max: u16,
}

impl Default for PromptWidth {
    fn default() -> Self {
        Self { min: 40, max: 92 }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptOption {
    pub value: String,
    pub label: String,
}

impl PromptOption {
    #[must_use]
    pub fn new(value: impl Into<String>, label: impl Into<String>) -> Self {
        Self {
            value: value.into(),
            label: label.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PromptField {
    Confirm {
        default: bool,
        yes_label: String,
        no_label: String,
    },
    TextInput {
        initial_value: String,
        placeholder: Option<String>,
        required: bool,
    },
    SingleSelect {
        options: Vec<PromptOption>,
        default_index: usize,
    },
    MultiToggle {
        options: Vec<PromptOption>,
        default_indices: Vec<usize>,
        min_selected: usize,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptRequest {
    pub id: u64,
    pub title: String,
    pub message: Option<String>,
    pub submit_label: String,
    pub cancel_label: String,
    pub esc_cancels: bool,
    pub policy: PromptPolicy,
    pub width: PromptWidth,
    pub field: PromptField,
}

impl PromptRequest {
    #[must_use]
    pub fn confirm(title: impl Into<String>) -> Self {
        Self {
            id: next_prompt_id(),
            title: title.into(),
            message: None,
            submit_label: "Submit".to_string(),
            cancel_label: "Cancel".to_string(),
            esc_cancels: true,
            policy: PromptPolicy::Enqueue,
            width: PromptWidth::default(),
            field: PromptField::Confirm {
                default: false,
                yes_label: "Yes".to_string(),
                no_label: "No".to_string(),
            },
        }
    }

    #[must_use]
    pub fn text_input(title: impl Into<String>) -> Self {
        Self {
            id: next_prompt_id(),
            title: title.into(),
            message: None,
            submit_label: "Submit".to_string(),
            cancel_label: "Cancel".to_string(),
            esc_cancels: true,
            policy: PromptPolicy::Enqueue,
            width: PromptWidth::default(),
            field: PromptField::TextInput {
                initial_value: String::new(),
                placeholder: None,
                required: false,
            },
        }
    }

    #[must_use]
    pub fn single_select(title: impl Into<String>, options: Vec<PromptOption>) -> Self {
        Self {
            id: next_prompt_id(),
            title: title.into(),
            message: None,
            submit_label: "Select".to_string(),
            cancel_label: "Cancel".to_string(),
            esc_cancels: true,
            policy: PromptPolicy::Enqueue,
            width: PromptWidth::default(),
            field: PromptField::SingleSelect {
                options,
                default_index: 0,
            },
        }
    }

    #[must_use]
    pub fn multi_toggle(title: impl Into<String>, options: Vec<PromptOption>) -> Self {
        Self {
            id: next_prompt_id(),
            title: title.into(),
            message: None,
            submit_label: "Apply".to_string(),
            cancel_label: "Cancel".to_string(),
            esc_cancels: true,
            policy: PromptPolicy::Enqueue,
            width: PromptWidth::default(),
            field: PromptField::MultiToggle {
                options,
                default_indices: Vec::new(),
                min_selected: 0,
            },
        }
    }

    #[must_use]
    pub fn message(mut self, message: impl Into<String>) -> Self {
        self.message = Some(message.into());
        self
    }

    #[must_use]
    pub fn submit_label(mut self, label: impl Into<String>) -> Self {
        self.submit_label = label.into();
        self
    }

    #[must_use]
    pub fn cancel_label(mut self, label: impl Into<String>) -> Self {
        self.cancel_label = label.into();
        self
    }

    #[must_use]
    pub fn esc_cancels(mut self, enabled: bool) -> Self {
        self.esc_cancels = enabled;
        self
    }

    #[must_use]
    pub fn policy(mut self, policy: PromptPolicy) -> Self {
        self.policy = policy;
        self
    }

    #[must_use]
    pub fn width_range(mut self, min: u16, max: u16) -> Self {
        let normalized = if min <= max {
            PromptWidth { min, max }
        } else {
            PromptWidth { min: max, max: min }
        };
        self.width = normalized;
        self
    }

    #[must_use]
    pub fn confirm_default(mut self, default: bool) -> Self {
        if let PromptField::Confirm {
            default: slot_default,
            ..
        } = &mut self.field
        {
            *slot_default = default;
        }
        self
    }

    #[must_use]
    pub fn confirm_labels(mut self, yes: impl Into<String>, no: impl Into<String>) -> Self {
        if let PromptField::Confirm {
            yes_label,
            no_label,
            ..
        } = &mut self.field
        {
            *yes_label = yes.into();
            *no_label = no.into();
        }
        self
    }

    #[must_use]
    pub fn input_initial(mut self, value: impl Into<String>) -> Self {
        if let PromptField::TextInput { initial_value, .. } = &mut self.field {
            *initial_value = value.into();
        }
        self
    }

    #[must_use]
    pub fn input_placeholder(mut self, value: impl Into<String>) -> Self {
        if let PromptField::TextInput { placeholder, .. } = &mut self.field {
            *placeholder = Some(value.into());
        }
        self
    }

    #[must_use]
    pub fn input_required(mut self, required: bool) -> Self {
        if let PromptField::TextInput {
            required: slot_required,
            ..
        } = &mut self.field
        {
            *slot_required = required;
        }
        self
    }

    #[must_use]
    pub fn single_default_index(mut self, index: usize) -> Self {
        if let PromptField::SingleSelect { default_index, .. } = &mut self.field {
            *default_index = index;
        }
        self
    }

    #[must_use]
    pub fn multi_defaults(mut self, indices: Vec<usize>) -> Self {
        if let PromptField::MultiToggle {
            default_indices, ..
        } = &mut self.field
        {
            *default_indices = indices;
        }
        self
    }

    #[must_use]
    pub fn multi_min_selected(mut self, min_selected: usize) -> Self {
        if let PromptField::MultiToggle {
            min_selected: slot_min,
            ..
        } = &mut self.field
        {
            *slot_min = min_selected;
        }
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PromptValue {
    Confirm(bool),
    Text(String),
    Single(String),
    Multi(Vec<String>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PromptResponse {
    Submitted(PromptValue),
    Cancelled,
    RejectedBusy,
}

impl PromptResponse {
    #[must_use]
    pub fn submitted_value(&self) -> Option<&PromptValue> {
        if let Self::Submitted(value) = self {
            Some(value)
        } else {
            None
        }
    }
}

#[derive(Debug)]
pub struct PromptHostRequest {
    pub request: PromptRequest,
    pub response_tx: oneshot::Sender<PromptResponse>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptSubmitError {
    HostUnavailable,
    HostDisconnected,
}

impl fmt::Display for PromptSubmitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::HostUnavailable => f.write_str("prompt host unavailable"),
            Self::HostDisconnected => f.write_str("prompt host disconnected"),
        }
    }
}

impl Error for PromptSubmitError {}

#[derive(Clone)]
struct PromptHostRegistration {
    id: u64,
    sender: mpsc::UnboundedSender<PromptHostRequest>,
}

static HOST_REGISTRY: OnceLock<Mutex<Option<PromptHostRegistration>>> = OnceLock::new();
static HOST_REGISTRATION_SEQUENCE: AtomicU64 = AtomicU64::new(1);
static PROMPT_REQUEST_SEQUENCE: AtomicU64 = AtomicU64::new(1);

fn host_registry() -> &'static Mutex<Option<PromptHostRegistration>> {
    HOST_REGISTRY.get_or_init(|| Mutex::new(None))
}

fn next_prompt_id() -> u64 {
    PROMPT_REQUEST_SEQUENCE.fetch_add(1, Ordering::Relaxed)
}

#[derive(Debug)]
pub struct PromptHostGuard {
    id: u64,
}

impl Drop for PromptHostGuard {
    fn drop(&mut self) {
        if let Ok(mut slot) = host_registry().lock()
            && slot
                .as_ref()
                .is_some_and(|registration| registration.id == self.id)
        {
            *slot = None;
        }
    }
}

pub fn register_host(sender: mpsc::UnboundedSender<PromptHostRequest>) -> PromptHostGuard {
    let id = HOST_REGISTRATION_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    if let Ok(mut slot) = host_registry().lock() {
        *slot = Some(PromptHostRegistration { id, sender });
    }
    PromptHostGuard { id }
}

pub fn submit(
    request: PromptRequest,
) -> std::result::Result<oneshot::Receiver<PromptResponse>, PromptSubmitError> {
    let guard = host_registry()
        .lock()
        .map_err(|_| PromptSubmitError::HostDisconnected)?;
    let sender = guard
        .as_ref()
        .map(|registration| registration.sender.clone())
        .ok_or(PromptSubmitError::HostUnavailable)?;
    drop(guard);

    let (response_tx, response_rx) = oneshot::channel();
    sender
        .send(PromptHostRequest {
            request,
            response_tx,
        })
        .map_err(|_| PromptSubmitError::HostDisconnected)?;
    Ok(response_rx)
}

pub async fn request(
    request: PromptRequest,
) -> std::result::Result<PromptResponse, PromptSubmitError> {
    let response_rx = submit(request)?;
    response_rx
        .await
        .map_err(|_| PromptSubmitError::HostDisconnected)
}

#[cfg(test)]
mod tests {
    use super::{PromptRequest, PromptResponse, PromptSubmitError, register_host, request, submit};

    #[tokio::test]
    #[serial_test::serial]
    async fn request_fails_when_no_host_is_registered() {
        let response = request(PromptRequest::confirm("missing host")).await;
        assert_eq!(response, Err(PromptSubmitError::HostUnavailable));
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn request_routes_through_registered_host() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let _guard = register_host(tx);

        let client_task =
            tokio::spawn(async { request(PromptRequest::confirm("quit session?")).await });

        let host_request = rx.recv().await.expect("host should receive request");
        assert_eq!(host_request.request.title, "quit session?");
        host_request
            .response_tx
            .send(PromptResponse::Cancelled)
            .expect("host should send response");

        let response = client_task
            .await
            .expect("request task should complete")
            .expect("request should resolve");
        assert_eq!(response, PromptResponse::Cancelled);
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn dropping_host_guard_unregisters_the_host() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let guard = register_host(tx);
        drop(guard);

        let receiver = submit(PromptRequest::confirm("hello"));
        assert!(matches!(receiver, Err(PromptSubmitError::HostUnavailable)));
    }
}
