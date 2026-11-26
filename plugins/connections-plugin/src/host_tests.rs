//! Loopback Iroh tests (relay disabled) of hosting, pairing, the session
//! listener and the connection manager, against a fake server ingress.
use super::*;
use crate::link::PairingLink;
use crate::pairing::client::pair;
use crate::roles::memory::MemoryGrants;
use crate::transport::manager::{ConnectionManager, HostRoute, refusal};
use crate::transport::{PAIR_ALPN, close};
use bmux_ipc::transport::ErasedIpcStream;
use bmux_plugin::stream_admission::{
    StreamAdmissionBinding, StreamIngress, StreamIngressError, StreamIngressHandle,
};
use std::sync::atomic::Ordering;

type Accepted = (ErasedIpcStream, Arc<StreamAdmissionBinding>);

/// Synchronous host entries run on blocking threads with the host runtime
/// entered, never on a runtime worker; tests call them the same way.
fn blocking<T: Send>(work: impl FnOnce() -> T + Send) -> T {
    let runtime = tokio::runtime::Handle::current();
    std::thread::scope(|scope| {
        scope
            .spawn(move || {
                let _entered = runtime.enter();
                work()
            })
            .join()
            .expect("blocking host entry panicked")
    })
}

struct FakeIngress(tokio::sync::mpsc::UnboundedSender<Accepted>);

impl StreamIngress for FakeIngress {
    fn accept(
        &self,
        stream: ErasedIpcStream,
        binding: Arc<StreamAdmissionBinding>,
    ) -> Result<(), StreamIngressError> {
        self.0
            .send((stream, binding))
            .map_err(|_| StreamIngressError::Unavailable)
    }
}

struct Fixture {
    root: tempfile::TempDir,
    host: Host,
    grants: MemoryGrants,
    accepted: tokio::sync::mpsc::UnboundedReceiver<Accepted>,
}

fn settings() -> RemoteSettings {
    RemoteSettings {
        relay: RelayPolicy::Disabled,
        bind_addr: Some("127.0.0.1:0".parse().unwrap()),
        ..RemoteSettings::default()
    }
}

fn hosting() -> Fixture {
    let root = tempfile::tempdir().unwrap();
    let host = Host::new(root.path(), settings()).unwrap();
    let (sender, accepted) = tokio::sync::mpsc::unbounded_channel();
    host.set_ingress(Some(StreamIngressHandle::new(Arc::new(FakeIngress(
        sender,
    )))));
    let grants = MemoryGrants::default();
    blocking(|| host.set_enabled(true, &grants)).unwrap();
    assert!(host.status().enabled);
    Fixture {
        root,
        host,
        grants,
        accepted,
    }
}

fn device() -> Arc<ConnectionManager> {
    ConnectionManager::shared(
        &iroh::SecretKey::generate(),
        &crate::transport::manager::ClientTransport {
            relay: RelayPolicy::Disabled,
            bind_addr: Some("127.0.0.1:0".parse().unwrap()),
        },
    )
}

fn route(fixture: &Fixture) -> HostRoute {
    let status = fixture.host.status();
    HostRoute {
        host_key: status.endpoint_id.parse().unwrap(),
        addrs: status
            .bound_addrs
            .iter()
            .map(|addr| addr.parse().unwrap())
            .collect(),
        relay: None,
    }
}

/// Pair `manager` through a real pairing session; the owner answers `approve`.
async fn pair_device(
    fixture: &Fixture,
    manager: &Arc<ConnectionManager>,
    approve: bool,
) -> anyhow::Result<crate::pairing::client::Paired> {
    pair_named(fixture, manager, "laptop", approve).await
}

async fn pair_named(
    fixture: &Fixture,
    manager: &Arc<ConnectionManager>,
    name: &str,
    approve: bool,
) -> anyhow::Result<crate::pairing::client::Paired> {
    let opened = blocking(|| {
        fixture
            .host
            .begin_pairing(Role::Full, Some(name.into()), Duration::from_secs(60))
    })
    .unwrap();
    let link = PairingLink::decode(&opened.link).unwrap();
    let device_key = manager.endpoint().await.unwrap().id();
    let shown = Arc::new(Mutex::new(None));
    let client = {
        let manager = Arc::clone(manager);
        let shown = Arc::clone(&shown);
        tokio::spawn(async move {
            pair(
                &manager,
                device_key,
                &link,
                "my laptop",
                Role::Full,
                |phrase| {
                    *shown.lock().unwrap() = Some(phrase.to_owned());
                },
            )
            .await
        })
    };
    let Event::Request {
        request_id, phrase, ..
    } = blocking(|| {
        fixture
            .host
            .next_pairing_event(&opened.session_id, Duration::from_secs(10))
    })
    .unwrap()
    else {
        panic!("expected a pairing request");
    };
    let decided = fixture
        .host
        .decide_pairing(&opened.session_id, &request_id, approve, &fixture.grants)
        .unwrap();
    assert_eq!(decided.is_some(), approve);
    let paired = client.await.unwrap();
    assert_eq!(shown.lock().unwrap().as_deref(), Some(phrase.as_str()));
    fixture.host.end_pairing(&opened.session_id);
    paired
}

async fn echo_once(
    manager: &ConnectionManager,
    fixture: &mut Fixture,
) -> Arc<StreamAdmissionBinding> {
    let (mut stream, path) = manager
        .open_stream(&route(fixture), Duration::from_secs(10))
        .await
        .unwrap();
    assert_eq!(path, "direct");
    let envelope = bmux_ipc::Envelope::new(7, bmux_ipc::EnvelopeKind::Request, b"ping".to_vec());
    stream.send_envelope(&envelope).await.unwrap();
    let (mut served, binding) =
        tokio::time::timeout(Duration::from_secs(10), fixture.accepted.recv())
            .await
            .unwrap()
            .unwrap();
    assert_eq!(served.recv_envelope().await.unwrap().payload, b"ping");
    binding
}

#[tokio::test(flavor = "multi_thread")]
async fn pairing_then_session_streams_reach_the_server_as_the_principal() {
    let mut fixture = hosting();
    let manager = device();
    let paired = pair_device(&fixture, &manager, true).await.unwrap();
    let devices = fixture.host.list_devices(&fixture.grants).unwrap();
    assert_eq!(devices.len(), 1);
    assert_eq!(devices[0].name, "laptop");
    assert_eq!(devices[0].principal, paired.principal);
    assert_eq!(
        fixture.grants.get(paired.principal).unwrap(),
        Some(Role::Full)
    );
    let binding = echo_once(&manager, &mut fixture).await;
    assert_eq!(binding.subject_id(), paired.principal);
    // A second client in this process shares the endpoint and the connection.
    let again = echo_once(&manager, &mut fixture).await;
    assert_eq!(again.connection_id(), binding.connection_id());
    assert_eq!(manager.live_connections().await, 1);
    assert_eq!(
        manager.endpoint().await.unwrap().bound_sockets(),
        manager.endpoint().await.unwrap().bound_sockets()
    );
    assert_eq!(fixture.host.live_connections().len(), 1);

    // Role changes keep the connection; the authorizer applies the new role.
    let changed = fixture
        .host
        .set_role("laptop", Role::ViewOnly, &fixture.grants)
        .unwrap();
    assert_eq!(changed.role, Role::ViewOnly);
    assert_eq!(
        fixture.grants.get(paired.principal).unwrap(),
        Some(Role::ViewOnly)
    );
    assert!(!binding.is_revoked());

    // Revocation closes the live connection now and refuses reconnects.
    let connection = manager
        .connection(&route(&fixture), Duration::from_secs(5))
        .await
        .unwrap();
    fixture.host.revoke("laptop", &fixture.grants).unwrap();
    tokio::time::timeout(Duration::from_secs(1), binding.revoked())
        .await
        .expect("revocation reaches the binding within 1s");
    tokio::time::timeout(Duration::from_secs(2), connection.closed())
        .await
        .unwrap();
    assert!(refusal(&connection).unwrap().contains("revoked"));
    assert_eq!(fixture.grants.get(paired.principal).unwrap(), None);
    let reconnect = manager
        .connection(&route(&fixture), Duration::from_secs(5))
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(2), reconnect.closed())
        .await
        .unwrap();
    // A revoked device is told so (it knows its own key), not "not paired".
    assert!(refusal(&reconnect).unwrap().contains("revoked"));
    assert!(fixture.accepted.try_recv().is_err());

    // The audit log saw the whole story, oldest first, without secrets.
    // Connection events are written off the runtime workers: wait for them.
    let wanted = [
        AuditKind::Paired,
        AuditKind::Connected,
        AuditKind::RoleChanged,
        AuditKind::Revoked,
        AuditKind::Disconnected,
        AuditKind::Refused,
    ];
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let events = loop {
        let events = blocking(|| fixture.host.audit_log(100, &fixture.grants)).unwrap();
        let kinds: Vec<_> = events.iter().map(|event| event.kind).collect();
        if wanted.iter().all(|kind| kinds.contains(kind)) || std::time::Instant::now() > deadline {
            break events;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    let kinds: Vec<_> = events.iter().map(|event| event.kind).collect();
    for kind in wanted {
        assert!(kinds.contains(&kind), "{kind:?} missing from {kinds:?}");
    }
    let position = |kind| kinds.iter().position(|k| *k == kind).unwrap();
    assert!(position(AuditKind::Paired) < position(AuditKind::Revoked));
    assert!(
        events
            .iter()
            .all(|event| event.device_name.as_deref() == Some("laptop"))
    );
    blocking(|| fixture.host.stop()).unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn unknown_devices_are_closed_at_the_handshake_and_pairing_needs_a_session() {
    let mut fixture = hosting();
    let stranger = device();
    let connection = stranger
        .connection(&route(&fixture), Duration::from_secs(10))
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(2), connection.closed())
        .await
        .unwrap();
    let iroh::endpoint::ConnectionError::ApplicationClosed(closed) =
        connection.close_reason().unwrap()
    else {
        panic!("expected an application close");
    };
    assert_eq!(closed.error_code.into_inner(), u64::from(close::NOT_PAIRED));
    assert!(fixture.accepted.try_recv().is_err());
    // Without an open pairing session the pairing ALPN is not offered.
    assert!(
        stranger
            .dial(&route(&fixture), PAIR_ALPN, Duration::from_secs(5))
            .await
            .is_err()
    );
    blocking(|| fixture.host.stop()).unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn rejected_and_failed_approvals_leave_no_device() {
    let fixture = hosting();
    let manager = device();
    let rejected = pair_device(&fixture, &manager, false).await.unwrap_err();
    assert!(rejected.to_string().contains("rejected"), "{rejected:#}");
    assert_eq!(fixture.host.list_devices(&fixture.grants).unwrap(), []);
    fixture.grants.fail.store(true, Ordering::SeqCst);
    let opened = blocking(|| {
        fixture
            .host
            .begin_pairing(Role::Full, None, Duration::from_secs(60))
    })
    .unwrap();
    let link = PairingLink::decode(&opened.link).unwrap();
    let device_key = manager.endpoint().await.unwrap().id();
    let client = {
        let manager = Arc::clone(&manager);
        tokio::spawn(
            async move { pair(&manager, device_key, &link, "x", Role::Full, |_| {}).await },
        )
    };
    let Event::Request { request_id, .. } = blocking(|| {
        fixture
            .host
            .next_pairing_event(&opened.session_id, Duration::from_secs(10))
    })
    .unwrap() else {
        panic!("expected a request");
    };
    assert!(
        fixture
            .host
            .decide_pairing(&opened.session_id, &request_id, true, &fixture.grants)
            .is_err()
    );
    assert!(
        client
            .await
            .unwrap()
            .unwrap_err()
            .to_string()
            .contains("could not complete")
    );
    assert_eq!(fixture.host.list_devices(&fixture.grants).unwrap(), []);
    blocking(|| fixture.host.stop()).unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn hosting_policy_and_single_host_lock() {
    let fixture = hosting();
    // Another runtime of the same user cannot host at the same time.
    let second = Host::new(fixture.root.path(), settings()).unwrap();
    let (sender, _accepted) = tokio::sync::mpsc::unbounded_channel();
    second.set_ingress(Some(StreamIngressHandle::new(Arc::new(FakeIngress(
        sender,
    )))));
    assert!(blocking(|| second.start(&fixture.grants)).is_err());
    assert_eq!(preference(fixture.root.path()).unwrap(), Some(true));
    blocking(|| fixture.host.set_enabled(false, &fixture.grants)).unwrap();
    assert!(!fixture.host.status().enabled);
    assert_eq!(fixture.host.status().source, "remote disable");
    blocking(|| second.start(&fixture.grants)).unwrap();
    blocking(|| second.stop()).unwrap();
    // An explicit configuration prohibition wins over everything.
    let root = tempfile::tempdir().unwrap();
    let prohibited = Host::new(
        root.path(),
        RemoteSettings {
            allow_full_runtime_ingress: Some(false),
            ..settings()
        },
    )
    .unwrap();
    assert!(blocking(|| prohibited.set_enabled(true, &fixture.grants)).is_err());
    assert_eq!(prohibited.status().source, "config: prohibited");
    // Pairing needs hosting.
    assert!(
        prohibited
            .begin_pairing(Role::Full, None, Duration::from_secs(60))
            .is_err()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_changed_host_key_is_reported_as_such() {
    let fixture = hosting();
    let manager = device();
    let mut wrong = route(&fixture);
    wrong.host_key = iroh::SecretKey::generate().public();
    let error = manager
        .open_stream(&wrong, Duration::from_secs(5))
        .await
        .unwrap_err();
    let explained = crate::connector::explain("desk", error).to_string();
    assert!(explained.contains("different host key"), "{explained}");
    assert!(explained.contains("bmux remote forget desk"));
    blocking(|| fixture.host.stop()).unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn links_carry_external_addresses_and_the_configured_relay() {
    let root = tempfile::tempdir().unwrap();
    let external: std::net::SocketAddr = "192.0.2.7:4433".parse().unwrap();
    let host = Host::new(
        root.path(),
        RemoteSettings {
            external_addrs: vec![external],
            relay: RelayPolicy::Custom {
                urls: vec!["http://127.0.0.1:9/".into()],
            },
            ..settings()
        },
    )
    .unwrap();
    let (sender, _accepted) = tokio::sync::mpsc::unbounded_channel();
    host.set_ingress(Some(StreamIngressHandle::new(Arc::new(FakeIngress(
        sender,
    )))));
    blocking(|| host.set_enabled(true, &MemoryGrants::default())).unwrap();
    let opened = blocking(|| host.begin_pairing(Role::Full, None, Duration::ZERO)).unwrap();
    let link = PairingLink::decode(&opened.link).unwrap();
    assert_eq!(link.addrs.first(), Some(&external));
    assert!(link.addrs.iter().any(|addr| addr.ip().is_loopback()));
    assert_eq!(link.relay.as_deref(), Some("http://127.0.0.1:9/"));
    assert!(opened.expires_at > crate::trust::now_secs() + 500);
    blocking(|| host.stop()).unwrap();
}

/// Opens a stream to the host and sends one frame, so the host sees it.
async fn open_and_send(
    manager: &ConnectionManager,
    fixture: &Fixture,
) -> bmux_ipc::transport::ErasedIpcStream {
    let (mut stream, _) = manager
        .open_stream(&route(fixture), Duration::from_secs(10))
        .await
        .unwrap();
    let envelope = bmux_ipc::Envelope::new(1, bmux_ipc::EnvelopeKind::Request, b"x".to_vec());
    stream.send_envelope(&envelope).await.unwrap();
    stream
}

#[tokio::test(flavor = "multi_thread")]
async fn remote_streams_are_bounded_per_connection_and_in_total() {
    use crate::transport::listener::MAX_REMOTE_STREAMS;
    let mut fixture = hosting();
    let first = device();
    let second = device();
    pair_named(&fixture, &first, "first", true).await.unwrap();
    pair_named(&fixture, &second, "second", true).await.unwrap();

    // One connection holds at most 32 streams: the 33rd waits for QUIC credit.
    let mut clients = Vec::new();
    let mut served = Vec::new();
    for _ in 0..32 {
        clients.push(open_and_send(&first, &fixture).await);
        served.push(
            tokio::time::timeout(Duration::from_secs(10), fixture.accepted.recv())
                .await
                .unwrap()
                .unwrap(),
        );
    }
    let waiting = tokio::time::timeout(Duration::from_secs(1), async {
        let (mut stream, _) = first
            .open_stream(&route(&fixture), Duration::from_secs(10))
            .await
            .unwrap();
        let envelope = bmux_ipc::Envelope::new(1, bmux_ipc::EnvelopeKind::Request, b"x".to_vec());
        stream.send_envelope(&envelope).await
    })
    .await;
    assert!(waiting.is_err(), "a 33rd stream on one connection opened");
    assert!(fixture.accepted.try_recv().is_err());

    // Every device together holds at most MAX_REMOTE_STREAMS; past it a stream is reset.
    for _ in 32..MAX_REMOTE_STREAMS {
        clients.push(open_and_send(&second, &fixture).await);
        served.push(
            tokio::time::timeout(Duration::from_secs(10), fixture.accepted.recv())
                .await
                .unwrap()
                .unwrap(),
        );
    }
    let mut refused = open_and_send(&second, &fixture).await;
    assert!(
        tokio::time::timeout(Duration::from_secs(5), refused.recv_envelope())
            .await
            .unwrap()
            .is_err(),
        "a stream past the total limit was served"
    );
    assert!(fixture.accepted.try_recv().is_err());

    // A stream the server finishes frees its share.
    drop(served.pop());
    let _admitted = open_and_send(&second, &fixture).await;
    let (mut stream, _) = tokio::time::timeout(Duration::from_secs(10), fixture.accepted.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stream.recv_envelope().await.unwrap().payload, b"x");
    drop(clients);
    blocking(|| fixture.host.stop()).unwrap();
}
