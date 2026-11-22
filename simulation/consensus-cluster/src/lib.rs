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
    ClientError, Driver, DriverConfig, Jitter, Reply, Storage, Transport,
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

pub const NODES: [&str; 3] = ["n1", "n2", "n3"];
const CLUSTER: &str = "simulated-cluster";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(2);

fn node_id(index: usize) -> NodeId {
    NodeId::from(index as u64 + 1)
}

fn index_of(id: NodeId) -> Option<usize> {
    (0..NODES.len()).find(|index| node_id(*index) == id)
}

fn boxed(message: impl Into<String>) -> Box<dyn std::error::Error + Send> {
    Box::new(io::Error::other(message.into()))
}

/// What the hosts, workload and nemesis share outside the network.
#[derive(Default)]
struct World {
    filesystems: Vec<Arc<Filesystem>>,
    /// Per run, so parallel runs never contend through process-wide state.
    preflight: bmux_cluster_plugin::owned_vote_store::PreflightAdmission,
    drivers: BTreeMap<usize, Arc<Driver<ControlState>>>,
    proposed: BTreeSet<uuid::Uuid>,
    acknowledged: BTreeSet<uuid::Uuid>,
    /// The membership has all three voters; faults may begin.
    formed: bool,
    /// The nemesis healed everything; the workload stops proposing.
    quiet: bool,
    violations: Vec<String>,
    faults: usize,
}

type Shared = Arc<Mutex<World>>;

fn lock(world: &Shared) -> std::sync::MutexGuard<'_, World> {
    world.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Runs every storage operation inside the host's filesystem namespace.
struct Scoped {
    filesystem: Arc<Filesystem>,
    inner: OwnedStorage,
}

impl Storage for Scoped {
    type View = ControlState;

    fn write(&mut self, write: &Write) -> io::Result<()> {
        let filesystem = self.filesystem.clone();
        with_filesystem(&filesystem, || self.inner.write(write))
    }

    fn install_snapshot(
        &mut self,
        from: NodeId,
        last_included: LogIdentity,
        membership: &StoredMembership,
    ) -> io::Result<()> {
        let filesystem = self.filesystem.clone();
        with_filesystem(&filesystem, || {
            self.inner.install_snapshot(from, last_included, membership)
        })
    }

    fn apply(&mut self, through: LogIdentity) -> io::Result<Vec<(LogIdentity, Vec<u8>)>> {
        let filesystem = self.filesystem.clone();
        with_filesystem(&filesystem, || self.inner.apply(through))
    }

    fn compact(&mut self) -> io::Result<Option<(LogIdentity, StoredMembership)>> {
        let filesystem = self.filesystem.clone();
        with_filesystem(&filesystem, || self.inner.compact())
    }

    fn read(&mut self) -> io::Result<ControlState> {
        let filesystem = self.filesystem.clone();
        with_filesystem(&filesystem, || self.inner.read())
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
            let target = index_of(to)?;
            let (from, peer) = (NODES[local], NODES[target]);
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

    fn send_snapshot(&self, _: NodeId, _: Term, _: u64) -> Reply {
        // Snapshot transfer is exercised once compaction is enabled here.
        Box::pin(async { None })
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
            let membership = Membership::new(vec![vec![founder]], vec![(founder, NODES[0].into())])
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
        filesystem,
        inner: OwnedStorage::new(
            store,
            Box::new(|_, _| Err(io::Error::other("snapshot transfer not exercised"))),
        ),
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
            // Compaction would require snapshot transfer to followers; keep it off
            // until the simulated transport stages snapshots.
            compaction_interval: u64::MAX,
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

fn records() -> Vec<(NodeId, String)> {
    (0..NODES.len())
        .map(|index| (node_id(index), NODES[index].to_owned()))
        .collect()
}

/// Apply a membership change on whichever node currently leads, retrying.
async fn change(world: &Shared, voters: &[usize]) {
    let target = Membership::new(
        vec![voters.iter().map(|index| node_id(*index)).collect()],
        records(),
    )
    .expect("valid simulated membership");
    loop {
        for (_, driver) in drivers(world) {
            if let Ok(Ok(_)) =
                timeout(REQUEST_TIMEOUT, driver.change_membership(target.clone())).await
            {
                return;
            }
        }
        sleep(Duration::from_millis(50)).await;
    }
}

/// Wait until a leader reports the uniform configuration with `count` voters committed.
async fn wait_voters(world: &Shared, count: usize) {
    loop {
        let formed = drivers(world).iter().any(|(index, driver)| {
            let status = driver.status();
            status.leader == Some(node_id(*index)) && status.committed_voters == count
        });
        if formed {
            return;
        }
        sleep(Duration::from_millis(50)).await;
    }
}

/// Form the cluster, then propose and read until the nemesis is done.
async fn workload(world: Shared) -> simvar::client::ClientResult {
    // Learner records first, then promotion through joint consensus.
    change(&world, &[0]).await;
    change(&world, &[0, 1, 2]).await;
    wait_voters(&world, NODES.len()).await;
    lock(&world).formed = true;
    let mut next = 0u128;
    while !lock(&world).quiet {
        next += 1;
        let id = uuid::Uuid::from_u128(next);
        lock(&world).proposed.insert(id);
        let mut outcome = None;
        for (_, driver) in drivers(&world) {
            match timeout(REQUEST_TIMEOUT, driver.propose(command(id))).await {
                Ok(Ok(_)) => {
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
        let victim = NODES[rng().gen_range(0..NODES.len())];
        let others: Vec<_> = NODES.iter().filter(|name| **name != victim).collect();
        match rng().gen_range(0..6) {
            0 => network::partition([victim], others),
            1 => network::partition_one_way(others, [victim]),
            2 => network::isolate(victim),
            3 => network::set_default_policy(
                LinkPolicy::reliable()
                    .with_latency(Duration::from_millis(1), Duration::from_millis(60))
                    .with_loss(0.2),
            ),
            4 => fault::bounce_after(victim, Duration::from_millis(rng().gen_range(0..1000))),
            _ => {
                network::heal();
                network::set_default_policy(reliable);
            }
        }
    }
    network::heal();
    network::set_default_policy(reliable);
    lock(&world).quiet = true;
    while NODES
        .iter()
        .any(|name| fault::host_status(name) != Some(fault::HostStatus::Running))
    {
        sleep(Duration::from_millis(50)).await;
    }
    converge(&world).await
}

async fn converge(world: &Shared) -> simvar::client::ClientResult {
    let mut states = Vec::new();
    for _ in 0..600 {
        sleep(Duration::from_millis(100)).await;
        // A leader commits a no-op of its term, so idle followers still converge.
        let all = drivers(world);
        if all.len() < NODES.len() {
            continue;
        }
        let leader_commit = all
            .iter()
            .find(|(index, driver)| driver.status().leader == Some(node_id(*index)))
            .and_then(|(_, driver)| driver.status().commit);
        let Some(target) = leader_commit else {
            continue;
        };
        if all
            .iter()
            .any(|(_, driver)| driver.status().applied.map(|a| a.index) != Some(target.index))
        {
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
        if states.len() == NODES.len() {
            break;
        }
    }
    let mut world = lock(world);
    if states.len() == NODES.len() {
        let reference = states[0].1.clone();
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
        let hosts = NODES
            .iter()
            .map(|name| format!("{name}={:?}", fault::host_status(name)))
            .collect::<Vec<_>>()
            .join(" ");
        world.violations.push(format!(
            "cluster did not converge after healing: {statuses}; hosts: {hosts}"
        ));
    }
    let violations = std::mem::take(&mut world.violations);
    let acknowledged = world.acknowledged.len();
    println!(
        "faults={} proposed={} acknowledged={} converged_writes={}",
        world.faults,
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

/// Three consensus hosts, a workload and a nemesis.
pub struct ClusterBootstrap;

impl SimBootstrap for ClusterBootstrap {
    fn build_sim(&self, mut config: SimConfig) -> SimConfig {
        config.duration = Duration::from_hours(1);
        config
    }

    fn on_start(&self, sim: &mut impl Sim) {
        switchy_time::simulator::set_step_multiplier(1);
        let world = Shared::default();
        lock(&world).filesystems = NODES.iter().map(|_| Arc::new(Filesystem::new())).collect();
        for (index, name) in NODES.iter().enumerate() {
            let host_world = world.clone();
            sim.host(*name, move || host(index, host_world.clone()));
            let crash_world = world.clone();
            sim.on_crash(*name, move |_| {
                let mut world = lock(&crash_world);
                world.drivers.remove(&index);
                world.filesystems[index].crash(&CrashPolicy::power_loss());
            });
        }
        sim.client("workload", workload(world.clone()));
        sim.client("nemesis", nemesis(world));
    }
}
