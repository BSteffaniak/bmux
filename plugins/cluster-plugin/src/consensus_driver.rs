//! Runs the pure consensus engine against durable storage, peers and timers.
//!
//! One task owns the [`Engine`] and applies its effects. Durable effects run in
//! issue order on a single storage worker (blocking work off the async thread),
//! and only their completion is fed back to the engine. Outbound peer requests
//! are request/response calls whose replies re-enter as authenticated messages;
//! an inbound request waits for the engine's matching reply. Everything goes
//! through `switchy_async`, so the same code runs under deterministic simulation.

use crate::consensus_engine::{
    Effect, Engine, Input, Message, NodeId, Rejection, StopReason, TimerKind, Write,
};
use crate::consensus_model::{LogIdentity, LogIndex, Membership, StoredMembership, Term};
use std::collections::BTreeMap;
use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;
use switchy_async::time::Instant;
use tokio::sync::{mpsc, oneshot, watch};

#[cfg(test)]
mod tests;

const INBOX_CAPACITY: usize = 256;

/// Durable authority, driven in issue order by one worker. Implementations block.
pub trait Storage: Send + 'static {
    /// The applied state a linearizable read observes.
    type View: Send + 'static;

    /// # Errors
    /// Any failure stops the engine; durability is then uncertain.
    fn write(&mut self, write: &Write) -> io::Result<()>;

    /// Install the snapshot staged from `from` through `last_included`.
    /// # Errors
    /// Any failure stops the engine.
    fn install_snapshot(
        &mut self,
        from: NodeId,
        last_included: LogIdentity,
        membership: &StoredMembership,
    ) -> io::Result<()>;

    /// Apply committed entries through `through` in one durable transaction and
    /// return each application entry's response by log identity.
    /// # Errors
    /// Any failure stops the engine.
    fn apply(&mut self, through: LogIdentity) -> io::Result<Vec<(LogIdentity, Vec<u8>)>>;

    /// Snapshot and compact through the applied position when worthwhile.
    /// # Errors
    /// Any failure stops the engine.
    fn compact(&mut self) -> io::Result<Option<(LogIdentity, StoredMembership)>>;

    /// # Errors
    /// Read failure is reported to the reader, not treated as empty state.
    fn read(&mut self) -> io::Result<Self::View>;

    /// Release durable ownership.
    /// # Errors
    /// Reports incomplete cleanup.
    fn close(self) -> io::Result<()>;
}

pub type Reply = Pin<Box<dyn Future<Output = Option<Message>> + Send>>;

/// Authenticated request/response transport to peers.
pub trait Transport: Send + Sync + 'static {
    /// Send a request and return the peer's reply, or `None` when unavailable.
    fn call(&self, to: NodeId, message: Message) -> Reply;
    /// Stage and install the latest snapshot on `to`, returning its reply.
    fn send_snapshot(&self, to: NodeId, term: Term, seq: u64) -> Reply;
}

/// Draws an election deadline within `[min, max]`.
pub type Jitter = Box<dyn FnMut(Duration, Duration) -> Duration + Send>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientError {
    Rejected(Rejection),
    /// The proposal's slot was taken by another entry; it did not apply.
    Superseded,
    /// The engine stopped; the outcome may be unknown.
    Stopped(Option<StopReason>),
    Storage(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Status {
    pub term: Term,
    pub leader: Option<NodeId>,
    pub commit: Option<LogIdentity>,
    pub applied: Option<LogIdentity>,
}

enum Command<V> {
    Inbound {
        from: NodeId,
        message: Message,
        reply: Option<oneshot::Sender<Message>>,
    },
    Propose {
        payload: Vec<u8>,
        respond: oneshot::Sender<Result<Vec<u8>, ClientError>>,
    },
    ChangeMembership {
        target: Membership,
        respond: oneshot::Sender<Result<LogIdentity, ClientError>>,
    },
    Read {
        respond: oneshot::Sender<Result<V, ClientError>>,
    },
    Shutdown,
}

enum Operation {
    Write(u64, Write),
    Install(u64, NodeId, LogIdentity, StoredMembership),
    Apply(LogIdentity),
    Compact,
    Read(u64),
}

enum Completion<V> {
    Durable(u64),
    Applied(LogIdentity, Vec<(LogIdentity, Vec<u8>)>),
    Compacted(Option<(LogIdentity, StoredMembership)>),
    Read(u64, io::Result<V>),
    Failed,
}

/// A running consensus node.
pub struct Driver<V> {
    commands: mpsc::Sender<Command<V>>,
    status: watch::Receiver<Status>,
    task: Option<switchy_async::task::JoinHandle<()>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DriverConfig {
    /// Compact after this many entries are applied past the last snapshot.
    pub compaction_interval: u64,
}

impl Default for DriverConfig {
    fn default() -> Self {
        Self {
            compaction_interval: 128,
        }
    }
}

impl<V: Send + 'static> Driver<V> {
    /// Start a node from its recovered engine and the effects recovery produced.
    pub fn start<S, T>(
        engine: Engine,
        initial: Vec<Effect>,
        storage: S,
        transport: Arc<T>,
        jitter: Jitter,
        config: DriverConfig,
    ) -> Self
    where
        S: Storage<View = V>,
        T: Transport,
    {
        let (commands, inbox) = mpsc::channel(INBOX_CAPACITY);
        let status = Status {
            term: engine.term(),
            leader: engine.leader_hint(),
            commit: engine.commit(),
            applied: engine.applied(),
        };
        let (status_tx, status_rx) = watch::channel(status);
        let loopback = commands.downgrade();
        let task = switchy_async::task::spawn(async move {
            run(Run {
                engine,
                initial,
                storage,
                transport,
                jitter,
                config,
                inbox,
                loopback,
                status: status_tx,
            })
            .await;
        });
        Self {
            commands,
            status: status_rx,
            task: Some(task),
        }
    }

    #[must_use]
    pub fn status(&self) -> Status {
        *self.status.borrow()
    }

    #[must_use]
    pub fn watch(&self) -> watch::Receiver<Status> {
        self.status.clone()
    }

    /// Deliver an authenticated inbound request and wait for the engine's reply.
    pub async fn request(&self, from: NodeId, message: Message) -> Option<Message> {
        let (reply, receive) = oneshot::channel();
        self.commands
            .send(Command::Inbound {
                from,
                message,
                reply: Some(reply),
            })
            .await
            .ok()?;
        receive.await.ok()
    }

    /// Propose application bytes and wait for their durable application.
    /// # Errors
    /// Rejection, supersession, or stop. Dropping the future stops waiting only.
    pub async fn propose(&self, payload: Vec<u8>) -> Result<Vec<u8>, ClientError> {
        let (respond, receive) = oneshot::channel();
        self.commands
            .send(Command::Propose { payload, respond })
            .await
            .map_err(|_| ClientError::Stopped(None))?;
        receive.await.map_err(|_| ClientError::Stopped(None))?
    }

    /// # Errors
    /// Rejection or stop.
    pub async fn change_membership(&self, target: Membership) -> Result<LogIdentity, ClientError> {
        let (respond, receive) = oneshot::channel();
        self.commands
            .send(Command::ChangeMembership { target, respond })
            .await
            .map_err(|_| ClientError::Stopped(None))?;
        receive.await.map_err(|_| ClientError::Stopped(None))?
    }

    /// A quorum-confirmed read of applied state.
    /// # Errors
    /// Rejection (not leader / not ready), stop, or storage read failure.
    pub async fn read(&self) -> Result<V, ClientError> {
        let (respond, receive) = oneshot::channel();
        self.commands
            .send(Command::Read { respond })
            .await
            .map_err(|_| ClientError::Stopped(None))?;
        receive.await.map_err(|_| ClientError::Stopped(None))?
    }

    /// Stop admission, finish storage work, and release durable ownership.
    pub async fn shutdown(mut self) {
        let _ = self.commands.send(Command::Shutdown).await;
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
    }
}

struct Run<S: Storage, T> {
    engine: Engine,
    initial: Vec<Effect>,
    storage: S,
    transport: Arc<T>,
    jitter: Jitter,
    config: DriverConfig,
    inbox: mpsc::Receiver<Command<S::View>>,
    loopback: mpsc::WeakSender<Command<S::View>>,
    status: watch::Sender<Status>,
}

/// Engine-loop state other than the engine itself.
struct Loop<V> {
    election: Option<(u64, Instant)>,
    heartbeat: Option<(u64, Instant)>,
    replies: BTreeMap<(NodeId, u8, u64), oneshot::Sender<Message>>,
    next_token: u64,
    proposals: BTreeMap<u64, oneshot::Sender<Result<Vec<u8>, ClientError>>>,
    proposed: BTreeMap<LogIndex, (LogIdentity, u64)>,
    changes: BTreeMap<u64, oneshot::Sender<Result<LogIdentity, ClientError>>>,
    reads: BTreeMap<u64, oneshot::Sender<Result<V, ClientError>>>,
    applied_since_compaction: u64,
    compacting: bool,
}

const fn reply_key(message: &Message) -> Option<(u8, u64)> {
    match message {
        Message::VoteReply { .. } => Some((0, 0)),
        Message::AppendReply { seq, .. } => Some((1, *seq)),
        Message::SnapshotReply { seq, .. } => Some((2, *seq)),
        _ => None,
    }
}

const fn request_key(message: &Message) -> Option<(u8, u64)> {
    match message {
        Message::Vote { .. } => Some((0, 0)),
        Message::Append { seq, .. } => Some((1, *seq)),
        Message::Snapshot { seq, .. } => Some((2, *seq)),
        _ => None,
    }
}

// One state machine owns admission, effects, storage completion and shutdown so
// cancellation cannot transfer durable ownership.
#[allow(clippy::too_many_lines)]
async fn run<S: Storage, T: Transport>(run: Run<S, T>) {
    let Run {
        mut engine,
        initial,
        storage,
        transport,
        mut jitter,
        config,
        mut inbox,
        loopback,
        status,
    } = run;
    let (operations, storage_inbox) = mpsc::unbounded_channel::<Operation>();
    let (completions_tx, mut completions) = mpsc::unbounded_channel::<Completion<S::View>>();
    let worker = switchy_async::task::spawn(storage_worker(storage, storage_inbox, completions_tx));
    let mut state = Loop {
        election: None,
        heartbeat: None,
        replies: BTreeMap::new(),
        next_token: 0,
        proposals: BTreeMap::new(),
        proposed: BTreeMap::new(),
        changes: BTreeMap::new(),
        reads: BTreeMap::new(),
        applied_since_compaction: 0,
        compacting: false,
    };
    let mut effects = initial;
    loop {
        let stopped = apply_effects(
            &engine,
            std::mem::take(&mut effects),
            &mut state,
            &operations,
            &transport,
            &loopback,
            &mut jitter,
        );
        let _ = status.send(Status {
            term: engine.term(),
            leader: engine.leader_hint(),
            commit: engine.commit(),
            applied: engine.applied(),
        });
        if stopped {
            fail_all(&mut state, engine.stopped());
            break;
        }
        let deadline = [state.election, state.heartbeat]
            .into_iter()
            .flatten()
            .map(|(_, at)| at)
            .min();
        let sleep = async move {
            match deadline {
                Some(at) => switchy_async::time::sleep_until(at).await,
                None => std::future::pending::<()>().await,
            }
        };
        switchy_async::select! {
            biased;
            completion = completions.recv() => {
                let Some(completion) = completion else {
                    effects = engine.handle(Input::PersistFailed { id: u64::MAX });
                    continue;
                };
                effects = on_completion(&mut engine, &mut state, completion, &operations, config);
            }
            command = inbox.recv() => {
                match command {
                    None | Some(Command::Shutdown) => break,
                    Some(command) => effects = on_command(&mut engine, &mut state, command),
                }
            }
            () = sleep => {
                let now = Instant::now();
                let mut fired = Vec::new();
                for (kind, timer) in [
                    (TimerKind::Election, &mut state.election),
                    (TimerKind::Heartbeat, &mut state.heartbeat),
                ] {
                    if let Some((generation, at)) = *timer
                        && at <= now
                    {
                        *timer = None;
                        fired.push(Input::Timer { kind, generation });
                    }
                }
                for input in fired {
                    effects.extend(engine.handle(input));
                }
            }
        }
    }
    fail_all(&mut state, engine.stopped());
    drop(operations);
    let _ = worker.await;
}

fn fail_all<V>(state: &mut Loop<V>, reason: Option<StopReason>) {
    for (_, respond) in std::mem::take(&mut state.proposals) {
        let _ = respond.send(Err(ClientError::Stopped(reason)));
    }
    for (_, respond) in std::mem::take(&mut state.changes) {
        let _ = respond.send(Err(ClientError::Stopped(reason)));
    }
    for (_, respond) in std::mem::take(&mut state.reads) {
        let _ = respond.send(Err(ClientError::Stopped(reason)));
    }
    state.replies.clear();
    state.proposed.clear();
}

fn on_command<V>(engine: &mut Engine, state: &mut Loop<V>, command: Command<V>) -> Vec<Effect> {
    state.next_token += 1;
    let token = state.next_token;
    match command {
        Command::Inbound {
            from,
            message,
            reply,
        } => {
            if let (Some(reply), Some((kind, seq))) = (reply, request_key(&message)) {
                state.replies.insert((from, kind, seq), reply);
            }
            engine.handle(Input::Message { from, message })
        }
        Command::Propose { payload, respond } => {
            state.proposals.insert(token, respond);
            engine.handle(Input::Propose { token, payload })
        }
        Command::ChangeMembership { target, respond } => {
            state.changes.insert(token, respond);
            engine.handle(Input::ChangeMembership { token, target })
        }
        Command::Read { respond } => {
            state.reads.insert(token, respond);
            engine.handle(Input::Read { token })
        }
        Command::Shutdown => Vec::new(),
    }
}

fn on_completion<V>(
    engine: &mut Engine,
    state: &mut Loop<V>,
    completion: Completion<V>,
    operations: &mpsc::UnboundedSender<Operation>,
    config: DriverConfig,
) -> Vec<Effect> {
    match completion {
        Completion::Durable(id) => engine.handle(Input::Persisted { id }),
        Completion::Failed => engine.handle(Input::PersistFailed { id: u64::MAX }),
        Completion::Applied(through, outcomes) => {
            let outcomes = outcomes
                .into_iter()
                .map(|(identity, response)| (identity.index, (identity, response)))
                .collect::<BTreeMap<_, _>>();
            let resolved = state
                .proposed
                .range(..=through.index)
                .map(|(index, _)| *index)
                .collect::<Vec<_>>();
            for index in resolved {
                let Some((identity, token)) = state.proposed.remove(&index) else {
                    continue;
                };
                if let Some(respond) = state.proposals.remove(&token) {
                    // Another entry at the proposal's index means it never applied.
                    let result = outcomes
                        .get(&index)
                        .filter(|(applied, _)| *applied == identity)
                        .map(|(_, response)| response.clone())
                        .ok_or(ClientError::Superseded);
                    let _ = respond.send(result);
                }
            }
            let previous = engine.applied().map_or(0, |applied| applied.index.0);
            state.applied_since_compaction += through.index.0.saturating_sub(previous);
            let effects = engine.handle(Input::Applied { through });
            if state.applied_since_compaction >= config.compaction_interval && !state.compacting {
                state.compacting = true;
                let _ = operations.send(Operation::Compact);
            }
            effects
        }
        Completion::Compacted(result) => {
            state.compacting = false;
            match result {
                Some((through, membership)) => {
                    state.applied_since_compaction = 0;
                    engine.handle(Input::Compacted {
                        through,
                        membership,
                    })
                }
                None => Vec::new(),
            }
        }
        Completion::Read(token, result) => {
            if let Some(respond) = state.reads.remove(&token) {
                let _ =
                    respond.send(result.map_err(|error| ClientError::Storage(error.to_string())));
            }
            Vec::new()
        }
    }
}

/// Execute effects; returns whether the engine stopped.
fn apply_effects<V: Send + 'static, T: Transport>(
    engine: &Engine,
    mut effects: Vec<Effect>,
    state: &mut Loop<V>,
    operations: &mpsc::UnboundedSender<Operation>,
    transport: &Arc<T>,
    loopback: &mpsc::WeakSender<Command<V>>,
    jitter: &mut Jitter,
) -> bool {
    let mut stopped = false;
    while !effects.is_empty() {
        for effect in std::mem::take(&mut effects) {
            match effect {
                Effect::Persist { id, write } => {
                    let _ = operations.send(Operation::Write(id, write));
                }
                Effect::InstallSnapshot {
                    id,
                    from,
                    last_included,
                    membership,
                } => {
                    let _ =
                        operations.send(Operation::Install(id, from, last_included, membership));
                }
                Effect::Apply { through } => {
                    let _ = operations.send(Operation::Apply(through));
                }
                Effect::Send { to, message } => {
                    if let Some((kind, seq)) = reply_key(&message) {
                        if let Some(reply) = state.replies.remove(&(to, kind, seq)) {
                            let _ = reply.send(message);
                        }
                    } else {
                        let call = transport.call(to, message);
                        forward_reply(to, call, loopback.clone());
                    }
                }
                Effect::SendSnapshot { to, term, seq } => {
                    let call = transport.send_snapshot(to, term, seq);
                    let loopback = loopback.clone();
                    switchy_async::task::spawn(async move {
                        let reply = call.await;
                        if let Some(sender) = loopback.upgrade() {
                            let command = reply.map_or(
                                Command::Inbound {
                                    from: to,
                                    message: Message::SnapshotReply {
                                        term,
                                        seq,
                                        installed: None,
                                    },
                                    reply: None,
                                },
                                |message| Command::Inbound {
                                    from: to,
                                    message,
                                    reply: None,
                                },
                            );
                            let _ = sender.send(command).await;
                        }
                    });
                }
                Effect::SetTimer {
                    kind,
                    generation,
                    min,
                    max,
                } => {
                    let at = Instant::now() + jitter(min, max);
                    match kind {
                        TimerKind::Election => state.election = Some((generation, at)),
                        TimerKind::Heartbeat => state.heartbeat = Some((generation, at)),
                    }
                }
                Effect::Proposed { token, identity } => {
                    state.proposed.insert(identity.index, (identity, token));
                }
                Effect::Rejected { token, reason } => {
                    if let Some(respond) = state.proposals.remove(&token) {
                        let _ = respond.send(Err(ClientError::Rejected(reason)));
                    } else if let Some(respond) = state.changes.remove(&token) {
                        let _ = respond.send(Err(ClientError::Rejected(reason)));
                    } else if let Some(respond) = state.reads.remove(&token) {
                        let _ = respond.send(Err(ClientError::Rejected(reason)));
                    }
                }
                Effect::MembershipProposed { token, identity } => {
                    if let Some(respond) = state.changes.remove(&token) {
                        let _ = respond.send(Ok(identity));
                    }
                }
                Effect::ReadReady { token, .. } => {
                    // Reads queue behind every apply already issued.
                    let _ = operations.send(Operation::Read(token));
                }
                Effect::ReadFailed { token } => {
                    if let Some(respond) = state.reads.remove(&token) {
                        let _ = respond.send(Err(ClientError::Rejected(Rejection::NotLeader {
                            hint: engine.leader_hint(),
                        })));
                    }
                }
                Effect::Stopped { .. } => stopped = true,
                Effect::Leadership { .. } | Effect::ProtocolViolation { .. } => {}
            }
        }
    }
    stopped
}

fn forward_reply<V: Send + 'static>(
    to: NodeId,
    call: Reply,
    loopback: mpsc::WeakSender<Command<V>>,
) {
    switchy_async::task::spawn(async move {
        if let Some(message) = call.await
            && let Some(sender) = loopback.upgrade()
        {
            let _ = sender
                .send(Command::Inbound {
                    from: to,
                    message,
                    reply: None,
                })
                .await;
        }
    });
}

/// The single serialized owner of durable authority.
async fn storage_worker<S: Storage>(
    storage: S,
    mut operations: mpsc::UnboundedReceiver<Operation>,
    completions: mpsc::UnboundedSender<Completion<S::View>>,
) {
    let mut storage = Some(storage);
    let mut failed = false;
    while let Some(operation) = operations.recv().await {
        if failed {
            continue;
        }
        let Some(mut owned) = storage.take() else {
            break;
        };
        let joined = switchy_async::task::spawn_blocking(move || {
            let completion = match operation {
                Operation::Write(id, write) => {
                    owned.write(&write).map(|()| Completion::Durable(id))
                }
                Operation::Install(id, from, last_included, membership) => owned
                    .install_snapshot(from, last_included, &membership)
                    .map(|()| Completion::Durable(id)),
                Operation::Apply(through) => owned
                    .apply(through)
                    .map(|outcomes| Completion::Applied(through, outcomes)),
                Operation::Compact => owned.compact().map(Completion::Compacted),
                Operation::Read(token) => Ok(Completion::Read(token, owned.read())),
            };
            (owned, completion)
        })
        .await;
        let Ok((owned, completion)) = joined else {
            let _ = completions.send(Completion::Failed);
            failed = true;
            continue;
        };
        storage = Some(owned);
        if let Ok(completion) = completion {
            let _ = completions.send(completion);
        } else {
            failed = true;
            let _ = completions.send(Completion::Failed);
        }
    }
    if let Some(storage) = storage {
        let _ = switchy_async::task::spawn_blocking(move || storage.close()).await;
    }
}
