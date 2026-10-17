use super::*;
use bmux_tui::{damage::Damage, geometry::Rect, presentation::PresentationState};
use bmux_web_renderer::{BrowserFrame, decode_binary, encode_binary, encode_delta};
use futures_util::SinkExt;
use tokio_tungstenite::{
    connect_async,
    tungstenite::{Message as ClientMessage, client::IntoClientRequest},
};

type Client =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;
fn frame(revision: u64) -> BrowserFrame {
    let state = PresentationState::new(Rect::new(0, 0, 8, 2));
    let prepared = state.prepare(Damage::Full, |_| {}).unwrap();
    BrowserFrame::from_prepared_in_generation("ws-test", revision, &prepared).unwrap()
}
async fn message(client: &mut Client) -> ClientMessage {
    loop {
        let value = timeout(Duration::from_secs(2), client.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        if let ClientMessage::Ping(bytes) = value {
            client.send(ClientMessage::Pong(bytes)).await.unwrap();
        } else {
            return value;
        }
    }
}
async fn binary(client: &mut Client) -> axum::body::Bytes {
    loop {
        if let ClientMessage::Binary(bytes) = message(client).await {
            return bytes;
        }
    }
}
async fn ack(client: &mut Client, revision: u64) {
    client
        .send(ClientMessage::Text(
            json!({"type":"ack", "generation":"ws-test", "revision":revision.to_string()})
                .to_string()
                .into(),
        ))
        .await
        .unwrap();
}
struct Fixture {
    app: App,
    url: String,
    task: tokio::task::JoinHandle<()>,
    directory: PathBuf,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.task.abort();
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}
impl Fixture {
    async fn new() -> Self {
        Self::with_egress(16 * 1024 * 1024, 64 * 1024 * 1024).await
    }
    async fn with_egress(viewer_egress_sec: u64, aggregate_egress_sec: u64) -> Self {
        let directory = env::temp_dir().join(format!("bmux-relay-ws-test-{}", Uuid::new_v4()));
        let mut relay = Relay::open(directory.join("authority.json"), now()).unwrap();
        relay.start(Uuid::new_v4(), now()).unwrap();
        let (updates, _) = watch::channel(relay.view());
        let app = App {
            relay: Arc::new(Mutex::new(relay)),
            updates,
            publisher_key: Sha256::digest(b"publisher-test-key").into(),
            metrics_key: Sha256::digest(b"metrics-test-key").into(),
            origins: Arc::new(HashSet::from(["https://bmux.dev".to_owned()])),
            viewers: Arc::new(Semaphore::new(2)),
            publisher: Arc::new(Semaphore::new(1)),
            requests: Arc::new(Semaphore::new(16)),
            frames: Arc::new(AtomicU64::new(0)),
            repairs: Arc::new(AtomicU64::new(0)),
            limits: Limits {
                viewers: 2,
                message_bytes: MAX_BYTES,
                frames_sec: 60,
                bytes_sec: 32 * 1024 * 1024,
                viewer_egress_sec,
                aggregate_egress_sec,
            },
            ingress: Arc::new(Mutex::new(Rate::new())),
            http_rate: Arc::new(Mutex::new(Rate::new())),
            uploaded_bytes: Arc::new(AtomicU64::new(0)),
            delivered_bytes: Arc::new(AtomicU64::new(0)),
            egress: Arc::new(Mutex::new(Rate::new())),
            viewer_throttled: Arc::new(AtomicU64::new(0)),
            aggregate_throttled: Arc::new(AtomicU64::new(0)),
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}", listener.local_addr().unwrap());
        let server = app.clone();
        let task = tokio::spawn(async move {
            axum::serve(listener, router(server)).await.unwrap();
        });
        Self {
            app,
            url,
            task,
            directory,
        }
    }
    async fn viewer(&self) -> Client {
        let mut request = format!("{}/v1/viewer/ws", self.url)
            .into_client_request()
            .unwrap();
        request
            .headers_mut()
            .insert("origin", "https://bmux.dev".parse().unwrap());
        connect_async(request).await.unwrap().0
    }
    async fn publisher(&self) -> Client {
        let authority = self.app.relay.lock().await.authority.clone();
        let mut request = format!("{}/v1/publisher/ws", self.url)
            .into_client_request()
            .unwrap();
        request.headers_mut().insert(
            "authorization",
            "Bearer publisher-test-key".parse().unwrap(),
        );
        request.headers_mut().insert(
            "x-bmux-epoch",
            authority.epoch.unwrap().to_string().parse().unwrap(),
        );
        request
            .headers_mut()
            .insert("x-bmux-fence", authority.fence.to_string().parse().unwrap());
        connect_async(request).await.unwrap().0
    }
}
#[tokio::test]
async fn real_ws_full_delta_ack_and_slow_viewer_snapshot_repair() {
    let fixture = Fixture::new().await;
    let mut viewer = fixture.viewer().await;
    assert!(matches!(message(&mut viewer).await, ClientMessage::Text(_)));
    let mut publisher = fixture.publisher().await;
    let first = frame(1);
    let second = frame(2);
    let third = frame(3);
    publisher
        .send(ClientMessage::Binary(encode_binary(&first).unwrap().into()))
        .await
        .unwrap();
    assert!(
        message(&mut publisher)
            .await
            .into_text()
            .unwrap()
            .contains("ack")
    );
    let bytes = binary(&mut viewer).await;
    assert_eq!(decode_binary(&bytes, None).unwrap().revision(), 1);
    // Hold viewer ACK while publisher advances twice. No per-viewer delta queue.
    publisher
        .send(ClientMessage::Binary(
            encode_delta(&second, &first).unwrap().into(),
        ))
        .await
        .unwrap();
    assert!(
        message(&mut publisher)
            .await
            .into_text()
            .unwrap()
            .contains("ack")
    );
    publisher
        .send(ClientMessage::Binary(
            encode_delta(&third, &second).unwrap().into(),
        ))
        .await
        .unwrap();
    assert!(
        message(&mut publisher)
            .await
            .into_text()
            .unwrap()
            .contains("ack")
    );
    ack(&mut viewer, 1).await;
    let bytes = binary(&mut viewer).await;
    assert_eq!(
        decode_binary(&bytes, None).unwrap().revision(),
        3,
        "gapped viewer must receive a full"
    );
    ack(&mut viewer, 3).await;
    let fourth = frame(4);
    publisher
        .send(ClientMessage::Binary(
            encode_delta(&fourth, &third).unwrap().into(),
        ))
        .await
        .unwrap();
    assert!(
        message(&mut publisher)
            .await
            .into_text()
            .unwrap()
            .contains("ack")
    );
    let bytes = binary(&mut viewer).await;
    assert!(decode_binary(&bytes, None).is_err());
    assert_eq!(decode_binary(&bytes, Some(&third)).unwrap().revision(), 4);
    viewer
        .send(ClientMessage::Text("{\"type\":\"repair\"}".into()))
        .await
        .unwrap();
    assert_eq!(
        decode_binary(&binary(&mut viewer).await, None)
            .unwrap()
            .revision(),
        4
    );
}
#[tokio::test]
async fn origins_authentication_and_viewer_limit_are_enforced_over_http_upgrade() {
    let fixture = Fixture::new().await;
    assert!(
        connect_async(format!("{}/v1/viewer/ws", fixture.url))
            .await
            .is_err()
    );
    let mut bad = format!("{}/v1/viewer/ws", fixture.url)
        .into_client_request()
        .unwrap();
    bad.headers_mut()
        .insert("origin", "https://evil.example".parse().unwrap());
    assert!(connect_async(bad).await.is_err());
    assert!(
        connect_async(format!("{}/v1/publisher/ws", fixture.url))
            .await
            .is_err()
    );
    let _first = fixture.viewer().await;
    let _second = fixture.viewer().await;
    let mut third = format!("{}/v1/viewer/ws", fixture.url)
        .into_client_request()
        .unwrap();
    third
        .headers_mut()
        .insert("origin", "https://bmux.dev".parse().unwrap());
    assert!(connect_async(third).await.is_err());
}
#[tokio::test]
async fn pause_fences_an_already_connected_publisher() {
    let fixture = Fixture::new().await;
    let mut publisher = fixture.publisher().await;
    {
        let mut relay = fixture.app.relay.lock().await;
        relay.control("pause", now()).unwrap();
        fixture.app.updates.send_replace(relay.view());
    }
    publisher
        .send(ClientMessage::Binary(
            encode_binary(&frame(1)).unwrap().into(),
        ))
        .await
        .unwrap();
    let response = timeout(Duration::from_secs(2), publisher.next())
        .await
        .unwrap();
    assert!(!matches!(response, Some(Ok(ClientMessage::Text(ref text))) if text.contains("ack")));
    assert!(fixture.app.relay.lock().await.delivery.is_none());
}
#[test]
fn exact_key_authorization_and_rate_limits() {
    let expected: [u8; 32] = Sha256::digest(b"test-key").into();
    let mut headers = HeaderMap::new();
    assert!(authorize(&headers, &expected).is_err());
    headers.insert("authorization", "Bearer wrong".parse().unwrap());
    assert!(authorize(&headers, &expected).is_err());
    headers.insert("authorization", "Bearer test-key".parse().unwrap());
    assert!(authorize(&headers, &expected).is_ok());
    let mut rate = Rate::new();
    assert!(rate.admit(10, 2, 20));
    assert!(rate.admit(10, 2, 20));
    assert!(!rate.admit(1, 2, 20));
}

#[tokio::test]
async fn stop_and_expiry_revoke_socket_and_notify_viewer() {
    for expire in [false, true] {
        let fixture = Fixture::new().await;
        let mut viewer = fixture.viewer().await;
        let _ = message(&mut viewer).await;
        let mut publisher = fixture.publisher().await;
        publisher
            .send(ClientMessage::Binary(
                encode_binary(&frame(1)).unwrap().into(),
            ))
            .await
            .unwrap();
        let _ = message(&mut publisher).await;
        let _ = binary(&mut viewer).await;
        ack(&mut viewer, 1).await;
        {
            let mut relay = fixture.app.relay.lock().await;
            if expire {
                let expiry = relay.authority.expires_ms;
                assert!(relay.expire(expiry).unwrap());
            } else {
                relay.control("stop", now()).unwrap();
            }
            fixture.app.updates.send_replace(relay.view());
        }
        let state = message(&mut viewer).await.into_text().unwrap();
        let state: Value = serde_json::from_str(&state).unwrap();
        assert_eq!(state["state"], "offline");
        assert_eq!(state["lease_remaining_ms"], 0);
        let disconnected = timeout(Duration::from_secs(2), async {
            loop {
                match publisher.next().await {
                    Some(Ok(ClientMessage::Ping(bytes))) => {
                        let _ = publisher.send(ClientMessage::Pong(bytes)).await;
                    }
                    Some(Ok(ClientMessage::Close(_))) | Some(Err(_)) | None => break,
                    other => panic!("revoked publisher received unexpected message: {other:?}"),
                }
            }
        })
        .await;
        assert!(disconnected.is_ok(), "revoked publisher must disconnect");
        assert!(fixture.app.relay.lock().await.delivery.is_none());
    }
}

#[tokio::test]
async fn publisher_reconnect_requires_new_full_even_with_retained_baseline() {
    let fixture = Fixture::new().await;
    let first = frame(1);
    let second = frame(2);
    let mut publisher = fixture.publisher().await;
    publisher
        .send(ClientMessage::Binary(encode_binary(&first).unwrap().into()))
        .await
        .unwrap();
    let _ = message(&mut publisher).await;
    publisher.close(None).await.unwrap();
    timeout(Duration::from_secs(2), async {
        while fixture.app.publisher.available_permits() == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let mut publisher = fixture.publisher().await;
    publisher
        .send(ClientMessage::Binary(
            encode_delta(&second, &first).unwrap().into(),
        ))
        .await
        .unwrap();
    let repair: Value =
        serde_json::from_str(&message(&mut publisher).await.into_text().unwrap()).unwrap();
    assert_eq!(repair["type"], "repair");
    publisher
        .send(ClientMessage::Binary(
            encode_binary(&second).unwrap().into(),
        ))
        .await
        .unwrap();
    let ack: Value =
        serde_json::from_str(&message(&mut publisher).await.into_text().unwrap()).unwrap();
    assert_eq!(ack["revision"], "2");
}

#[tokio::test]
async fn one_cell_bmwp4_patch_is_forwarded_unchanged_and_tiny() {
    let fixture = Fixture::new().await;
    let state = PresentationState::new(Rect::new(0, 0, 160, 40));
    let prepared = state.prepare(Damage::Full, |_| {}).unwrap();
    let first = BrowserFrame::from_prepared_in_generation("ws-test", 1, &prepared).unwrap();
    let mut value = serde_json::to_value(&first).unwrap();
    value["revision"] = 2.into();
    value["buffer"]["cells"][100]["symbol"] = "x".into();
    let second: BrowserFrame = serde_json::from_value(value).unwrap();
    let patch = encode_delta(&second, &first).unwrap();
    let full = encode_binary(&second).unwrap();
    assert!(
        patch.len() < full.len() / 4,
        "one-cell patch must not send a full row/frame"
    );
    let mut viewer = fixture.viewer().await;
    let _ = message(&mut viewer).await;
    let mut publisher = fixture.publisher().await;
    publisher
        .send(ClientMessage::Binary(encode_binary(&first).unwrap().into()))
        .await
        .unwrap();
    let _ = message(&mut publisher).await;
    let _ = binary(&mut viewer).await;
    ack(&mut viewer, 1).await;
    publisher
        .send(ClientMessage::Binary(patch.clone().into()))
        .await
        .unwrap();
    let _ = message(&mut publisher).await;
    let received = binary(&mut viewer).await;
    assert_eq!(received.as_ref(), patch.as_slice());
    assert_eq!(decode_binary(&received, Some(&first)).unwrap(), second);
}

async fn expect_throttled_disconnect(client: &mut Client) {
    timeout(Duration::from_secs(2), async {
        loop {
            match client.next().await {
                Some(Ok(ClientMessage::Ping(bytes))) => {
                    let _ = client.send(ClientMessage::Pong(bytes)).await;
                }
                Some(Ok(ClientMessage::Text(_))) => {} // Initial authority state is permitted.
                Some(Ok(ClientMessage::Close(_))) | Some(Err(_)) | None => break,
                other => panic!("throttled viewer received payload: {other:?}"),
            }
        }
    })
    .await
    .expect("egress exhaustion must disconnect, not queue");
}

#[tokio::test]
async fn tiny_viewer_budget_bounds_repair_snapshots() {
    let full = encode_binary(&frame(1)).unwrap();
    let fixture = Fixture::with_egress(full.len() as u64, 1024 * 1024).await;
    let mut publisher = fixture.publisher().await;
    publisher
        .send(ClientMessage::Binary(full.into()))
        .await
        .unwrap();
    let _ = message(&mut publisher).await;
    let mut viewer = fixture.viewer().await;
    let _ = binary(&mut viewer).await;
    viewer
        .send(ClientMessage::Text("{\"type\":\"repair\"}".into()))
        .await
        .unwrap();
    expect_throttled_disconnect(&mut viewer).await;
    assert_eq!(fixture.app.viewer_throttled.load(Ordering::Relaxed), 1);
    assert_eq!(fixture.app.aggregate_throttled.load(Ordering::Relaxed), 0);
}

#[tokio::test]
async fn aggregate_budget_survives_snapshot_reconnect() {
    let full = encode_binary(&frame(1)).unwrap();
    let fixture = Fixture::with_egress(1024 * 1024, full.len() as u64).await;
    let mut publisher = fixture.publisher().await;
    publisher
        .send(ClientMessage::Binary(full.into()))
        .await
        .unwrap();
    let _ = message(&mut publisher).await;
    let mut first = fixture.viewer().await;
    let _ = binary(&mut first).await;
    first.close(None).await.unwrap();
    let mut reconnected = fixture.viewer().await;
    expect_throttled_disconnect(&mut reconnected).await;
    assert_eq!(fixture.app.aggregate_throttled.load(Ordering::Relaxed), 1);
    assert_eq!(fixture.app.viewer_throttled.load(Ordering::Relaxed), 0);
}

#[tokio::test]
async fn egress_admission_exact_limit_patch_and_window_reset() {
    let fixture = Fixture::with_egress(10, 15).await;
    let mut first = Rate::new();
    assert!(admit_egress(&fixture.app, &mut first, 8).await); // Snapshot.
    assert!(admit_egress(&fixture.app, &mut first, 2).await); // Patch.
    assert!(!admit_egress(&fixture.app, &mut first, 1).await);
    let mut other = Rate::new();
    assert!(admit_egress(&fixture.app, &mut other, 5).await);
    assert!(!admit_egress(&fixture.app, &mut other, 1).await);
    first.started = Instant::now() - Duration::from_secs(2);
    fixture.app.egress.lock().await.started = Instant::now() - Duration::from_secs(2);
    assert!(admit_egress(&fixture.app, &mut first, 10).await);
    assert_eq!(fixture.app.viewer_throttled.load(Ordering::Relaxed), 1);
    assert_eq!(fixture.app.aggregate_throttled.load(Ordering::Relaxed), 1);
}
