//! The server instance: open drafts, and the worker that keeps the Bcode connection.

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::sync::PoisonError;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use bcode_session_bridge::models::BridgeEvent;
use bcode_session_bridge::models::bcode_feedback_models::{
    Facet, MAX_FACET_VALUE_BYTES, MAX_FACETS, RecordFeedbackRequest, TriageDispatchState,
};
use bcode_session_bridge::{BridgeClient, BridgeEvents, FeedbackSubmission};
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
            let notice = Notice::of(&submission, &open.runtime);
            if notice.severity != Severity::Success {
                tracing::info!(
                    id = %submission.item.id,
                    title = %notice.title,
                    body = %notice.body,
                    "bmux.feedback: feedback saved, triage not started"
                );
            }
            notify(host, &notice.title, &notice.body, notice.severity).await;
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

/// What the person is told about saved feedback: saved, and whether triage started, is
/// queued for Bcode to retry on its own, or is blocked until they ask again.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Notice {
    title: String,
    body: String,
    severity: Severity,
}

impl Notice {
    fn of(submission: &FeedbackSubmission, runtime: &str) -> Self {
        let id = submission.item.id;
        let dispatch = submission.item.triage_dispatch.as_ref();
        let error = submission
            .triage_error
            .as_ref()
            .map(|error| error.message.as_str());
        let state = dispatch.map(|dispatch| dispatch.state);
        match (state, &submission.intake, error) {
            (Some(TriageDispatchState::Started | TriageDispatchState::Completed), ..)
            | (None, Some(_), None) => Self {
                title: format!("Feedback {id} saved"),
                body: format!("Triage started: Bcode is triaging it ({runtime})."),
                severity: Severity::Success,
            },
            (Some(TriageDispatchState::Queued), ..) => Self {
                title: format!("Feedback {id} saved · triage queued"),
                body: error.map_or_else(
                    || "Bcode will triage it as soon as it can.".to_owned(),
                    |error| {
                        format!("Bcode keeps retrying on its own, even if bmux closes. Last error: {error}")
                    },
                ),
                severity: Severity::Info,
            },
            (Some(TriageDispatchState::Blocked), ..) => Self {
                title: format!("Feedback {id} saved · triage blocked"),
                body: format!(
                    "{}. Run `bcode feedback triage {id}` to try again.",
                    dispatch
                        .and_then(|dispatch| dispatch.blocked_reason.as_deref())
                        .or(error)
                        .unwrap_or("Bcode stopped trying")
                        .trim_end_matches('.')
                ),
                severity: Severity::Warning,
            },
            (Some(TriageDispatchState::Skipped), ..) | (None, None, None) => Self {
                title: format!("Feedback {id} saved"),
                body: "Saved without triage.".to_owned(),
                severity: Severity::Success,
            },
            // A Bcode that predates durable triage: nothing retries it.
            (None, _, Some(error)) => Self {
                title: format!("Feedback {id} saved · triage blocked"),
                body: format!(
                    "Triage did not start: {error}. Run `bcode feedback triage {id}` to try again."
                ),
                severity: Severity::Warning,
            },
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

    mod notices {
        use bcode_session_bridge::models::bcode_feedback_models::{
            FeedbackId, FeedbackItem, FeedbackStatus, TriageDispatch, TriageState,
        };
        use bcode_session_bridge::models::{BridgeError, FeedbackIntake};

        use super::*;

        fn submission(
            state: Option<TriageDispatchState>,
            intake: bool,
            error: Option<&str>,
        ) -> FeedbackSubmission {
            FeedbackSubmission {
                item: FeedbackItem {
                    id: FeedbackId(3),
                    app: "bmux".into(),
                    text: "the pane froze".into(),
                    facets: Vec::new(),
                    created_at_ms: 0,
                    updated_at_ms: 0,
                    triage_state: TriageState::Pending,
                    triage: None,
                    status: FeedbackStatus::Open,
                    status_change: None,
                    links: Vec::new(),
                    attachments: Vec::new(),
                    triage_dispatch: state.map(|state| TriageDispatch {
                        state,
                        generation: 1,
                        attempts: u32::from(error.is_some()),
                        requested_at_ms: 0,
                        updated_at_ms: 0,
                        next_attempt_at_ms: None,
                        last_error: error.map(str::to_owned),
                        blocked_reason: (state == TriageDispatchState::Blocked)
                            .then(|| "the intake session refused the turn".to_owned()),
                        session_id: None,
                        turn_id: None,
                    }),
                },
                intake: intake.then(|| FeedbackIntake {
                    session_id: "s".into(),
                    turn_id: "t".into(),
                }),
                triage_error: error.map(|message| BridgeError {
                    code: "feedback_triage_queued".into(),
                    message: message.into(),
                }),
            }
        }

        #[test]
        fn started_queued_blocked_and_saved_read_differently() {
            let started = Notice::of(
                &submission(Some(TriageDispatchState::Started), true, None),
                "embedded Bcode",
            );
            assert_eq!(started.title, "Feedback fb-3 saved");
            assert!(started.body.starts_with("Triage started"), "{started:?}");
            assert_eq!(started.severity, Severity::Success);

            let queued = Notice::of(
                &submission(
                    Some(TriageDispatchState::Queued),
                    false,
                    Some("model unavailable"),
                ),
                "embedded Bcode",
            );
            assert_eq!(queued.title, "Feedback fb-3 saved · triage queued");
            assert!(queued.body.contains("model unavailable"), "{queued:?}");
            assert!(queued.body.contains("even if bmux closes"), "{queued:?}");
            assert_eq!(queued.severity, Severity::Info);

            let blocked = Notice::of(
                &submission(Some(TriageDispatchState::Blocked), false, Some("refused")),
                "embedded Bcode",
            );
            assert_eq!(blocked.title, "Feedback fb-3 saved · triage blocked");
            assert!(
                blocked.body.contains("the intake session refused the turn"),
                "{blocked:?}"
            );
            assert!(
                blocked.body.contains("bcode feedback triage fb-3"),
                "{blocked:?}"
            );
            assert_eq!(blocked.severity, Severity::Warning);

            let saved = Notice::of(
                &submission(Some(TriageDispatchState::Skipped), false, None),
                "embedded Bcode",
            );
            assert_eq!(saved.title, "Feedback fb-3 saved");
            assert_eq!(saved.body, "Saved without triage.");
        }

        #[test]
        fn a_bcode_without_durable_triage_reports_a_failed_start_as_blocked() {
            let legacy = Notice::of(&submission(None, false, Some("no session")), "installed");
            assert_eq!(legacy.title, "Feedback fb-3 saved · triage blocked");
            assert!(legacy.body.contains("no session"), "{legacy:?}");
            let started = Notice::of(&submission(None, true, None), "installed");
            assert_eq!(started.severity, Severity::Success);
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
