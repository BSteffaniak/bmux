//! Real TCP acceptance tests. Each test owns its ephemeral listener and temp assets.
use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::timeout;

use crate::{Asset, ContentType, HostConfig, HostHandle, SnapshotPublisher, snapshot_feed, start};

const DEADLINE: Duration = Duration::from_secs(10);

#[path = "auth_acceptance.rs"]
mod auth_tests;
#[path = "delivery_tests.rs"]
mod delivery_tests;
#[path = "network_acceptance.rs"]
mod network_tests;

#[tokio::test]
async fn document_host_serves_defaults_at_root_without_redirect_or_auth_bypass() {
    let (directory, mut config) = fixture();
    std::fs::write(
        directory.path().join("adapter.js"),
        b"// application adapter",
    )
    .unwrap();
    config.assets.push(Asset {
        url: "/adapter.js".into(),
        filename: "adapter.js".into(),
        content_type: ContentType::JavaScript,
    });
    config.browser_delivery = crate::BrowserDelivery::Document {
        adapter: "/adapter.js".into(),
    };
    let (_publisher, feed) = snapshot_feed(config.limits.max_snapshot_bytes).unwrap();
    let host = start(config, feed).await.unwrap();
    let address = host.address();
    let root = exchange(address, &request(address, "GET", "/", "", "")).await;
    status(&root, 200);
    assert!(!root.contains("Location:"));
    let bootstrap = exchange(address, &request(address, "GET", "/bootstrap.js", "", "")).await;
    status(&bootstrap, 200);
    assert!(bootstrap.contains("const bmuxViewerDefaults = "));
    assert!(bootstrap.contains("\"adapter\":\"/adapter.js\""));
    assert!(bootstrap.contains("Cache-Control: no-store"));
    let denied = exchange(
        address,
        &request(address, "GET", "/document?version=1", "", ""),
    )
    .await;
    status(&denied, 401);
    let authorized = exchange(
        address,
        &request(address, "GET", "/document?version=1", &token(&host), ""),
    )
    .await;
    assert!(!authorized.starts_with("HTTP/1.1 401"));
    host.shutdown().await;
}

#[tokio::test]
async fn static_asset_queries_preserve_exact_paths_and_bootstrap_authentication() {
    let (_directory, publisher, host) = host().await;
    let address = host.address();
    publisher.publish(1, Arc::from(b"{}".as_slice())).unwrap();
    for (path, expected_path) in [
        ("/?delivery=frame", "/"),
        (
            "/?delivery=document&adapter=/presenter-adapter.js&inspect=1",
            "/",
        ),
        ("/bootstrap.js?v=3", "/bootstrap.js"),
        (
            "/bootstrap.js?adapter=%2Fpresenter-adapter.js",
            "/bootstrap.js",
        ),
    ] {
        // Static bootstrap stays publicly fetchable; capabilities remain in fragments.
        let base = exchange(
            address,
            &request(address, "GET", expected_path, "wrong", ""),
        )
        .await;
        let response = exchange(address, &request(address, "GET", path, "wrong", "")).await;
        assert!(response.starts_with("HTTP/1.1 200"), "{path}: {response}");
        assert_eq!(response, base);
    }
    for path in [
        "/../bootstrap.js?v=3",
        "/%2e%2e/bootstrap.js?v=3",
        "/%62ootstrap.js?v=3",
        "//bootstrap.js?v=3",
        "/missing.js?v=3",
    ] {
        let response = exchange(address, &request(address, "GET", path, "wrong", "")).await;
        assert!(response.starts_with("HTTP/1.1 404"), "{path}: {response}");
    }
    for path in [
        "/?token=secret",
        "/bootstrap.js?Token=secret",
        "/?%74oken=secret",
    ] {
        let response = exchange(address, &request(address, "GET", path, "wrong", "")).await;
        assert!(response.starts_with("HTTP/1.1 404"), "{path}: {response}");
        assert!(!response.contains("secret"));
    }
    let response = exchange(address, &request(address, "GET", "/snapshot", "wrong", "")).await;
    assert!(response.starts_with("HTTP/1.1 401"));
    let response = exchange(
        address,
        &request(
            address,
            "GET",
            "/?delivery=frame",
            "wrong",
            "Origin: http://evil.example\r\n",
        ),
    )
    .await;
    assert!(response.starts_with("HTTP/1.1 403"));
    host.shutdown().await;
}

#[tokio::test]
async fn negotiated_document_is_atomic_and_repairs_latest_full() {
    use crate::{DocumentPublication, Resource};
    use sha2::Digest;
    use std::collections::BTreeMap;
    let (directory, mut config) = fixture();
    config.limits.max_snapshot_bytes = 4096;
    config.limits.poll_timeout = Duration::from_millis(25);
    let (publisher, feed) = snapshot_feed(config.limits.max_snapshot_bytes).unwrap();
    let host = start(config, feed).await.unwrap();
    let _directory = directory;
    let address = host.address();
    let bearer = token(&host);
    let resource = Resource {
        bytes: Arc::from(b"opaque resource".as_slice()),
        mime_type: "application/octet-stream".into(),
    };
    let id: String = sha2::Sha256::digest(&resource.bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    let publish = |transport, revision| {
        publisher.publish_document(
            transport,
            DocumentPublication {
                generation: "source-v1".into(),
                revision,
                capability: "test.document.v1".into(),
                content: Arc::from(b"opaque semantic bytes".as_slice()),
            },
            BTreeMap::from([(id.clone(), resource.clone())]),
        )
    };
    publish(1, 7).unwrap();
    assert!(publish(2, 7).is_err());
    for transport in 2..=10 {
        publish(transport, transport + 7).unwrap();
    }
    for path in ["/document?version=1", "/document?version=1&after=1"] {
        let response = exchange(address, &request(address, "GET", path, &bearer, "")).await;
        assert!(response.starts_with("HTTP/1.1 200"));
        assert!(response.contains("Content-Type: application/vnd.bmux.document-v1+json\r\n"));
        assert!(response.contains("X-Snapshot-Revision: 10\r\n"));
        let (_, body) = response.split_once("\r\n\r\n").unwrap();
        let value: serde_json::Value = serde_json::from_str(body).unwrap();
        assert_eq!(value["publication_revision"], "10");
        assert_eq!(value["revision"], "17");
        assert_eq!(value["resources"][0]["id"], id);
    }
    let response = exchange(
        address,
        &request(address, "GET", "/document?version=1&after=10", &bearer, ""),
    )
    .await;
    assert!(response.starts_with("HTTP/1.1 204"));
    for path in [
        "/document",
        "/document?version=2",
        "/document?version=1&version=1",
        "/document?version=1&after=-1",
    ] {
        let response = exchange(address, &request(address, "GET", path, &bearer, "")).await;
        assert!(response.starts_with("HTTP/1.1 400"));
    }
    let response = exchange(
        address,
        &request(address, "GET", "/document?version=1", "wrong", ""),
    )
    .await;
    assert!(response.starts_with("HTTP/1.1 401"));
    let response = exchange(
        address,
        &request(address, "GET", &format!("/resource/{id}"), &bearer, ""),
    )
    .await;
    assert!(response.ends_with("opaque resource"));
    // Legacy JSON is not substituted for semantic content, nor vice versa.
    let response = exchange(address, &request(address, "GET", "/snapshot", &bearer, "")).await;
    assert!(response.starts_with("HTTP/1.1 406"));
    publisher.publish(11, Arc::from(b"{}".as_slice())).unwrap();
    let response = exchange(
        address,
        &request(address, "GET", "/document?version=1", &bearer, ""),
    )
    .await;
    assert!(response.starts_with("HTTP/1.1 406"));
    host.shutdown().await;
}

#[tokio::test]
async fn negotiated_binary_resources_and_repair() {
    use crate::{BinaryPresentation, Resource};
    use sha2::Digest;
    use std::collections::BTreeMap;
    let (_directory, publisher, host) = host().await;
    let address = host.address();
    let bearer = token(&host);
    let publish = |revision, delta, resources| {
        publisher.publish_representations(
            revision,
            Some(Arc::from(b"{}".as_slice())),
            BinaryPresentation {
                snapshot: Arc::from(b"full".as_slice()),
                delta,
            },
            resources,
        )
    };
    let resource = Resource {
        bytes: Arc::from(b"resource".as_slice()),
        mime_type: "application/octet-stream".into(),
    };
    let id: String = sha2::Sha256::digest(&resource.bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    let resource_path = format!("/resource/{id}");
    publish(1, None, BTreeMap::from([(id.clone(), resource.clone())])).unwrap();
    publish(
        2,
        Some((1, Arc::from(b"delta".as_slice()))),
        BTreeMap::new(),
    )
    .unwrap();
    for (path, status, body) in [
        ("/presentation?version=1&after=1&base=1", 200, "delta"),
        ("/presentation?version=1&after=1&base=0", 200, "full"),
        ("/presentation?version=1&after=2&base=0", 200, "full"),
        ("/presentation?version=2", 400, ""),
        ("/presentation", 400, ""),
        ("/presentation?version=1&version=1", 400, ""),
        (resource_path.as_str(), 200, "resource"),
        ("/resource/%2e", 400, ""),
        ("/snapshot", 200, "{}"),
    ] {
        let response = exchange(address, &request(address, "GET", path, &bearer, "")).await;
        assert!(
            response.starts_with(&format!("HTTP/1.1 {status}")),
            "{path}: {response}"
        );
        assert!(response.ends_with(body));
        if path.starts_with("/presentation") && status == 200 {
            assert!(response.contains("Content-Type: application/vnd.bmux.presentation-v1\r\n"));
            assert!(response.contains("X-Snapshot-Revision: 2\r\n"));
        }
    }
    for path in ["/presentation?version=1", resource_path.as_str()] {
        let response = exchange(address, &request(address, "GET", path, "wrong", "")).await;
        assert!(response.starts_with("HTTP/1.1 401"));
        let response = exchange(
            address,
            &request(
                address,
                "GET",
                path,
                &bearer,
                "Origin: http://evil.example\r\n",
            ),
        )
        .await;
        assert!(response.starts_with("HTTP/1.1 403"));
    }
    host.set_generation("generation").unwrap();
    let view = exchange(
        address,
        &request(
            address,
            "POST",
            "/view",
            &bearer,
            &format!("Origin: http://{address}\r\n"),
        ),
    )
    .await;
    assert!(view.contains("\"presentation_versions\":[1]"));
    let changed = Resource {
        bytes: Arc::from(b"changed".as_slice()),
        ..resource.clone()
    };
    assert!(publish(3, None, BTreeMap::from([(id.clone(), changed)])).is_err());
    publish(3, None, BTreeMap::new()).unwrap();
    let response = exchange(
        address,
        &request(address, "GET", &resource_path, &bearer, ""),
    )
    .await;
    assert!(response.starts_with("HTTP/1.1 404"));
    publisher
        .publish_representations(
            4,
            None,
            BinaryPresentation {
                snapshot: Arc::from(b"full".as_slice()),
                delta: None,
            },
            BTreeMap::from([(id, resource)]),
        )
        .unwrap();
    let response = exchange(address, &request(address, "GET", "/snapshot", &bearer, "")).await;
    assert!(response.starts_with("HTTP/1.1 406"));
    let response = exchange(
        address,
        &request(address, "GET", "/snapshot?after=4", &bearer, ""),
    )
    .await;
    assert!(response.starts_with("HTTP/1.1 406"));
    let response = exchange(
        address,
        &request(address, "GET", "/presentation?version=1", &bearer, ""),
    )
    .await;
    assert!(response.ends_with("full"));
    let mut poll = connect(
        address,
        &request(
            address,
            "GET",
            "/presentation?version=1&after=4&base=4",
            &bearer,
            "",
        ),
    )
    .await;
    assert_pending(&mut poll).await;
    drop(publisher);
    let response = read_response(&mut poll).await;
    if !response.is_empty() {
        assert!(response.starts_with("HTTP/1.1 503"));
    }
    host.shutdown().await;
}

fn fixture() -> (TempDir, HostConfig) {
    let directory = tempfile::tempdir().unwrap();
    for (filename, bytes) in [
        (
            "index.html",
            b"<!doctype html><title>fixture</title>".as_slice(),
        ),
        ("bootstrap.js", b"export const fixture = true;".as_slice()),
        ("style.css", b"body { margin: 0; }".as_slice()),
    ] {
        std::fs::write(directory.path().join(filename), bytes).unwrap();
    }
    // Canonicalize the fixture root because some Unix temp roots are symlinks.
    let mut config = HostConfig::new(directory.path().canonicalize().unwrap());
    config.port = 0;
    config.auth_mode = crate::AuthMode::Token;
    // Tests exercise explicit configured assets without fabricating renderer/WASM output.
    config.assets = vec![
        Asset {
            url: "/".into(),
            filename: "index.html".into(),
            content_type: ContentType::Html,
        },
        Asset {
            url: "/style.css".into(),
            filename: "style.css".into(),
            content_type: ContentType::TextCss,
        },
        Asset {
            url: "/bootstrap.js".into(),
            filename: "bootstrap.js".into(),
            content_type: ContentType::JavaScript,
        },
    ];
    config.limits.max_snapshot_bytes = 128;
    config.limits.poll_timeout = Duration::from_secs(30);
    config.limits.header_timeout = Duration::from_secs(30);
    (directory, config)
}

async fn host() -> (TempDir, SnapshotPublisher, HostHandle) {
    let (directory, config) = fixture();
    let (publisher, feed) = snapshot_feed(config.limits.max_snapshot_bytes).unwrap();
    let host = start(config, feed).await.unwrap();
    assert!(host.address().ip().is_loopback());
    assert_ne!(host.address().port(), 0);
    (directory, publisher, host)
}

#[tokio::test]
async fn idle_preconnections_close_without_http_rejection_but_partial_requests_time_out() {
    let (_directory, mut config) = fixture();
    config.limits.header_timeout = Duration::from_millis(100);
    let (_publisher, feed) = snapshot_feed(config.limits.max_snapshot_bytes).unwrap();
    let host = start(config, feed).await.unwrap();
    let mut idle = TcpStream::connect(host.address()).await.unwrap();
    let mut bytes = Vec::new();
    timeout(DEADLINE, idle.read_to_end(&mut bytes))
        .await
        .unwrap()
        .unwrap();
    assert!(
        bytes.is_empty(),
        "idle socket received an unsolicited response"
    );
    assert_eq!(host.diagnostics().request_rejections, 0);
    assert!(
        host.diagnostics()
            .events
            .iter()
            .any(|event| event.code == "idle_connection_timeout")
    );

    let mut partial = connect(host.address(), "GET / HTTP/1.1\r\n").await;
    status(&read_response(&mut partial).await, 408);
    assert_eq!(host.diagnostics().request_rejections, 1);
    assert!(
        host.diagnostics()
            .events
            .iter()
            .any(|event| event.code == "request_rejected" && event.detail == Some(408))
    );
    host.shutdown().await;
}

#[tokio::test]
async fn diagnostics_follow_auth_policy_and_survive_producer_loss_locally() {
    let (_directory, publisher, host) = host().await;
    let address = host.address();
    let rejected = exchange(
        address,
        &request(address, "GET", "/diagnostics", "wrong", ""),
    )
    .await;
    assert!(rejected.starts_with("HTTP/1.1 401"));
    let accepted = exchange(
        address,
        &request(address, "GET", "/diagnostics", &token(&host), ""),
    )
    .await;
    assert!(accepted.starts_with("HTTP/1.1 200"));
    let body = accepted.split_once("\r\n\r\n").unwrap().1;
    let evidence: serde_json::Value = serde_json::from_str(body).unwrap();
    assert_eq!(evidence["running"], true);
    assert_eq!(evidence["request_rejections"], 1);
    assert!(!body.contains(&token(&host)));
    drop(publisher);
    tokio::time::timeout(Duration::from_secs(2), async {
        while host.diagnostics().running {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        host.diagnostics().events.last().unwrap().code,
        "producer_closed"
    );
    host.shutdown().await;
}

#[tokio::test]
async fn diagnostics_record_capacity_without_request_contents() {
    let (_directory, mut config) = fixture();
    config.limits.max_connections = 1;
    let (_publisher, feed) = snapshot_feed(config.limits.max_snapshot_bytes).unwrap();
    let host = start(config, feed).await.unwrap();
    let held = tokio::net::TcpStream::connect(host.address())
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        while host.diagnostics().accepted_connections < 1 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let excess = tokio::net::TcpStream::connect(host.address())
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        while host.diagnostics().capacity_rejections < 1 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        host.diagnostics().events.last().unwrap().code,
        "capacity_rejected"
    );
    assert!(host.diagnostics().running);
    drop(excess);
    drop(held);
    host.shutdown().await;
}

fn token(host: &HostHandle) -> String {
    host.bootstrap_url()
        .split_once("#token=")
        .unwrap()
        .1
        .to_owned()
}

fn request(address: SocketAddr, method: &str, path: &str, token: &str, extra: &str) -> String {
    format!(
        "{method} {path} HTTP/1.1\r\nHost: {address}\r\nAuthorization: Bearer {token}\r\n{extra}\r\n"
    )
}

async fn connect(address: SocketAddr, request: &str) -> TcpStream {
    let mut stream = timeout(DEADLINE, TcpStream::connect(address))
        .await
        .unwrap()
        .unwrap();
    timeout(DEADLINE, stream.write_all(request.as_bytes()))
        .await
        .unwrap()
        .unwrap();
    stream
}

async fn read_response(stream: &mut TcpStream) -> String {
    let mut bytes = Vec::new();
    // Bound even the test client's read, so regressions cannot hang or grow indefinitely.
    timeout(DEADLINE, (&mut *stream).take(65536).read_to_end(&mut bytes))
        .await
        .unwrap()
        .unwrap();
    assert!(bytes.len() < 65536);
    String::from_utf8(bytes).unwrap()
}

async fn exchange(address: SocketAddr, request: &str) -> String {
    let mut stream = connect(address, request).await;
    read_response(&mut stream).await
}

fn status(response: &str, expected: u16) {
    assert!(
        response.starts_with(&format!("HTTP/1.1 {expected} ")),
        "{response}"
    );
    assert!(response.contains("\r\nConnection: close\r\n"));
    assert!(response.contains("\r\nCache-Control: no-store\r\n"));
    assert!(
        !response
            .to_ascii_lowercase()
            .contains("access-control-allow-origin")
    );
}

async fn wait_receivers(publisher: &SnapshotPublisher, expected: usize) {
    timeout(DEADLINE, async {
        loop {
            if publisher.sender.lock().unwrap().receiver_count() == expected {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

async fn assert_pending(stream: &mut TcpStream) {
    let mut byte = [0];
    assert!(
        timeout(Duration::from_millis(30), stream.read(&mut byte))
            .await
            .is_err()
    );
}

async fn assert_disconnected(stream: &mut TcpStream) {
    let mut byte = [0];
    match timeout(DEADLINE, stream.read(&mut byte)).await.unwrap() {
        Ok(0) => {}
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::ConnectionReset | io::ErrorKind::ConnectionAborted
            ) => {}
        other => panic!("connection survived host cleanup: {other:?}"),
    }
}

#[tokio::test]
async fn tcp_view_claim_geometry_renewal_release_and_source_loss() {
    let (directory, mut config) = fixture();
    config.viewport_mode = crate::ViewportMode::Resizable;
    let (publisher, feed) = snapshot_feed(128).unwrap();
    let mut host = start(config, feed).await.unwrap();
    let mut requests = host.take_viewport_requests().unwrap();
    let address = host.address();
    let token = token(&host);
    let origin = format!("Origin: http://{address}\r\n");
    let claim = exchange(address, &request(address, "POST", "/view", &token, &origin)).await;
    status(&claim, 503);
    publisher.publish(1, Arc::from(*b"{}")).unwrap();
    host.set_generation("generation-1").unwrap();
    let claim = exchange(address, &request(address, "POST", "/view", &token, &origin)).await;
    status(&claim, 200);
    let info: serde_json::Value =
        serde_json::from_str(claim.split_once("\r\n\r\n").unwrap().1).unwrap();
    let controller = info["controller"].as_str().unwrap();
    assert_eq!(info["generation"], "generation-1");
    assert_eq!(info["viewport_allowed"], true);
    let ownership = format!("{origin}X-Bmux-Controller: {controller}\r\n");
    let extra = exchange(address, &request(address, "POST", "/view", &token, &origin)).await;
    assert!(!extra.contains(controller));
    assert!(extra.contains("\"viewport_allowed\":false"));
    let body = r#"{"generation":"generation-1","request_id":"1","cols":100,"rows":40}"#;
    let headers = format!(
        "{ownership}Content-Type: application/json\r\nContent-Length: {}\r\n",
        body.len()
    );
    let submit = format!(
        "{}{body}",
        request(address, "POST", "/viewport", &token, &headers)
    );
    status(&exchange(address, &submit).await, 204);
    let geometry = timeout(DEADLINE, requests.recv()).await.unwrap().unwrap();
    assert_eq!((geometry.cols, geometry.rows), (100, 40));
    assert!(requests.is_current(&geometry));
    status(&exchange(address, &submit).await, 409);
    status(
        &exchange(
            address,
            &request(address, "POST", "/view", &token, &ownership),
        )
        .await,
        200,
    );
    assert!(requests.is_current(&geometry));
    status(
        &exchange(
            address,
            &request(address, "DELETE", "/view", &token, &origin),
        )
        .await,
        403,
    );
    status(
        &exchange(
            address,
            &request(address, "DELETE", "/view", &token, &ownership),
        )
        .await,
        204,
    );
    assert!(!requests.is_current(&geometry));
    status(&exchange(address, &submit).await, 409);
    host.invalidate_view();
    status(
        &exchange(address, &request(address, "POST", "/view", &token, &origin)).await,
        503,
    );
    host.set_generation("generation-2").unwrap();
    host.shutdown().await;
    assert!(timeout(DEADLINE, requests.recv()).await.unwrap().is_none());
    drop(directory);
}

#[tokio::test]
async fn tcp_viewport_partial_body_disconnect_timeout_and_bounds() {
    let (_directory, mut config) = fixture();
    config.viewport_mode = crate::ViewportMode::Resizable;
    config.limits.header_timeout = Duration::from_millis(100);
    let (publisher, feed) = snapshot_feed(128).unwrap();
    let mut host = start(config, feed).await.unwrap();
    let mut requests = host.take_viewport_requests().unwrap();
    publisher.publish(1, Arc::from(*b"{}")).unwrap();
    host.set_generation("g").unwrap();
    let address = host.address();
    let token = token(&host);
    let origin = format!("Origin: http://{address}\r\n");
    let claim = exchange(address, &request(address, "POST", "/view", &token, &origin)).await;
    let info: serde_json::Value =
        serde_json::from_str(claim.split_once("\r\n\r\n").unwrap().1).unwrap();
    let controller = info["controller"].as_str().unwrap();
    let headers = format!(
        "{origin}X-Bmux-Controller: {controller}\r\nContent-Type: application/json\r\nContent-Length: 100\r\n"
    );
    let partial = request(address, "POST", "/viewport", &token, &headers);
    let mut slow = connect(address, &format!("{partial}{{")).await;
    status(&read_response(&mut slow).await, 408);
    let mut disconnected = connect(address, &format!("{partial}{{")).await;
    disconnected.shutdown().await.unwrap();
    status(&read_response(&mut disconnected).await, 400);
    assert!(
        timeout(Duration::from_millis(30), requests.recv())
            .await
            .is_err()
    );
    let oversized = format!("{origin}Content-Type: application/json\r\nContent-Length: 513\r\n");
    status(
        &exchange(
            address,
            &request(address, "POST", "/viewport", &token, &oversized),
        )
        .await,
        413,
    );
    host.shutdown().await;
}

#[tokio::test]
async fn tcp_full_snapshots_longpoll_and_latest_repair() {
    let (_directory, publisher, host) = host().await;
    let address = host.address();
    let token = token(&host);
    publisher.publish(1, Arc::from(*b"{\"value\":1}")).unwrap();
    let first = exchange(address, &request(address, "GET", "/snapshot", &token, "")).await;
    status(&first, 200);
    assert!(first.contains("\r\nX-Snapshot-Revision: 1\r\n"));
    assert!(first.ends_with("\r\n\r\n{\"value\":1}"));

    let mut poll = connect(
        address,
        &request(address, "GET", "/snapshot?after=1", &token, ""),
    )
    .await;
    assert_pending(&mut poll).await;
    // No await between replacements: the server can only observe the latest state.
    publisher.publish(2, Arc::from(*b"{\"value\":2}")).unwrap();
    publisher.publish(3, Arc::from(*b"{\"value\":3}")).unwrap();
    let replacement = read_response(&mut poll).await;
    status(&replacement, 200);
    assert!(replacement.contains("\r\nX-Snapshot-Revision: 3\r\n"));
    assert!(replacement.ends_with("\r\n\r\n{\"value\":3}"));
    let repair = exchange(
        address,
        &request(address, "GET", "/snapshot?after=99", &token, ""),
    )
    .await;
    status(&repair, 200);
    assert!(repair.ends_with("\r\n\r\n{\"value\":3}"));

    let asset = exchange(address, &request(address, "GET", "/", "", "")).await;
    status(&asset, 200);
    assert!(asset.ends_with("<!doctype html><title>fixture</title>"));
    let stylesheet = exchange(address, &request(address, "GET", "/style.css", "", "")).await;
    status(&stylesheet, 200);
    assert!(stylesheet.contains("\r\nContent-Type: text/css; charset=utf-8\r\n"));
    assert!(stylesheet.contains("style-src 'self';"));
    assert!(!stylesheet.contains("'unsafe-inline'"));
    assert!(stylesheet.ends_with("body { margin: 0; }"));
    status(
        &exchange(address, &request(address, "GET", "/unlisted.css", "", "")).await,
        404,
    );
    host.shutdown().await;
}

#[tokio::test]
async fn tcp_rejects_wrong_capability_foreign_origin_host_and_input() {
    let (_directory, publisher, host) = host().await;
    publisher.publish(1, Arc::from(*b"{}")).unwrap();
    let address = host.address();
    let token = token(&host);
    let mut wrong_token = token.clone();
    wrong_token.replace_range(..1, if token.starts_with('0') { "1" } else { "0" });
    for (method, path, capability, extra, expected) in [
        ("GET", "/snapshot", wrong_token, "", 401),
        (
            "GET",
            "/snapshot",
            token.clone(),
            "Origin: http://evil.example\r\n",
            403,
        ),
        (
            "GET",
            "/snapshot",
            token.clone(),
            "Sec-Fetch-Site: cross-site\r\n",
            403,
        ),
        ("POST", "/input", token.clone(), "", 405),
        ("GET", "/input", token.clone(), "", 404),
        ("GET", "/snapshot?token=secret", token.clone(), "", 404),
        ("GET", "/", token.clone(), "Content-Length: 1\r\n", 400),
    ] {
        let response = exchange(address, &request(address, method, path, &capability, extra)).await;
        status(&response, expected);
        assert!(!response.contains(&token));
    }
    let foreign_host = format!(
        "GET /snapshot HTTP/1.1\r\nHost: evil.example:{}\r\nAuthorization: Bearer {token}\r\n\r\n",
        address.port()
    );
    status(&exchange(address, &foreign_host).await, 200);
    host.shutdown().await;
}

#[tokio::test]
async fn tcp_closed_producer_invalidates_retained_and_waiting_snapshots() {
    let (_directory, publisher, host) = host().await;
    let address = host.address();
    let token = token(&host);
    publisher.publish(1, Arc::from(*b"{}")).unwrap();
    let mut poll = connect(
        address,
        &request(address, "GET", "/snapshot?after=1", &token, ""),
    )
    .await;
    assert_pending(&mut poll).await;
    drop(publisher);
    // Either the poll observes feed closure first, or owner cancellation closes TCP first.
    let mut bytes = Vec::new();
    let _ = timeout(DEADLINE, poll.read_to_end(&mut bytes))
        .await
        .unwrap();
    if !bytes.is_empty() {
        status(&String::from_utf8(bytes).unwrap(), 503);
    }
    timeout(DEADLINE, async {
        loop {
            if TcpStream::connect(address).await.is_err() {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    host.shutdown().await;
}

async fn cleanup_case(drop_handle: bool) {
    let (_directory, publisher, host) = host().await;
    let address = host.address();
    let token = token(&host);
    publisher.publish(1, Arc::from(*b"{}")).unwrap();
    let mut poll = connect(
        address,
        &request(address, "GET", "/snapshot?after=1", &token, ""),
    )
    .await;
    assert_pending(&mut poll).await;
    // A second child remains blocked in header parsing, not snapshot polling.
    let mut incomplete = connect(address, "GET / HTTP/1.1\r\n").await;
    wait_receivers(&publisher, 3).await;
    if drop_handle {
        drop(host);
    } else {
        timeout(DEADLINE, host.shutdown()).await.unwrap();
    }
    // Both children and the listener's feed receiver must be gone.
    wait_receivers(&publisher, 0).await;
    assert_disconnected(&mut poll).await;
    assert_disconnected(&mut incomplete).await;
    assert!(publisher.publish(2, Arc::from(*b"{}")).is_err());
    assert!(
        timeout(DEADLINE, TcpStream::connect(address))
            .await
            .unwrap()
            .is_err()
    );
}

#[tokio::test]
async fn tcp_shutdown_drains_children_and_closes_channel() {
    cleanup_case(false).await;
}

#[tokio::test]
async fn tcp_drop_cancels_children_and_closes_channel() {
    cleanup_case(true).await;
}

#[tokio::test]
async fn overbudget_feed_and_publication_are_rejected() {
    let (_directory, config) = fixture();
    let (_publisher, feed) = snapshot_feed(config.limits.max_snapshot_bytes + 1).unwrap();
    assert!(
        matches!(start(config, feed).await, Err(error) if error.kind() == io::ErrorKind::InvalidInput)
    );
    let (_directory, publisher, host) = host().await;
    publisher.publish(1, Arc::from(*b"{}")).unwrap();
    assert!(publisher.publish(2, Arc::from(vec![b'x'; 129])).is_err());
    let address = host.address();
    let response = exchange(
        address,
        &request(address, "GET", "/snapshot", &token(&host), ""),
    )
    .await;
    status(&response, 200);
    assert!(response.contains("\r\nX-Snapshot-Revision: 1\r\n"));
    assert!(response.ends_with("\r\n\r\n{}"));
    host.shutdown().await;
}
