use super::*;
use crate::consensus_engine::{Config, Recovered};
use crate::consensus_model::{Entry, EntryPayload, HardState};
use std::collections::BTreeSet;
use std::sync::Mutex;

fn node(id: u64) -> NodeId {
    NodeId::from(id)
}

/// Durable state shared with the test so a restart recovers exactly what was written.
#[derive(Debug, Clone, Default)]
struct Disk {
    hard_state: Option<HardState>,
    entries: Vec<Entry>,
    commit: Option<LogIdentity>,
    applied: Option<LogIdentity>,
}

struct MemoryStorage(Arc<Mutex<Disk>>);

impl Storage for MemoryStorage {
    type View = Option<LogIdentity>;

    fn write(&mut self, write: &Write) -> io::Result<()> {
        let mut disk = self.0.lock().unwrap();
        if let Some(state) = write.hard_state {
            disk.hard_state = Some(state);
        }
        if let Some(from) = write.truncate_from {
            disk.entries.retain(|entry| entry.identity.index < from);
        }
        disk.entries.extend(write.entries.iter().cloned());
        if write.commit.is_some() {
            disk.commit = write.commit;
        }
        drop(disk);
        Ok(())
    }

    fn install_snapshot(
        &mut self,
        _: NodeId,
        _: LogIdentity,
        _: &StoredMembership,
    ) -> io::Result<()> {
        Err(io::Error::other("snapshots are not exercised here"))
    }

    fn apply(&mut self, through: LogIdentity) -> io::Result<Vec<(LogIdentity, Vec<u8>)>> {
        let mut disk = self.0.lock().unwrap();
        let from = disk.applied.map_or(0, |applied| applied.index.0 + 1);
        let outcomes = disk
            .entries
            .iter()
            .filter(|entry| (from..=through.index.0).contains(&entry.identity.index.0))
            .filter_map(|entry| match &entry.payload {
                EntryPayload::Application(bytes) => Some((entry.identity, bytes.clone())),
                _ => None,
            })
            .collect();
        disk.applied = Some(through);
        drop(disk);
        Ok(outcomes)
    }

    fn compact(&mut self) -> io::Result<Option<(LogIdentity, StoredMembership)>> {
        Ok(None)
    }

    fn read(&mut self) -> io::Result<Self::View> {
        Ok(self.0.lock().unwrap().applied)
    }

    fn close(self) -> io::Result<()> {
        Ok(())
    }
}

type Nodes = Arc<Mutex<BTreeMap<NodeId, Arc<Driver<Option<LogIdentity>>>>>>;

struct MemoryTransport {
    local: NodeId,
    nodes: Nodes,
    down: Arc<Mutex<BTreeSet<NodeId>>>,
}

impl Transport for MemoryTransport {
    fn call(&self, to: NodeId, message: Message) -> Reply {
        let peer = self.nodes.lock().unwrap().get(&to).cloned();
        let down = self.down.lock().unwrap().clone();
        let local = self.local;
        Box::pin(async move {
            if down.contains(&local) || down.contains(&to) {
                return None;
            }
            peer?.request(local, message).await
        })
    }

    fn send_snapshot(&self, _: NodeId, _: Term, _: u64) -> Reply {
        Box::pin(async { None })
    }
}

fn founding(voters: &[u64]) -> Disk {
    let identity = LogIdentity {
        term: Term(0),
        leader: node(voters[0]),
        index: LogIndex(0),
    };
    let membership = Membership::new(
        vec![voters.iter().copied().map(node).collect()],
        voters
            .iter()
            .map(|id| (node(*id), format!("node-{id}")))
            .collect(),
    )
    .unwrap();
    Disk {
        hard_state: Some(HardState {
            term: Term(0),
            voted_for: None,
        }),
        entries: vec![Entry {
            identity,
            payload: EntryPayload::Membership(membership),
        }],
        commit: Some(identity),
        applied: None,
    }
}

struct Cluster {
    nodes: Nodes,
    disks: BTreeMap<NodeId, Arc<Mutex<Disk>>>,
    down: Arc<Mutex<BTreeSet<NodeId>>>,
}

impl Cluster {
    fn new(voters: &[u64]) -> Self {
        let cluster = Self {
            nodes: Arc::default(),
            disks: voters
                .iter()
                .map(|id| (node(*id), Arc::new(Mutex::new(founding(voters)))))
                .collect(),
            down: Arc::default(),
        };
        for id in voters {
            cluster.start(node(*id));
        }
        cluster
    }

    fn start(&self, id: NodeId) {
        let disk = self.disks[&id].lock().unwrap().clone();
        let (engine, effects) = Engine::recover(
            id,
            Config::default(),
            Recovered {
                hard_state: disk.hard_state.unwrap(),
                snapshot: None,
                entries: disk.entries,
                commit: disk.commit,
                applied: disk.applied,
            },
        )
        .unwrap();
        let mut state = u64::from_le_bytes(id.as_bytes()[24..].try_into().unwrap());
        let jitter: Jitter = Box::new(move |min, max| {
            // A per-node deterministic stream keeps tests reproducible.
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1);
            let span = u64::try_from(max.saturating_sub(min).as_millis().max(1)).unwrap_or(1);
            min + Duration::from_millis((state >> 33) % span)
        });
        let driver = Driver::start(
            engine,
            effects,
            MemoryStorage(self.disks[&id].clone()),
            Arc::new(MemoryTransport {
                local: id,
                nodes: self.nodes.clone(),
                down: self.down.clone(),
            }),
            jitter,
            DriverConfig::default(),
        );
        self.nodes.lock().unwrap().insert(id, Arc::new(driver));
    }

    fn driver(&self, id: NodeId) -> Arc<Driver<Option<LogIdentity>>> {
        self.nodes.lock().unwrap()[&id].clone()
    }

    async fn leader(&self, excluding: &BTreeSet<NodeId>) -> NodeId {
        for _ in 0..200 {
            let drivers = self.nodes.lock().unwrap().clone();
            for (id, driver) in drivers {
                let status = driver.status();
                if !excluding.contains(&id)
                    && status.leader == Some(id)
                    && status
                        .commit
                        .is_some_and(|commit| commit.term == status.term)
                {
                    return id;
                }
            }
            switchy_async::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("no leader elected");
    }

    async fn stop(&self, id: NodeId) {
        self.down.lock().unwrap().insert(id);
        let driver = self.nodes.lock().unwrap().remove(&id).unwrap();
        if let Ok(driver) = Arc::try_unwrap(driver) {
            driver.shutdown().await;
        }
    }
}

#[switchy_async::test]
async fn three_drivers_elect_apply_and_read() {
    let cluster = Cluster::new(&[1, 2, 3]);
    let leader = cluster.leader(&BTreeSet::new()).await;
    let driver = cluster.driver(leader);
    assert_eq!(
        driver.propose(b"hello".to_vec()).await,
        Ok(b"hello".to_vec())
    );
    let applied = driver.read().await.unwrap().unwrap();
    assert!(applied.index.0 >= 2);
    let follower = [1, 2, 3]
        .map(node)
        .into_iter()
        .find(|id| *id != leader)
        .unwrap();
    assert!(matches!(
        cluster.driver(follower).propose(b"x".to_vec()).await,
        Err(ClientError::Rejected(Rejection::NotLeader { .. }))
    ));
}

#[switchy_async::test]
async fn failover_elects_a_new_leader_that_keeps_committed_writes() {
    let cluster = Cluster::new(&[1, 2, 3]);
    let first = cluster.leader(&BTreeSet::new()).await;
    cluster
        .driver(first)
        .propose(b"durable".to_vec())
        .await
        .unwrap();
    cluster.stop(first).await;
    let second = cluster.leader(&BTreeSet::from([first])).await;
    assert_ne!(first, second);
    let driver = cluster.driver(second);
    assert_eq!(
        driver.propose(b"after".to_vec()).await,
        Ok(b"after".to_vec())
    );
    let disk = cluster.disks[&second].lock().unwrap().clone();
    assert!(
        disk.entries
            .iter()
            .any(|entry| { entry.payload == EntryPayload::Application(b"durable".to_vec()) })
    );
}

#[switchy_async::test]
async fn restarted_node_recovers_its_durable_state_and_rejoins() {
    let cluster = Cluster::new(&[1, 2, 3]);
    let leader = cluster.leader(&BTreeSet::new()).await;
    cluster
        .driver(leader)
        .propose(b"one".to_vec())
        .await
        .unwrap();
    let follower = [1, 2, 3]
        .map(node)
        .into_iter()
        .find(|id| *id != leader)
        .unwrap();
    cluster.stop(follower).await;
    cluster
        .driver(leader)
        .propose(b"two".to_vec())
        .await
        .unwrap();
    cluster.down.lock().unwrap().remove(&follower);
    cluster.start(follower);
    let expected = cluster.driver(leader).status().commit;
    for _ in 0..200 {
        if cluster.driver(follower).status().commit == expected {
            return;
        }
        switchy_async::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("restarted follower did not catch up");
}
