//! Actual TLS response loss after an owner effect, not an in-process retry test.
use bmux_presenter::{DeckBuilder, SlideBuilder};
use bmux_presenter_runtime::{
    Action, Credential, Host, HostConfig, HostError, InteractionMode, Owner, OwnerContext,
    OwnerUpdate, Poll, Request, Response, Scopes, Submission, SurfaceInput,
    transport::{Client, NetworkConfig, serve_tls},
};
use std::{
    io,
    net::SocketAddr,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::watch,
    time::timeout,
};

const DEADLINE: Duration = Duration::from_secs(5);
const MESSAGE_LIMIT: usize = 4 * 1024 * 1024;

struct CountingOwner(Arc<AtomicUsize>);
impl Owner for CountingOwner {
    fn demo_input(
        &mut self,
        _context: OwnerContext<'_>,
        _input: &SurfaceInput,
    ) -> Result<OwnerUpdate, HostError> {
        Err(HostError::Unauthorized)
    }

    fn effect(
        &mut self,
        _context: OwnerContext<'_>,
        name: &str,
        input: &str,
    ) -> Result<OwnerUpdate, HostError> {
        assert_eq!((name, input), ("increment", "once"));
        self.0.fetch_add(1, Ordering::SeqCst);
        // An external effect happened, but its completion cannot be confirmed.
        Err(HostError::EffectUncertain)
    }
}

/// Transparent TCP relay: the real client/server still negotiate and verify TLS.
/// Forward handshake/bootstrap normally, then sever BOTH directions upon the first
/// server TLS bytes after owner invocation. The effect response never reaches the
/// client. No fabricated client error and no replay/reconnect in this relay.
async fn lose_effect_response(
    listener: TcpListener,
    upstream: SocketAddr,
    calls: Arc<AtomicUsize>,
) -> io::Result<usize> {
    let (mut downstream, _) = listener.accept().await?;
    let mut upstream = TcpStream::connect(upstream).await?;
    let (mut down_read, mut down_write) = downstream.split();
    let (mut up_read, mut up_write) = upstream.split();
    let forward = tokio::io::copy(&mut down_read, &mut up_write);
    let reply = async {
        let mut buffer = [0_u8; 8192];
        loop {
            let bytes = up_read.read(&mut buffer).await?;
            if bytes == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "no effect reply",
                ));
            }
            if calls.load(Ordering::SeqCst) != 0 {
                return Ok(bytes);
            }
            down_write.write_all(&buffer[..bytes]).await?;
        }
    };
    tokio::select! {
        result = reply => result,
        result = forward => {
            result?;
            Err(io::Error::new(io::ErrorKind::UnexpectedEof, "client closed before effect reply"))
        }
    }
    // Dropping both sockets closes the relay without forwarding the withheld bytes.
}

fn tls_configs() -> (Arc<rustls::ServerConfig>, Arc<rustls::ClientConfig>) {
    let certificate = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let server = rustls::ServerConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(
            vec![certificate.cert.der().clone()],
            rustls::pki_types::PrivateKeyDer::Pkcs8(certificate.signing_key.serialize_der().into()),
        )
        .unwrap();
    let mut roots = rustls::RootCertStore::empty();
    roots.add(certificate.cert.der().clone()).unwrap();
    let client = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth();
    (Arc::new(server), Arc::new(client))
}

async fn connect(address: SocketAddr, tls: Arc<rustls::ClientConfig>) -> Client {
    Client::connect_tls(address, "localhost".into(), tls, MESSAGE_LIMIT, DEADLINE)
        .await
        .unwrap()
}

async fn authenticate(client: &mut Client, secret: &str) -> u64 {
    let Response::Authenticated {
        next_action_id: Some(next),
        ..
    } = client
        .authenticate(Some(secret.to_owned()), InteractionMode::Shared)
        .await
        .unwrap()
    else {
        panic!("private principal authentication expected");
    };
    next
}

async fn snapshot(client: &mut Client) -> bmux_presenter_runtime::Snapshot {
    let Response::Poll(Poll::Snapshot { snapshot, .. }) =
        client.request(Request::Snapshot).await.unwrap()
    else {
        panic!("snapshot expected");
    };
    *snapshot
}

#[tokio::test]
async fn tls_lost_uncertain_effect_reply_reconnect_and_retry_never_reinvoke_owner() {
    let calls = Arc::new(AtomicUsize::new(0));
    let mut host = Host::new(
        DeckBuilder::new("effect-network")
            .slide(
                SlideBuilder::new("one")
                    .text("visible", "effect fixture")
                    .build(),
            )
            .build()
            .unwrap(),
        vec![],
        HostConfig::default(),
    )
    .unwrap();
    host.set_owner(Box::new(CountingOwner(Arc::clone(&calls))))
        .unwrap();
    let credential = Credential::generate(
        Scopes {
            effects: true,
            ..Scopes::default()
        },
        Duration::from_secs(60),
    );
    let secret = credential.secret.clone();
    host.add_credential(credential).unwrap();
    let host = Arc::new(Mutex::new(host));
    let (server_tls, client_tls) = tls_configs();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (shutdown, receiver) = watch::channel(false);
    let server = tokio::spawn(serve_tls(
        listener,
        host,
        server_tls,
        NetworkConfig::default(),
        receiver,
    ));
    let relay_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let relay_address = relay_listener.local_addr().unwrap();
    let relay = tokio::spawn(lose_effect_response(
        relay_listener,
        address,
        Arc::clone(&calls),
    ));

    let mut client = connect(relay_address, Arc::clone(&client_tls)).await;
    assert_eq!(authenticate(&mut client, &secret).await, 1);
    let before = snapshot(&mut client).await;
    let submission = Submission {
        action_id: 1,
        expected: before.cursor.clone(),
        action: Action::Effect {
            name: "increment".into(),
            input: "once".into(),
        },
    };
    // Use the production Client. It must report network loss, not the server's
    // EffectUncertain response, because the encrypted reply was withheld.
    assert!(
        client
            .request(Request::Submit(submission.clone()))
            .await
            .is_err()
    );
    let withheld_bytes = timeout(DEADLINE, relay).await.unwrap().unwrap().unwrap();
    assert!(
        withheld_bytes > 0,
        "relay must actually discard server TLS output"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    // The poisoned connection cannot silently retry the effect.
    let poisoned = client
        .request(Request::Submit(submission.clone()))
        .await
        .expect_err("lost response must poison the production client");
    assert_eq!(poisoned.kind(), io::ErrorKind::NotConnected);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    drop(client);

    let mut resumed = connect(address, client_tls).await;
    assert_eq!(authenticate(&mut resumed, &secret).await, 2);
    let committed = snapshot(&mut resumed).await;
    assert_eq!(committed.cursor.run_id, before.cursor.run_id);
    assert_eq!(committed.cursor.deck_revision, before.cursor.deck_revision);
    assert_eq!(committed.cursor.revision, before.cursor.revision + 1);
    for _ in 0..2 {
        assert!(matches!(
            resumed
                .request(Request::Submit(submission.clone()))
                .await
                .unwrap(),
            Response::Error(HostError::EffectUncertain)
        ));
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "retry invoked external owner effect again"
        );
        assert_eq!(snapshot(&mut resumed).await.cursor, committed.cursor);
    }
    let mut conflicting = submission;
    conflicting.action = Action::Effect {
        name: "increment".into(),
        input: "different".into(),
    };
    assert!(matches!(
        resumed.request(Request::Submit(conflicting)).await.unwrap(),
        Response::Error(HostError::ActionConflict)
    ));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(snapshot(&mut resumed).await.cursor, committed.cursor);
    drop(resumed);
    shutdown.send(true).unwrap();
    timeout(DEADLINE, server).await.unwrap().unwrap().unwrap();
}
