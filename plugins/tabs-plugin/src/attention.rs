//! What deserves the user's look in each tab, whether anyone has looked, and
//! the one state every view shows for the tab.
//!
//! The status board observes each tab's signals once per refresh. Three
//! things that happen in a tab are recorded as numbered events, once each:
//!
//! * **finished**: its agent went from working to idle and stayed idle for
//!   [`FINISH_SETTLE_MS`], so a pause between steps, or before a question,
//!   is not a finish;
//! * **attention**: a pane rang the bell or sent a notification;
//! * **failed**: a command exited non-zero.
//!
//! A tab is *looked at* while any client shows it in a terminal that has
//! focus; looking at it marks every event so far seen, and an event that
//! happens while it is looked at is seen at once. Seen marks are shared by
//! every client.
//!
//! The tab's state, most pressing first: needs input (its agent asks, or an
//! unseen bell or notification), failed (an unseen failure, or any while the
//! failed command's output stands, by [`FailedClears`]), done (an unseen
//! finish while the agent stays idle), working, stopped after observed work, idle.

use std::collections::{HashMap, HashSet};

use bmux_tabs_plugin_api::tabs_status::{AgentActivity, TabState};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// How long an agent must stay idle after working before that counts as
/// finishing.
pub const FINISH_SETTLE_MS: u64 = 1_500;
/// Output within this long, while a command runs, counts as working.
pub const WORKING_WINDOW_MS: u64 = 3_000;

/// When a failed state ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FailedClears {
    /// Once anyone looks at the tab (or another command starts).
    #[default]
    WhenSeen,
    /// When another command starts, whether or not anyone looked.
    OnNextCommand,
}

impl FailedClears {
    /// The tabs plugin's `failed_clears` setting: `"when_seen"` (the
    /// default) or `"on_next_command"`.
    ///
    /// # Errors
    /// Returns a message for any other value.
    pub fn parse(settings: Option<&toml::Value>) -> Result<Self, String> {
        settings
            .and_then(|settings| settings.get("failed_clears"))
            .map_or_else(
                || Ok(Self::default()),
                |value| match value.as_str() {
                    Some("when_seen") => Ok(Self::WhenSeen),
                    Some("on_next_command") => Ok(Self::OnNextCommand),
                    _ => Err(format!(
                        "bmux.tabs failed_clears must be \"when_seen\" or \"on_next_command\", got {value}"
                    )),
                },
            )
    }
}

/// The plugin storage key the book is saved under.
pub const STORAGE_KEY: &str = "attention";
/// The saved book's format.
const FORMAT_VERSION: u32 = 1;

/// The book as saved: its format version, then its content.
#[derive(Debug, Serialize, Deserialize)]
struct Saved {
    version: u32,
    #[serde(default)]
    book: AttentionBook,
}

/// What loading a saved book found.
#[derive(Debug, PartialEq, Eq)]
pub enum Loaded {
    /// Nothing saved yet.
    Nothing,
    /// A book this build reads.
    Book(AttentionBook),
    /// A book in a format this build does not know, left as it is: the
    /// board neither uses nor overwrites it.
    Unsupported,
}

impl Loaded {
    /// Read a saved book.
    #[must_use]
    pub fn from_bytes(bytes: Option<&[u8]>) -> Self {
        let Some(bytes) = bytes else {
            return Self::Nothing;
        };
        match serde_json::from_slice::<Saved>(bytes) {
            Ok(saved) if saved.version == FORMAT_VERSION => Self::Book(saved.book),
            _ => Self::Unsupported,
        }
    }
}

impl AttentionBook {
    /// The book as saved.
    ///
    /// # Errors
    /// Returns a message when it cannot be encoded.
    pub fn to_bytes(&self) -> Result<Vec<u8>, String> {
        #[derive(Serialize)]
        struct SavedRef<'a> {
            version: u32,
            book: &'a AttentionBook,
        }
        serde_json::to_vec(&SavedRef {
            version: FORMAT_VERSION,
            book: self,
        })
        .map_err(|error| error.to_string())
    }
}

/// One recorded event: its order among all events, and when it happened.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Event {
    pub seq: u64,
    pub at_ms: u64,
}

/// A tab's latest event of each kind and how far it has been seen.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TabAttention {
    #[serde(default)]
    pub finished: Option<Event>,
    #[serde(default)]
    pub attention: Option<Event>,
    #[serde(default)]
    pub failed: Option<Event>,
    /// Every event up to this sequence number has been seen.
    #[serde(default)]
    pub seen_through: u64,
}

impl TabAttention {
    const fn unseen(&self, event: Option<Event>) -> Option<Event> {
        match event {
            Some(event) if event.seq > self.seen_through => Some(event),
            _ => None,
        }
    }

    fn latest(&self) -> u64 {
        [self.finished, self.attention, self.failed]
            .into_iter()
            .flatten()
            .map(|event| event.seq)
            .max()
            .unwrap_or(0)
    }
}

/// A tab's agent as the board sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AgentSignal {
    pub activity: AgentActivity,
    pub changed_at_ms: u64,
    pub finished_at_ms: Option<u64>,
}

/// What the board knows about a tab on one refresh.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Observation {
    pub agent: Option<AgentSignal>,
    /// A command runs in its focused pane.
    pub running: bool,
    /// When any pane last produced output.
    pub output_at_ms: Option<u64>,
    /// When any pane last rang the bell or sent a notification.
    pub attention_at_ms: Option<u64>,
    /// The most recent command's exit, when it is known: when, and its status.
    pub last_exit: Option<(u64, u32)>,
}

/// Every tab's events and seen marks: what the tabs plugin keeps, and saves.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttentionBook {
    #[serde(default)]
    pub tabs: HashMap<Uuid, TabAttention>,
    /// The last sequence number given out.
    #[serde(default)]
    pub last_seq: u64,
}

impl AttentionBook {
    const fn next(&mut self, at_ms: u64) -> Event {
        self.last_seq += 1;
        Event {
            seq: self.last_seq,
            at_ms,
        }
    }

    /// Record what is new in `tab` at `now_ms`, then mark it seen when
    /// `looked_at`. Returns whether anything a view shows changed.
    pub fn observe(
        &mut self,
        tab: Uuid,
        observation: &Observation,
        looked_at: bool,
        now_ms: u64,
    ) -> bool {
        let before = self.tabs.get(&tab).copied().unwrap_or_default();
        let mut record = before;
        if let Some(agent) = observation.agent
            && agent.activity == AgentActivity::Idle
            && let Some(finished) = agent.finished_at_ms
            && now_ms.saturating_sub(finished) >= FINISH_SETTLE_MS
            && record.finished.is_none_or(|event| event.at_ms != finished)
        {
            record.finished = Some(self.next(finished));
        }
        if let Some(at) = observation.attention_at_ms
            && record.attention.is_none_or(|event| event.at_ms != at)
        {
            record.attention = Some(self.next(at));
        }
        if let Some((at, status)) = observation.last_exit
            && status != 0
            && record.failed.is_none_or(|event| event.at_ms != at)
        {
            record.failed = Some(self.next(at));
        }
        if looked_at {
            record.seen_through = record.latest();
        }
        let changed = record != before;
        if changed {
            self.tabs.insert(tab, record);
        }
        changed
    }

    /// Forget tabs that no longer exist.
    pub fn retain(&mut self, live: &HashSet<Uuid>) -> bool {
        let before = self.tabs.len();
        self.tabs.retain(|tab, _| live.contains(tab));
        self.tabs.len() != before
    }

    /// The state `tab` shows at `now_ms`, and since when, when known.
    #[must_use]
    pub fn state(
        &self,
        tab: Uuid,
        observation: &Observation,
        failed_clears: FailedClears,
        now_ms: u64,
    ) -> (TabState, Option<u64>) {
        let record = self.tabs.get(&tab).copied().unwrap_or_default();
        let agent = observation.agent;
        if let Some(agent) = agent.filter(|agent| agent.activity == AgentActivity::Blocked) {
            return (TabState::NeedsInput, Some(agent.changed_at_ms));
        }
        if let Some(event) = record.unseen(record.attention) {
            return (TabState::NeedsInput, Some(event.at_ms));
        }
        if !observation.running
            && let Some((at, status)) = observation.last_exit
            && status != 0
        {
            let shown = match failed_clears {
                FailedClears::WhenSeen => record
                    .unseen(record.failed)
                    .is_some_and(|event| event.at_ms == at),
                FailedClears::OnNextCommand => true,
            };
            if shown {
                return (TabState::Failed, Some(at));
            }
        }
        if let Some(agent) = agent.filter(|agent| agent.activity == AgentActivity::Idle)
            && let Some(event) = record.unseen(record.finished)
            && agent.finished_at_ms == Some(event.at_ms)
        {
            return (TabState::Done, Some(event.at_ms));
        }
        match agent {
            Some(agent) if agent.activity == AgentActivity::Working => {
                (TabState::Working, Some(agent.changed_at_ms))
            }
            Some(agent) => agent
                .finished_at_ms
                .map_or((TabState::Idle, None), |finished| {
                    (TabState::Stopped, Some(finished))
                }),
            None if observation.running
                && observation
                    .output_at_ms
                    .is_some_and(|at| now_ms.saturating_sub(at) < WORKING_WINDOW_MS) =>
            {
                (TabState::Working, None)
            }
            None => (TabState::Idle, None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TAB: Uuid = Uuid::from_u128(1);

    fn agent(activity: AgentActivity, changed: u64, finished: Option<u64>) -> Observation {
        Observation {
            agent: Some(AgentSignal {
                activity,
                changed_at_ms: changed,
                finished_at_ms: finished,
            }),
            running: true,
            ..Observation::default()
        }
    }

    fn state(book: &AttentionBook, observation: &Observation, now: u64) -> TabState {
        book.state(TAB, observation, FailedClears::WhenSeen, now).0
    }

    /// An agent that finishes while nobody looks is done until someone does,
    /// then stopped; one that finishes in view is stopped at once.
    #[test]
    fn a_finish_is_done_until_looked_at() {
        let mut book = AttentionBook::default();
        let working = agent(AgentActivity::Working, 1_000, None);
        book.observe(TAB, &working, false, 1_000);
        assert_eq!(state(&book, &working, 1_000), TabState::Working);

        let idle = agent(AgentActivity::Idle, 5_000, Some(5_000));
        book.observe(TAB, &idle, false, 5_000);
        assert_eq!(
            state(&book, &idle, 5_000),
            TabState::Stopped,
            "not finished until it stays idle"
        );
        assert!(book.observe(TAB, &idle, false, 6_600));
        assert_eq!(
            book.state(TAB, &idle, FailedClears::WhenSeen, 6_600),
            (TabState::Done, Some(5_000))
        );
        assert!(!book.observe(TAB, &idle, false, 9_000), "recorded once");
        assert!(book.observe(TAB, &idle, true, 9_500));
        assert_eq!(state(&book, &idle, 9_500), TabState::Stopped);

        // In view when it finishes: never done.
        let mut viewed = AttentionBook::default();
        viewed.observe(TAB, &idle, true, 6_600);
        assert_eq!(state(&viewed, &idle, 6_600), TabState::Stopped);
    }

    #[test]
    fn an_agent_asking_before_any_work_still_needs_input() {
        let book = AttentionBook::default();
        let asking = agent(AgentActivity::Blocked, 1_000, None);
        assert_eq!(
            book.state(TAB, &asking, FailedClears::WhenSeen, 1_000),
            (TabState::NeedsInput, Some(1_000))
        );
    }

    /// A pause shorter than the settle, such as before the agent asks a
    /// question, is no finish.
    #[test]
    fn a_short_idle_before_a_question_is_not_a_finish() {
        let mut book = AttentionBook::default();
        let idle = agent(AgentActivity::Idle, 5_000, Some(5_000));
        book.observe(TAB, &idle, false, 5_500);
        let asking = agent(AgentActivity::Blocked, 6_000, Some(5_000));
        book.observe(TAB, &asking, false, 7_000);
        assert_eq!(state(&book, &asking, 7_000), TabState::NeedsInput);
        assert_eq!(book.tabs.get(&TAB).and_then(|tab| tab.finished), None);
    }

    /// Working again ends done, without anyone looking.
    #[test]
    fn working_again_ends_done() {
        let mut book = AttentionBook::default();
        let idle = agent(AgentActivity::Idle, 5_000, Some(5_000));
        book.observe(TAB, &idle, false, 7_000);
        assert_eq!(state(&book, &idle, 7_000), TabState::Done);
        let working = agent(AgentActivity::Working, 8_000, Some(5_000));
        assert_eq!(state(&book, &working, 8_000), TabState::Working);
    }

    /// A bell or notification needs input until looked at.
    #[test]
    fn a_bell_needs_input_until_seen() {
        let mut book = AttentionBook::default();
        let rang = Observation {
            attention_at_ms: Some(2_000),
            ..Observation::default()
        };
        book.observe(TAB, &rang, false, 2_000);
        assert_eq!(
            book.state(TAB, &rang, FailedClears::WhenSeen, 2_000),
            (TabState::NeedsInput, Some(2_000))
        );
        book.observe(TAB, &rang, true, 3_000);
        assert_eq!(state(&book, &rang, 3_000), TabState::Idle);
    }

    /// A failure clears once seen by default, or only with the next command.
    #[test]
    fn failures_clear_by_the_configured_rule() {
        let failed = Observation {
            running: false,
            last_exit: Some((4_000, 101)),
            ..Observation::default()
        };
        let mut book = AttentionBook::default();
        book.observe(TAB, &failed, false, 4_000);
        for clears in [FailedClears::WhenSeen, FailedClears::OnNextCommand] {
            assert_eq!(
                book.state(TAB, &failed, clears, 4_000),
                (TabState::Failed, Some(4_000))
            );
        }
        book.observe(TAB, &failed, true, 5_000);
        assert_eq!(
            book.state(TAB, &failed, FailedClears::WhenSeen, 5_000).0,
            TabState::Idle
        );
        assert_eq!(
            book.state(TAB, &failed, FailedClears::OnNextCommand, 5_000)
                .0,
            TabState::Failed
        );
        let rerun = Observation {
            running: true,
            ..failed
        };
        assert_eq!(
            book.state(TAB, &rerun, FailedClears::OnNextCommand, 6_000)
                .0,
            TabState::Idle,
            "the next command clears it"
        );
        let succeeded = Observation {
            last_exit: Some((7_000, 0)),
            ..failed
        };
        book.observe(TAB, &succeeded, false, 7_000);
        assert_eq!(
            book.state(TAB, &succeeded, FailedClears::OnNextCommand, 7_000)
                .0,
            TabState::Idle
        );
    }

    /// Needs input outranks failed, which outranks done, which outranks
    /// working.
    #[test]
    fn states_rank_most_pressing_first() {
        let mut book = AttentionBook::default();
        let mut observation = agent(AgentActivity::Idle, 5_000, Some(5_000));
        observation.running = false;
        observation.last_exit = Some((5_100, 1));
        observation.attention_at_ms = Some(5_200);
        book.observe(TAB, &observation, false, 7_000);
        assert_eq!(state(&book, &observation, 7_000), TabState::NeedsInput);
        observation.attention_at_ms = None;
        let mut without_bell = book.clone();
        without_bell.tabs.get_mut(&TAB).unwrap().attention = None;
        assert_eq!(state(&without_bell, &observation, 7_000), TabState::Failed);
        observation.last_exit = None;
        assert_eq!(state(&without_bell, &observation, 7_000), TabState::Done);
    }

    /// Seen marks and events survive a save and a reload; a book in a
    /// format this build does not know is reported, not guessed at.
    #[test]
    fn the_book_round_trips_and_unknown_formats_are_left_alone() {
        let mut book = AttentionBook::default();
        book.observe(
            TAB,
            &agent(AgentActivity::Idle, 5_000, Some(5_000)),
            false,
            7_000,
        );
        let saved = book.to_bytes().unwrap();
        let Loaded::Book(loaded) = Loaded::from_bytes(Some(&saved)) else {
            panic!("a book");
        };
        assert_eq!(loaded, book);
        assert_eq!(
            state(
                &loaded,
                &agent(AgentActivity::Idle, 5_000, Some(5_000)),
                9_000
            ),
            TabState::Done,
            "an unseen finish is still done after a restart"
        );
        assert_eq!(Loaded::from_bytes(None), Loaded::Nothing);
        assert_eq!(
            Loaded::from_bytes(Some(br#"{"version":2,"book":{}}"#)),
            Loaded::Unsupported
        );
        assert_eq!(Loaded::from_bytes(Some(b"garbage")), Loaded::Unsupported);
        let mut pruned = loaded;
        assert!(pruned.retain(&HashSet::new()));
        assert!(pruned.tabs.is_empty());
    }

    #[test]
    fn failed_clears_is_a_setting() {
        let parse = |text: &str| FailedClears::parse(Some(&toml::from_str(text).unwrap()));
        assert_eq!(FailedClears::parse(None), Ok(FailedClears::WhenSeen));
        assert_eq!(
            parse("failed_clears = 'on_next_command'"),
            Ok(FailedClears::OnNextCommand)
        );
        assert!(parse("failed_clears = 'never'").is_err());
    }
}
