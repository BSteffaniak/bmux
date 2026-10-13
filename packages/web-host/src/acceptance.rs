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
    status(&exchange(address, &foreign_host).await, 403);
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
