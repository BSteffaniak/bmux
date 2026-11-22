//! The server instance: open drafts, and the worker that keeps the Bcode connection.

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::sync::PoisonError;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use bcode_session_bridge::models::BridgeEvent;
use bcode_session_bridge::models::bcode_feedback_models::{
    Facet, MAX_FACET_VALUE_BYTES, MAX_FACETS, RecordFeedbackRequest,
};
use bcode_session_bridge::{BridgeClient, BridgeEvents};
use bmux_plugin_sdk::PluginHostServices;
use tokio::sync::mpsc;
use tokio::task::JoinSet;

use crate::declaration::provides::feedback_commands;
use crate::declaration::requires::{notifications, recording_commands};
use crate::settings::Settings;
use crate::tools;

/// Most drafts open at once; opening another forgets the oldest.
const MAX_DRAFTS: usize = 16;
/// How long a draft waits for its text.
const DRAFT_LIFETIME: Duration = Duration::from_hours(1);
/// Facet naming when the person started giving the feedback, in Unix epoch milliseconds.
pub const OPENED_FACET: &str = "bmux.opened_epoch_ms";

/// Milliseconds since the Unix epoch.
#[must_use]
pub fn epoch_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
        })
}

/// Feedback the person started giving.
#[derive(Debug, Clone)]
pub struct Draft {
    opened_epoch_ms: u64,
    facets: Vec<feedback_commands::Facet>,
    opened: Instant,
}

/// A draft with its text, for the worker to send.
#[derive(Debug)]
pub struct Submission {
    draft: Draft,
    text: String,
}

/// Open drafts and the worker's queue.
pub struct Server {
    drafts: Mutex<Drafts>,
    jobs: mpsc::UnboundedSender<Submission>,
}

#[derive(Default)]
struct Drafts {
    next: u64,
    open: BTreeMap<u64, Draft>,
}

impl Server {
    /// A server and the receiving end of its queue, for [`run`].
    #[must_use]
    pub fn new() -> (Self, mpsc::UnboundedReceiver<Submission>) {
        let (jobs, received) = mpsc::unbounded_channel();
        (
            Self {
                drafts: Mutex::default(),
                jobs,
            },
            received,
        )
    }

    /// Open a draft; returns its id.
    pub fn open(&self, opened_epoch_ms: u64, facets: Vec<feedback_commands::Facet>) -> u64 {
        let mut drafts = self.drafts.lock().unwrap_or_else(PoisonError::into_inner);
        drafts
            .open
            .retain(|_, draft| draft.opened.elapsed() < DRAFT_LIFETIME);
        while drafts.open.len() >= MAX_DRAFTS {
            drafts.open.pop_first();
        }
        drafts.next += 1;
        let id = drafts.next;
        drafts.open.insert(
            id,
            Draft {
                opened_epoch_ms,
                facets,
                opened: Instant::now(),
            },
        );
        id
    }

    /// Queue `draft` with its text; `false` when the draft is unknown or expired.
    pub fn submit(&self, draft: u64, text: String) -> bool {
        let Some(draft) = self.take(draft) else {
            return false;
        };
        self.jobs.send(Submission { draft, text }).is_ok()
    }

    /// Forget `draft`; `false` when it is unknown or expired.
    pub fn discard(&self, draft: u64) -> bool {
        self.take(draft).is_some()
    }

    fn take(&self, draft: u64) -> Option<Draft> {
        self.drafts
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .open
            .remove(&draft)
            .filter(|draft| draft.opened.elapsed() < DRAFT_LIFETIME)
    }
}

/// Mark in the rolling recording the moment the person started giving feedback. A recording
/// that is off or failing never stops the feedback.
pub async fn mark_opened(host: &PluginHostServices, draft: u64, opened_epoch_ms: u64) {
    let payload = serde_json::to_vec(&serde_json::json!({
        "draft": draft,
        "opened_epoch_ms": opened_epoch_ms,
    }))
    .unwrap_or_default();
    let marked = recording_commands::v3::Client::remote(host.clone())
        .write_custom_event(
            None,
            None,
            "bmux.feedback".to_owned(),
            "feedback-opened".to_owned(),
            payload,
        )
        .await;
    if let Err(error) = marked {
        tracing::debug!(?error, "bmux.feedback: feedback moment not marked");
    }
}

/// The open connection to Bcode.
struct Connection {
    client: BridgeClient,
    events: BridgeEvents,
    runtime: String,
}

/// Send queued feedback to Bcode and answer the intake agent's tool calls, until the
/// activation ends.
pub async fn run(
    host: PluginHostServices,
    settings: Settings,
    mut jobs: mpsc::UnboundedReceiver<Submission>,
) {
    let mut connection: Option<Connection> = None;
    let mut calls = JoinSet::new();
    loop {
        tokio::select! {
            job = jobs.recv() => {
                let Some(job) = job else { break };
                submit(&host, &settings, &mut connection, job).await;
            }
            event = next_event(connection.as_mut()) => match event {
                Some(BridgeEvent::ToolInvoke(call)) => {
                    if let Some(open) = &connection {
                        calls.spawn(tools::answer(
                            host.clone(),
                            settings.clone(),
                            open.client.clone(),
                            call,
                        ));
                    }
                }
                Some(BridgeEvent::ToolsLost { reason }) => {
                    tracing::info!(%reason, "bmux.feedback: registering the recording tools again");
                    if let Some(open) = &connection
                        && let Err(error) = tools::register(&open.client, &settings).await
                    {
                        tracing::warn!(%error, "bmux.feedback: recording tools unavailable");
                    }
                }
                Some(_) => {}
                None => {
                    tracing::info!("bmux.feedback: the Bcode connection closed");
                    connection = None;
                }
            },
            Some(_) = calls.join_next(), if !calls.is_empty() => {}
        }
    }
}

async fn next_event(connection: Option<&mut Connection>) -> Option<BridgeEvent> {
    match connection {
        Some(open) => open.events.recv().await,
        None => std::future::pending().await,
    }
}

/// Connect to Bcode if needed and send `job`, reporting the outcome as a notification.
async fn submit(
    host: &PluginHostServices,
    settings: &Settings,
    connection: &mut Option<Connection>,
    job: Submission,
) {
    if connection.is_none() {
        match connect(settings).await {
            Ok(open) => *connection = Some(open),
            Err(error) => {
                // The text is not lost: it stays in the log.
                tracing::warn!(%error, text = %job.text, "bmux.feedback: feedback not sent");
                notify(host, "Feedback not sent", &error, Severity::Warning).await;
                return;
            }
        }
    }
    let Some(open) = connection.as_ref() else {
        return;
    };
    let item = RecordFeedbackRequest {
        app: crate::APP_ID.to_owned(),
        text: job.text.clone(),
        facets: facets(&job.draft),
    };
    match open
        .client
        .feedback_submit(item, Some(tools::INSTRUCTIONS.to_owned()), true)
        .await
    {
        Ok(submission) => {
            let id = submission.item.id;
            let body = submission.triage_error.map_or_else(
                || format!("Bcode is triaging it ({}).", open.runtime),
                |error| format!("Saved; triage did not start: {}", error.message),
            );
            notify(
                host,
                &format!("Feedback {id} saved"),
                &body,
                Severity::Success,
            )
            .await;
        }
        Err(error) => {
            tracing::warn!(%error, text = %job.text, "bmux.feedback: feedback not sent");
            notify(
                host,
                "Feedback not sent",
                &error.to_string(),
                Severity::Warning,
            )
            .await;
            if error.code().is_none() {
                // The transport failed: connect again next time.
                *connection = None;
            }
        }
    }
}

async fn connect(settings: &Settings) -> Result<Connection, String> {
    let attached = bcode_session_bridge_runtime::attach(&settings.runtime_options())
        .await
        .map_err(|error| error.to_string())?;
    let runtime = attached.runtime().label().to_owned();
    tracing::info!(
        runtime = %runtime,
        reason = %attached.reason(),
        "bmux.feedback: connected to Bcode"
    );
    tools::register(&attached.client, settings)
        .await
        .map_err(|error| format!("Bcode refused bmux's recording tools: {error}"))?;
    Ok(Connection {
        client: attached.client,
        events: attached.events,
        runtime,
    })
}

/// The draft's facts as Bcode facets, within Bcode's bounds, with the moment it was opened.
fn facets(draft: &Draft) -> Vec<Facet> {
    let mut facets = vec![Facet::new(OPENED_FACET, draft.opened_epoch_ms.to_string())];
    facets.extend(
        draft
            .facets
            .iter()
            .filter(|facet| !facet.value.is_empty())
            .take(MAX_FACETS - 1)
            .map(|facet| Facet {
                key: facet.key.clone(),
                value: truncate(&facet.value, MAX_FACET_VALUE_BYTES),
                sensitive: facet.sensitive,
            }),
    );
    facets
}

/// `text` cut to at most `max` bytes on a character boundary.
fn truncate(text: &str, max: usize) -> String {
    let mut end = text.len().min(max);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_owned()
}

type Severity = bmux_tab_bar_plugin_api::status_items_list::Severity;

async fn notify(host: &PluginHostServices, title: &str, body: &str, severity: Severity) {
    let notified = notifications::v1::Client::remote(host.clone())
        .notify(title.to_owned(), Some(body.to_owned()), severity, None)
        .await;
    if let Err(error) = notified {
        tracing::debug!(?error, "bmux.feedback: notification not shown");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wire_facet(key: &str, value: &str, sensitive: bool) -> feedback_commands::Facet {
        feedback_commands::Facet {
            key: key.to_owned(),
            value: value.to_owned(),
            sensitive,
        }
    }

    #[test]
    fn drafts_are_taken_once() {
        let (server, mut jobs) = Server::new();
        let first = server.open(1, Vec::new());
        let second = server.open(2, Vec::new());
        assert_ne!(first, second);
        assert!(server.discard(first));
        assert!(!server.discard(first));
        assert!(server.submit(second, "slow".to_owned()));
        assert!(!server.submit(second, "again".to_owned()));
        let job = jobs.try_recv().unwrap();
        assert_eq!(job.text, "slow");
        assert_eq!(job.draft.opened_epoch_ms, 2);
    }

    #[test]
    fn the_oldest_draft_goes_when_too_many_are_open() {
        let (server, _jobs) = Server::new();
        let first = server.open(0, Vec::new());
        for _ in 0..MAX_DRAFTS {
            server.open(0, Vec::new());
        }
        assert!(!server.discard(first));
    }

    #[test]
    fn facets_carry_the_moment_and_stay_within_bcode_bounds() {
        let draft = Draft {
            opened_epoch_ms: 42,
            facets: vec![
                wire_facet("bmux.pane_command", &"é".repeat(200), true),
                wire_facet("bmux.pane_name", "", false),
            ],
            opened: Instant::now(),
        };
        let facets = facets(&draft);
        assert_eq!(facets[0], Facet::new(OPENED_FACET, "42"));
        assert_eq!(facets.len(), 2, "empty values are left out");
        assert!(facets[1].sensitive);
        assert!(facets[1].value.len() <= MAX_FACET_VALUE_BYTES);
        assert!(facets[1].value.chars().all(|ch| ch == 'é'));
    }
}
