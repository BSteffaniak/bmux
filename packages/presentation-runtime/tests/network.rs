use bmux_presentation::{Deck, Metadata};
use bmux_presentation_runtime::{
    Host, HostConfig, InteractionMode, PROTOCOL_VERSION, Poll, Request, Response,
    transport::{Client, NetworkConfig, serve_local, serve_tls},
};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{net::TcpListener, sync::watch};
fn host() -> Arc<Mutex<Host>> {
    Arc::new(Mutex::new(
        Host::new(
            Deck {
                id: "deck".into(),
                metadata: Metadata::default(),
                slides: vec![],
            },
            vec![],
            HostConfig {
                public_audience: true,
                ..HostConfig::default()
            },
        )
        .unwrap(),
    ))
}
#[tokio::test]
async fn loopback_network_auth_snapshot_poll_and_shutdown() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (shutdown, rx) = watch::channel(false);
    let task = tokio::spawn(serve_local(listener, host(), NetworkConfig::default(), rx));
    let mut client = Client::connect_local(address, 4 * 1024 * 1024, Duration::from_secs(2))
        .await
        .unwrap();
    assert!(matches!(
        client
            .authenticate(None, InteractionMode::ReceiveOnly)
            .await
            .unwrap(),
        Response::Authenticated { .. }
    ));
    let Response::Poll(Poll::Snapshot { snapshot, .. }) =
        client.request(Request::Snapshot).await.unwrap()
    else {
        panic!("snapshot expected")
    };
    assert!(matches!(
        client
            .request(Request::Poll {
                cursor: Some(snapshot.cursor)
            })
            .await
            .unwrap(),
        Response::Poll(Poll::Unchanged(_))
    ));
    shutdown.send(true).unwrap();
    task.await.unwrap().unwrap();
}
#[tokio::test]
async fn tls_network_checks_certificate_and_authenticates() {
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
    let client_config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (shutdown, rx) = watch::channel(false);
    let task = tokio::spawn(serve_tls(
        listener,
        host(),
        Arc::new(server),
        NetworkConfig::default(),
        rx,
    ));
    assert!(
        Client::connect_tls(
            address,
            "wrong.example".into(),
            Arc::new(client_config.clone()),
            4 * 1024 * 1024,
            Duration::from_secs(2)
        )
        .await
        .is_err()
    );
    let mut client = Client::connect_tls(
        address,
        "localhost".into(),
        Arc::new(client_config),
        4 * 1024 * 1024,
        Duration::from_secs(2),
    )
    .await
    .unwrap();
    assert!(matches!(
        client
            .authenticate(None, InteractionMode::ReceiveOnly)
            .await
            .unwrap(),
        Response::Authenticated { .. }
    ));
    shutdown.send(true).unwrap();
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn slow_request_peer_is_disconnected_without_blocking_other_viewers() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (shutdown, rx) = watch::channel(false);
    let config = NetworkConfig {
        request_timeout: Duration::from_millis(500),
        ..NetworkConfig::default()
    };
    let task = tokio::spawn(serve_local(listener, host(), config, rx));
    let mut slow = tokio::net::TcpStream::connect(address).await.unwrap();
    // A partial length prefix must not retain a connection indefinitely.
    slow.write_all(&[0]).await.unwrap();
    let mut healthy = Client::connect_local(address, 4 * 1024 * 1024, Duration::from_secs(2))
        .await
        .unwrap();
    assert!(matches!(
        healthy
            .request(Request::Authenticate {
                version: PROTOCOL_VERSION,
                credential: None,
                mode: InteractionMode::ReceiveOnly,
            })
            .await
            .unwrap(),
        Response::Authenticated { .. }
    ));
    let mut bytes = Vec::new();
    tokio::time::timeout(Duration::from_secs(2), slow.read_to_end(&mut bytes))
        .await
        .unwrap()
        .unwrap();
    assert!(bytes.is_empty());
    drop(healthy);
    shutdown.send(true).unwrap();
    task.await.unwrap().unwrap();
}
