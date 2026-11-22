//! A bmux owned-consensus cluster under faults, deterministically simulated.
//!
//! Three hosts each run the real consensus driver over the real redb store, whose file
//! I/O goes through the `switchy_fs` simulator. Each host has its own filesystem
//! namespace; a crash loses what was not synced, as a power cut would, and the host
//! restarts from its store. Node 1 founds a single-voter cluster; the workload adds
//! nodes 2 and 3 as learners and promotes them through joint consensus, then proposes
//! uniquely identified commands while a nemesis partitions, isolates, degrades links
//! and bounces hosts. After healing, every node's applied state must be identical,
//! contain every acknowledged write, and contain nothing that was never proposed;
//! linearizable reads must observe every write acknowledged before they started.
//!
//! Peer messages are delivered in process, gated by the simulated network
//! (reachability, latency, loss), so partitions and crashed hosts behave as on a real
//! network. Every fault, delay and election deadline comes from the run's seed.

#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery)]

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use bmux_cluster_plugin::consensus_driver::{
    ClientError, Driver, DriverConfig, Jitter, Reply, SnapshotObject, Storage, Transport,
};
use bmux_cluster_plugin::consensus_engine::{Config, Engine, Message, NodeId, Rejection, Write};
use bmux_cluster_plugin::consensus_model::{LogIdentity, Membership, StoredMembership, Term};
use bmux_cluster_plugin::control_state::ControlState;
use bmux_cluster_plugin::owned_vote_store::{OwnedStorage, OwnedVoteStore};
use bmux_cluster_plugin_api::cluster_types::{
    CommandId, ControlCommand, ControlCommandRequest, WorkspaceId,
};
use simvar::switchy::random::rng;
use simvar::{Sim, SimBootstrap, SimConfig, fault, fault::network, fault::network::LinkPolicy};
use switchy_async::time::{sleep, timeout};
use switchy_fs::simulator::{CrashPolicy, Filesystem, with_filesystem};

fn name(index: usize) -> String {
    format!("n{}", index + 1)
}
const CLUSTER: &str = "simulated-cluster";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(2);

fn node_id(index: usize) -> NodeId {
    NodeId::from(index as u64 + 1)
}

fn index_of(id: NodeId, count: usize) -> Option<usize> {
    (0..count).find(|index| node_id(*index) == id)
}

fn boxed(message: impl Into<String>) -> Box<dyn std::error::Error + Send> {
    Box::new(io::Error::other(message.into()))
}

/// What the hosts, workload and nemesis share outside the network.
#[derive(Default)]
struct World {
    nodes: usize,
    filesystems: Vec<Arc<Filesystem>>,
    /// Per run, so parallel runs never contend through process-wide state.
    preflight: bmux_cluster_plugin::owned_vote_store::PreflightAdmission,
    drivers: BTreeMap<usize, Arc<Driver<ControlState>>>,
    /// Snapshot bytes staged on each node, by sender and boundary index. Staging
    /// is disposable: a crash clears the node's entries.
    staged: BTreeMap<usize, BTreeMap<(NodeId, u64), Vec<u8>>>,
    proposed: BTreeSet<uuid::Uuid>,
    acknowledged: BTreeSet<uuid::Uuid>,
    /// The membership has all three voters; faults may begin.
    formed: bool,
    /// The nemesis healed everything; the workload stops proposing.
    quiet: bool,
    /// The workload stopped and has no proposal in flight.
    workload_done: bool,
    violations: Vec<String>,
    faults: usize,
    /// A voter the nemesis removed from the configuration, if any.
    removed: Option<usize>,
    membership_changes: usize,
    snapshot_installs: usize,
    /// Debug history (`SIM_DEBUG`): faults, membership changes and acknowledgements.
    events: Vec<String>,
}

fn event(world: &Shared, text: impl std::fmt::Display) {
    if std::env::var("SIM_DEBUG").is_ok() {
        let now = switchy_async::time::Instant::now();
        lock(world).events.push(format!("{now:?} {text}"));
    }
}

type Shared = Arc<Mutex<World>>;

fn lock(world: &Shared) -> std::sync::MutexGuard<'_, World> {
    world.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Runs every storage operation inside the host's filesystem namespace.
struct Scoped {
    filesystem: Arc<Filesystem>,
    inner: OwnedStorage,
    index: usize,
}

fn trace(index: usize, text: impl FnOnce() -> String) {
    if std::env::var("SIM_TRACE").is_ok() {
        eprintln!("store n{} {}", index + 1, text());
    }
}

impl Scoped {
    fn state_summary(&mut self) -> String {
        if std::env::var("SIM_SUMMARY").is_err() {
            return String::new();
        }
        let filesystem = self.filesystem.clone();
        with_filesystem(&filesystem, || self.inner.read())
            .map(|state| format!(" [rev={} ws={}]", state.revision, state.workspaces.len()))
            .unwrap_or_default()
    }
}

impl Storage for Scoped {
    type View = ControlState;

    fn write(&mut self, write: &Write) -> io::Result<()> {
        trace(self.index, || {
            format!(
                "write hs={:?} truncate={:?} entries={:?} commit={:?}",
                write.hard_state.map(|h| h.term.0),
                write.truncate_from.map(|t| t.0),
                write
                    .entries
                    .iter()
                    .map(|e| {
                        let what = match &e.payload {
                            bmux_cluster_plugin::consensus_model::EntryPayload::Application(
                                bytes,
                            ) => bmux_cluster_plugin::control_codec::decode_control_command(bytes)
                                .map_or_else(
                                    |_| "?".into(),
                                    |c| format!("{:x}", c.command_id.value.as_u128()),
                                ),
                            bmux_cluster_plugin::consensus_model::EntryPayload::NoOp => {
                                "noop".into()
                            }
                            bmux_cluster_plugin::consensus_model::EntryPayload::Membership(m) => {
                                format!(
                                    "cfg{:?}",
                                    m.configurations()
                                        .iter()
                                        .map(BTreeSet::len)
                                        .collect::<Vec<_>>()
                                )
                            }
                        };
                        (e.identity.term.0, e.identity.index.0, what)
                    })
                    .collect::<Vec<_>>(),
                write.commit.map(|c| (c.term.0, c.index.0))
            )
        });
        let filesystem = self.filesystem.clone();
        let result = with_filesystem(&filesystem, || self.inner.write(write));
        let summary = self.state_summary();
        trace(self.index, || format!("  after write{summary}"));
        result
    }

    fn install_snapshot(
        &mut self,
        from: NodeId,
        last_included: LogIdentity,
        membership: &StoredMembership,
    ) -> io::Result<()> {
        trace(self.index, || {
            format!(
                "install {:?}",
                (last_included.term.0, last_included.index.0)
            )
        });
        let filesystem = self.filesystem.clone();
        with_filesystem(&filesystem, || {
            self.inner.install_snapshot(from, last_included, membership)
        })
    }

    fn apply(&mut self, through: LogIdentity) -> io::Result<Vec<(LogIdentity, Vec<u8>)>> {
        let filesystem = self.filesystem.clone();
        let result = with_filesystem(&filesystem, || self.inner.apply(through));
        let summary = self.state_summary();
        trace(self.index, || format!("  after apply{summary}"));
        trace(self.index, || {
            format!(
                "apply through={:?} outcomes={:?}",
                (through.term.0, through.index.0),
                result
                    .as_ref()
                    .map(|o| o.iter().map(|(i, _)| i.index.0).collect::<Vec<_>>())
            )
        });
        result
    }

    fn compact(&mut self) -> io::Result<Option<(LogIdentity, StoredMembership)>> {
        let filesystem = self.filesystem.clone();
        let result = with_filesystem(&filesystem, || self.inner.compact());
        let summary = self.state_summary();
        trace(self.index, || format!("  after compact{summary}"));
        trace(self.index, || {
            format!(
                "compact {:?}",
                result.as_ref().map(|r| r.as_ref().map(|(b, _)| b.index.0))
            )
        });
        result
    }

    fn snapshot(&mut self) -> io::Result<Option<SnapshotObject>> {
        let filesystem = self.filesystem.clone();
        with_filesystem(&filesystem, || self.inner.snapshot())
    }

    fn read(&mut self) -> io::Result<ControlState> {
        let filesystem = self.filesystem.clone();
        let result = with_filesystem(&filesystem, || self.inner.read());
        trace(self.index, || {
            format!(
                "read {:?}",
                result
                    .as_ref()
                    .map(|state| (state.revision, state.workspaces.len()))
            )
        });
        result
    }

    fn close(self) -> io::Result<()> {
        let filesystem = self.filesystem.clone();
        with_filesystem(&filesystem, move || self.inner.close())
    }
}

/// In-process delivery gated by the simulated network.
struct SimTransport {
    local: usize,
    world: Shared,
}

impl Transport for SimTransport {
    fn call(&self, to: NodeId, message: Message) -> Reply {
        let local = self.local;
        let world = self.world.clone();
        Box::pin(async move {
            let count = lock(&world).nodes;
            let target = index_of(to, count)?;
            let (from, peer) = (name(local), name(target));
            let (from, peer) = (from.as_str(), peer.as_str());
            if !network::can_reach(from, peer) || dropped(from, peer) {
                return None;
            }
            sleep(network::sample_latency(from, peer)).await;
            let driver = lock(&world).drivers.get(&target).cloned()?;
            let reply = timeout(REQUEST_TIMEOUT, driver.request(node_id(local), message))
                .await
                .ok()??;
            if !network::can_reach(peer, from) || dropped(peer, from) {
                return None;
            }
            sleep(network::sample_latency(peer, from)).await;
            Some(reply)
        })
    }

    fn send_snapshot(&self, to: NodeId, term: Term, seq: u64, snapshot: SnapshotObject) -> Reply {
        let local = self.local;
        let world = self.world.clone();
        Box::pin(async move {
            let count = lock(&world).nodes;
            let target = index_of(to, count)?;
            let (from, peer) = (name(local), name(target));
            let (from, peer) = (from.as_str(), peer.as_str());
            if !network::can_reach(from, peer) || dropped(from, peer) {
                return None;
            }
            // Staging the object costs one transfer's latency before the install.
            sleep(network::sample_latency(from, peer)).await;
            let driver = {
                let mut world = lock(&world);
                world.staged.entry(target).or_default().insert(
                    (node_id(local), snapshot.last_included.index.0),
                    snapshot.bytes,
                );
                world.drivers.get(&target).cloned()?
            };
            let reply = timeout(
                REQUEST_TIMEOUT,
                driver.request(
                    node_id(local),
                    Message::Snapshot {
                        term,
                        seq,
                        last_included: snapshot.last_included,
                        membership: snapshot.membership,
                    },
                ),
            )
            .await
            .ok()??;
            if !network::can_reach(peer, from) || dropped(peer, from) {
                return None;
            }
            sleep(network::sample_latency(peer, from)).await;
            if matches!(
                reply,
                Message::SnapshotReply {
                    installed: Some(_),
                    ..
                }
            ) {
                lock(&world).snapshot_installs += 1;
            }
            Some(reply)
        })
    }
}

fn dropped(from: &str, to: &str) -> bool {
    let loss = network::link_policy(from, to).loss;
    loss > 0.0 && rng().gen_bool(loss)
}

fn jitter() -> Jitter {
    Box::new(|min, max| {
        let span = u64::try_from(max.saturating_sub(min).as_millis()).unwrap_or(0);
        min + Duration::from_millis(rng().gen_range(0..=span))
    })
}

/// Open (or provision) this host's store and start its driver.
fn start(index: usize, world: &Shared) -> io::Result<Driver<ControlState>> {
    let (filesystem, preflight) = {
        let world = lock(world);
        (world.filesystems[index].clone(), world.preflight.clone())
    };
    let path = std::path::PathBuf::from(format!("/node-{}/owned.redb", index + 1));
    let mut store = with_filesystem(&filesystem, || -> io::Result<OwnedVoteStore> {
        if switchy_fs::exists(&path) {
            return OwnedVoteStore::open_with(&path, CLUSTER, &preflight);
        }
        // The store syncs its own directory; the directory's entry in `/` must be
        // durable too, or a power cut loses the whole store.
        switchy_fs::sync::create_dir_all(path.parent().expect("store directory"))?;
        switchy_fs::sync::sync_directory("/")?;
        if index == 0 {
            let founder = node_id(0);
            let membership = Membership::new(vec![vec![founder]], vec![(founder, name(0))])
                .map_err(|error| io::Error::other(format!("{error:?}")))?;
            OwnedVoteStore::bootstrap(&path, CLUSTER, founder, membership)
        } else {
            OwnedVoteStore::create(&path, CLUSTER)
        }
    })?;
    let recovered = with_filesystem(&filesystem, || store.recovered())?;
    let (engine, effects) = Engine::recover(node_id(index), Config::default(), recovered)
        .map_err(|error| io::Error::other(format!("{error:?}")))?;
    let storage = Scoped {
        index,
        filesystem,
        inner: OwnedStorage::new(store, {
            let world = world.clone();
            Box::new(move |from, boundary| {
                lock(&world)
                    .staged
                    .get(&index)
                    .and_then(|staged| staged.get(&(from, boundary.index.0)))
                    .cloned()
                    .ok_or_else(|| io::Error::other("no staged snapshot"))
            })
        }),
    };
    Ok(Driver::start(
        engine,
        effects,
        storage,
        Arc::new(SimTransport {
            local: index,
            world: world.clone(),
        }),
        jitter(),
        DriverConfig {
            // Compact often so lagging followers regularly catch up by snapshot.
            compaction_interval: 16,
        },
    ))
}

async fn host(index: usize, world: Shared) -> simvar::host::HostResult {
    let driver = start(index, &world).map_err(|error| boxed(error.to_string()))?;
    lock(&world).drivers.insert(index, Arc::new(driver));
    loop {
        sleep(Duration::from_secs(1)).await;
    }
}

fn drivers(world: &Shared) -> Vec<(usize, Arc<Driver<ControlState>>)> {
    lock(world)
        .drivers
        .iter()
        .map(|(index, driver)| (*index, driver.clone()))
        .collect()
}

fn command(id: uuid::Uuid) -> Vec<u8> {
    bmux_cluster_plugin::control_codec::encode_control_command(&ControlCommand {
        schema_version: 1,
        principal_id: "principal:simulation".into(),
        command_id: CommandId { value: id },
        issued_at_unix_ms: 1,
        request: ControlCommandRequest::CreateWorkspace {
            workspace_id: WorkspaceId { value: id },
            name: None,
        },
    })
}

fn records(count: usize) -> Vec<(NodeId, String)> {
    (0..count)
        .map(|index| (node_id(index), name(index)))
        .collect()
}

/// Apply a membership change on whichever node currently leads, retrying.
/// Propose a membership change on whichever node leads, for up to `rounds` rounds.
async fn try_change(world: &Shared, voters: &[usize], rounds: usize) -> bool {
    let target = Membership::new(
        vec![voters.iter().map(|index| node_id(*index)).collect()],
        records(lock(world).nodes),
    )
    .expect("valid simulated membership");
    for _ in 0..rounds {
        for (_, driver) in drivers(world) {
            if let Ok(Ok(_)) =
                timeout(REQUEST_TIMEOUT, driver.change_membership(target.clone())).await
            {
                return true;
            }
        }
        sleep(Duration::from_millis(50)).await;
    }
    false
}

fn voters_without(world: &Shared, removed: Option<usize>) -> Vec<usize> {
    (0..lock(world).nodes)
        .filter(|index| Some(*index) != removed)
        .collect()
}

/// A leader's status once it has nothing uncommitted, so its committed
/// configuration is final rather than about to be replaced by an in-flight change.
fn settled_leader(world: &Shared) -> Option<bmux_cluster_plugin::consensus_driver::Status> {
    drivers(world)
        .iter()
        .map(|(index, driver)| (*index, driver.status()))
        .find(|(index, status)| {
            status.leader == Some(node_id(*index)) && status.commit == status.last_log
        })
        .map(|(_, status)| status)
}

/// Whether a settled leader reports this committed uniform configuration shape.
fn committed(world: &Shared, voters: usize, members: usize) -> bool {
    settled_leader(world).is_some_and(|status| {
        status.committed_voters == voters && status.committed_members == members
    })
}

/// Drive the configuration to `voters` with every node recorded, retrying until it
/// commits: a proposal is indeterminate until then (a leader change can drop it).
/// Missing voters first join as learners, then are promoted through joint consensus.
async fn ensure_membership(world: &Shared, voters: &[usize]) {
    let count = lock(world).nodes;
    // Learner records for every node, keeping only committed voters that stay.
    loop {
        if let Some(status) = settled_leader(world) {
            if status.committed_members == count {
                break;
            }
            let present = (0..count)
                .filter(|index| {
                    voters.contains(index) && status.committed_voter_ids.contains(&node_id(*index))
                })
                .collect::<Vec<_>>();
            if !present.is_empty() {
                try_change(world, &present, 1).await;
            }
        }
        sleep(Duration::from_millis(100)).await;
    }
    while !committed(world, voters.len(), count) {
        try_change(world, voters, 1).await;
        sleep(Duration::from_millis(100)).await;
    }
}

/// Form the cluster, then propose and read until the nemesis is done.
async fn workload(world: Shared) -> simvar::client::ClientResult {
    // Learner records first, then promotion through joint consensus.
    let count = lock(&world).nodes;
    let all = (0..count).collect::<Vec<_>>();
    let formation = ensure_membership(&world, &all);
    if timeout(Duration::from_secs(300), formation).await.is_err() {
        let statuses = describe(&world);
        lock(&world)
            .violations
            .push(format!("cluster never formed: {statuses}"));
        let mut world = lock(&world);
        world.formed = true;
        world.quiet = true;
        world.workload_done = true;
        drop(world);
        return Ok(());
    }
    lock(&world).formed = true;
    let mut next = 0u128;
    while !lock(&world).quiet {
        next += 1;
        let id = uuid::Uuid::from_u128(next);
        lock(&world).proposed.insert(id);
        let mut outcome = None;
        for (index, driver) in drivers(&world) {
            match timeout(REQUEST_TIMEOUT, driver.propose(command(id))).await {
                Ok(Ok(response)) => {
                    let status = driver.status();
                    let result =
                        bmux_cluster_plugin::control_codec::decode_control_response(&response)
                            .map(|response| format!("{:?}", response.result));
                    event(
                        &world,
                        format!(
                            "ack {id} result={result:?} by n{} term={:?} commit={:?} voters={:?}",
                            index + 1,
                            status.term.0,
                            status.commit.map(|c| (c.term.0, c.index.0)),
                            status.committed_voter_ids.len()
                        ),
                    );
                    outcome = Some(true);
                    break;
                }
                Ok(Err(ClientError::Rejected(Rejection::NotLeader { .. }))) => {}
                // Indeterminate: it may or may not have applied.
                _ => outcome = Some(false),
            }
            if outcome.is_some() {
                break;
            }
        }
        if outcome == Some(true) {
            lock(&world).acknowledged.insert(id);
        }
        if next.is_multiple_of(5) {
            check_read(&world).await;
        }
        sleep(Duration::from_millis(rng().gen_range(5..50))).await;
    }
    lock(&world).workload_done = true;
    Ok(())
}

/// A linearizable read observes every write acknowledged before it started.
async fn check_read(world: &Shared) {
    let before = lock(world).acknowledged.clone();
    for (_, driver) in drivers(world) {
        if let Ok(Ok(state)) = timeout(REQUEST_TIMEOUT, driver.read()).await {
            let seen = state.workspaces.keys().copied().collect::<BTreeSet<_>>();
            let proposed = lock(world).proposed.clone();
            let violations = before
                .difference(&seen)
                .map(|missing| format!("linearizable read missed acknowledged write {missing}"))
                .chain(
                    seen.difference(&proposed)
                        .map(|unknown| format!("read observed never-proposed write {unknown}")),
                )
                .collect::<Vec<_>>();
            lock(world).violations.extend(violations);
            return;
        }
    }
}

/// Inject faults, heal, then check convergence and durability.
async fn nemesis(world: Shared) -> simvar::client::ClientResult {
    while !lock(&world).formed {
        sleep(Duration::from_millis(50)).await;
    }
    let reliable = network::default_policy();
    for _ in 0..rng().gen_range(10..30) {
        sleep(Duration::from_millis(rng().gen_range(100..1500))).await;
        lock(&world).faults += 1;
        let count = lock(&world).nodes;
        let victim = name(rng().gen_range(0..count));
        let others: Vec<_> = (0..count)
            .map(name)
            .filter(|other| *other != victim)
            .collect();
        let victim = victim.as_str();
        let action = rng().gen_range(0..7);
        event(&world, format!("fault {action} victim={victim}"));
        match action {
            0 => network::partition([victim], &others),
            1 => network::partition_one_way(&others, [victim]),
            2 => network::isolate(victim),
            3 => network::set_default_policy(
                LinkPolicy::reliable()
                    .with_latency(Duration::from_millis(1), Duration::from_millis(60))
                    .with_loss(0.2),
            ),
            4 => fault::bounce_after(victim, Duration::from_millis(rng().gen_range(0..1000))),
            5 => {
                // Remove a voter through joint consensus, under whatever faults
                // are active. Best effort: without a leader it is skipped.
                let removed = lock(&world).removed;
                if removed.is_none() {
                    let candidate = rng().gen_range(0..count);
                    event(&world, format!("remove n{} proposed?", candidate + 1));
                    if try_change(&world, &voters_without(&world, Some(candidate)), 3).await {
                        event(&world, format!("remove n{} accepted", candidate + 1));
                        let mut world = lock(&world);
                        world.removed = Some(candidate);
                        world.membership_changes += 1;
                    }
                }
            }
            _ => {
                network::heal();
                network::set_default_policy(reliable);
            }
        }
    }
    network::heal();
    network::set_default_policy(reliable);
    lock(&world).quiet = true;
    let count = lock(&world).nodes;
    // Restore full membership: the removed node rejoins as a learner, then is
    // promoted, before convergence is checked.
    // Take the value first: a guard in the `if let` scrutinee would be held
    // for the whole body, and the body locks the world again.
    let removed = lock(&world).removed.take();
    if let Some(removed) = removed {
        let all = voters_without(&world, None);
        let restore = ensure_membership(&world, &all);
        // A healed cluster must finish the transition; never pass by running out the clock.
        if timeout(Duration::from_secs(120), restore).await.is_err() {
            let statuses = describe(&world);
            lock(&world).violations.push(format!(
                "membership restoration of n{} did not complete: {statuses}",
                removed + 1
            ));
            return finish(&world);
        }
        lock(&world).membership_changes += 1;
    }
    // In-flight proposals can still commit; let the workload finish first.
    let settled = async {
        while !lock(&world).workload_done {
            sleep(Duration::from_millis(50)).await;
        }
    };
    if timeout(Duration::from_secs(60), settled).await.is_err() {
        lock(&world)
            .violations
            .push("workload did not stop after healing".into());
        return finish(&world);
    }
    let running = async {
        while (0..count)
            .any(|index| fault::host_status(&name(index)) != Some(fault::HostStatus::Running))
        {
            sleep(Duration::from_millis(50)).await;
        }
    };
    if timeout(Duration::from_secs(60), running).await.is_err() {
        lock(&world)
            .violations
            .push("hosts did not restart after healing".into());
        return finish(&world);
    }
    converge(&world).await
}

fn describe(world: &Shared) -> String {
    let world = lock(world);
    world
        .drivers
        .iter()
        .map(|(index, driver)| format!("n{}={:?}", index + 1, driver.status()))
        .collect::<Vec<_>>()
        .join("; ")
}

/// End the run, failing it if any violation was recorded.
fn finish(world: &Shared) -> simvar::client::ClientResult {
    let violations = std::mem::take(&mut lock(world).violations);
    simvar::utils::cancel_simulation();
    if violations.is_empty() {
        Ok(())
    } else {
        Err(boxed(violations.join("\n")))
    }
}

type NodeWrites = Vec<(usize, BTreeSet<uuid::Uuid>)>;

/// Every node's applied writes, read while the cluster is quiescent; empty if it
/// never settled.
async fn read_quiescent(world: &Shared) -> NodeWrites {
    let mut states = Vec::new();
    for _ in 0..600 {
        sleep(Duration::from_millis(100)).await;
        // A leader commits a no-op of its term, so idle followers still converge.
        let all = drivers(world);
        if all.len() < lock(world).nodes {
            continue;
        }
        // Read only a quiescent cluster: the leader has nothing uncommitted and
        // every node applied exactly that point; re-check after reading.
        let leader = all
            .iter()
            .find(|(index, driver)| driver.status().leader == Some(node_id(*index)))
            .map(|(_, driver)| driver.status());
        let Some(target) = leader
            .filter(|status| status.commit.is_some() && status.commit == status.last_log)
            .and_then(|status| status.commit)
        else {
            continue;
        };
        let settled = |all: &[(usize, Arc<Driver<ControlState>>)]| {
            all.iter()
                .all(|(_, driver)| driver.status().applied == Some(target))
        };
        if !settled(&all) {
            continue;
        }
        states.clear();
        for (index, driver) in &all {
            let Ok(Ok(state)) = timeout(REQUEST_TIMEOUT, driver.read_local()).await else {
                break;
            };
            states.push((
                *index,
                state.workspaces.keys().copied().collect::<BTreeSet<_>>(),
            ));
        }
        if states.len() == lock(world).nodes && settled(&all) {
            break;
        }
        states.clear();
    }
    states
}

async fn converge(world: &Shared) -> simvar::client::ClientResult {
    let states = read_quiescent(world).await;
    let mut world = lock(world);
    if states.len() == world.nodes {
        let reference = states[0].1.clone();
        if std::env::var("SIM_DEBUG").is_ok() {
            for (index, workspaces) in &states {
                let only = workspaces.difference(&reference).collect::<Vec<_>>();
                let missing = reference.difference(workspaces).collect::<Vec<_>>();
                let node_status = world.drivers.get(index).map(|driver| driver.status());
                eprintln!(
                    "n{} writes={} extra={only:?} missing={missing:?} status={node_status:?}",
                    index + 1,
                    workspaces.len()
                );
            }
        }
        if std::env::var("SIM_DEBUG").is_ok() {
            for line in &world.events {
                eprintln!("event {line}");
            }
        }
        for (index, workspaces) in &states {
            if *workspaces != reference {
                world
                    .violations
                    .push(format!("node {} diverged from node 1", index + 1));
            }
        }
        let lost = world
            .acknowledged
            .difference(&reference)
            .map(|missing| format!("acknowledged write {missing} was lost"))
            .collect::<Vec<_>>();
        let unknown = reference
            .difference(&world.proposed)
            .map(|unknown| format!("never-proposed write {unknown} applied"))
            .collect::<Vec<_>>();
        world.violations.extend(lost);
        world.violations.extend(unknown);
    } else {
        let statuses = world
            .drivers
            .iter()
            .map(|(index, driver)| format!("n{}={:?}", index + 1, driver.status()))
            .collect::<Vec<_>>()
            .join("; ");
        let hosts = (0..world.nodes)
            .map(|index| format!("{}={:?}", name(index), fault::host_status(&name(index))))
            .collect::<Vec<_>>()
            .join(" ");
        world.violations.push(format!(
            "cluster did not converge after healing: {statuses}; hosts: {hosts}"
        ));
    }
    let violations = std::mem::take(&mut world.violations);
    let acknowledged = world.acknowledged.len();
    println!(
        "faults={} membership_changes={} snapshot_installs={} proposed={} acknowledged={} converged_writes={}",
        world.faults,
        world.membership_changes,
        world.snapshot_installs,
        world.proposed.len(),
        acknowledged,
        states.first().map_or(0, |(_, workspaces)| workspaces.len()),
    );
    drop(world);
    simvar::record(u64::try_from(acknowledged).unwrap_or(u64::MAX));
    simvar::utils::cancel_simulation();
    if violations.is_empty() {
        Ok(())
    } else {
        Err(boxed(violations.join("\n")))
    }
}

/// Consensus hosts (three by default), a workload and a nemesis.
pub struct ClusterBootstrap {
    pub nodes: usize,
}

impl Default for ClusterBootstrap {
    fn default() -> Self {
        Self { nodes: 3 }
    }
}

impl SimBootstrap for ClusterBootstrap {
    fn build_sim(&self, mut config: SimConfig) -> SimConfig {
        config.duration = Duration::from_hours(1);
        config
    }

    fn on_start(&self, sim: &mut impl Sim) {
        switchy_time::simulator::set_step_multiplier(1);
        let world = Shared::default();
        lock(&world).nodes = self.nodes;
        lock(&world).filesystems = (0..self.nodes)
            .map(|_| Arc::new(Filesystem::new()))
            .collect();
        for index in 0..self.nodes {
            let host_world = world.clone();
            sim.host(name(index), move || host(index, host_world.clone()));
            let crash_world = world.clone();
            sim.on_crash(name(index), move |_| {
                let mut world = lock(&crash_world);
                if std::env::var("SIM_DEBUG").is_ok() {
                    let now = switchy_async::time::Instant::now();
                    world.events.push(format!("{now:?} crash n{}", index + 1));
                }
                world.drivers.remove(&index);
                world.staged.remove(&index);
                world.filesystems[index].crash(&CrashPolicy::power_loss());
            });
        }
        sim.client("workload", workload(world.clone()));
        sim.client("nemesis", nemesis(world));
    }
}
