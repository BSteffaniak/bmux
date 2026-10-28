use axum::{
    Json, Router,
    extract::{
        DefaultBodyLimit, Path, State, WebSocketUpgrade,
        ws::{Message, WebSocket},
    },
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use bmux_live_relay::{Authority, LEASE_MS, MAX_BYTES, Phase, Relay, View};
use futures_util::StreamExt;
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::HashSet,
    env,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{
    sync::{Mutex, OwnedSemaphorePermit, Semaphore, watch},
    time::{Instant, interval, timeout},
};
use uuid::Uuid;

use observability::{Disconnect, Observability, Role};
mod logging;
mod observability;

type Result<T> = std::result::Result<T, StatusCode>;
const IO_TIMEOUT: Duration = Duration::from_secs(10);
#[derive(Clone)]
struct App {
    telemetry: Arc<Observability>,
    relay: Arc<Mutex<Relay>>,
    updates: watch::Sender<View>,
    publisher_key: [u8; 32],
    metrics_key: [u8; 32],
    origins: Arc<HashSet<String>>,
    viewers: Arc<Semaphore>,
    publisher: Arc<Semaphore>,
    requests: Arc<Semaphore>,
    frames: Arc<AtomicU64>,
    repairs: Arc<AtomicU64>,
    limits: Limits,
    ingress: Arc<Mutex<Rate>>,
    http_rate: Arc<Mutex<Rate>>,
    uploaded_bytes: Arc<AtomicU64>,
    delivered_bytes: Arc<AtomicU64>,
    egress: Arc<Mutex<Rate>>,
    viewer_throttled: Arc<AtomicU64>,
    aggregate_throttled: Arc<AtomicU64>,
}
#[derive(Clone, Copy)]
struct Limits {
    viewers: usize,
    message_bytes: usize,
    frames_sec: u64,
    bytes_sec: u64,
    viewer_egress_sec: u64,
    aggregate_egress_sec: u64,
}
fn limit(
    name: &str,
    default: u64,
    max: u64,
) -> std::result::Result<u64, Box<dyn std::error::Error>> {
    let value = match env::var(name) {
        Ok(value) => value.parse()?,
        Err(env::VarError::NotPresent) => default,
        Err(error) => return Err(error.into()),
    };
    if value == 0 || value > max {
        return Err(format!("invalid {name}").into());
    }
    Ok(value)
}
struct Rate {
    started: Instant,
    messages: u64,
    bytes: u64,
}
impl Rate {
    fn new() -> Self {
        Self {
            started: Instant::now(),
            messages: 0,
            bytes: 0,
        }
    }
    fn admit(&mut self, bytes: usize, messages_max: u64, bytes_max: u64) -> bool {
        if self.started.elapsed() >= Duration::from_secs(1) {
            *self = Self::new();
        }
        self.messages = self.messages.saturating_add(1);
        self.bytes = self.bytes.saturating_add(bytes as u64);
        self.messages <= messages_max && self.bytes <= bytes_max
    }
}
fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}
fn hash_config(name: &str) -> std::result::Result<[u8; 32], Box<dyn std::error::Error>> {
    let value = env::var(name)?;
    if value.len() != 64
        || !value
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
    {
        return Err(format!("invalid {name}").into());
    }
    let mut result = [0; 32];
    for (i, byte) in result.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&value[i * 2..i * 2 + 2], 16)?;
    }
    Ok(result)
}
fn authorize(headers: &HeaderMap, expected: &[u8; 32]) -> Result<()> {
    let key = headers
        .get("authorization")
        .and_then(|h| h.to_str().ok())
        .and_then(|h| h.strip_prefix("Bearer "))
        .filter(|s| !s.is_empty() && s.len() <= 1024)
        .ok_or(StatusCode::UNAUTHORIZED)?;
    let actual = Sha256::digest(key.as_bytes());
    if actual
        .iter()
        .zip(expected)
        .fold(0_u8, |v, (a, b)| v | (a ^ b))
        != 0
    {
        return Err(StatusCode::UNAUTHORIZED);
    }
    Ok(())
}
fn publisher_auth(app: &App, headers: &HeaderMap) -> Result<()> {
    authorize(headers, &app.publisher_key)
        .inspect_err(|_| app.telemetry.publisher_auth_denied())?;
    if headers.contains_key("origin") {
        return Err(StatusCode::FORBIDDEN);
    }
    Ok(())
}
fn metrics_auth(app: &App, headers: &HeaderMap) -> Result<()> {
    authorize(headers, &app.metrics_key).inspect_err(|_| app.telemetry.metrics_auth_denied())
}
fn owner(headers: &HeaderMap) -> Result<(Uuid, u64)> {
    let epoch = headers
        .get("x-bmux-epoch")
        .and_then(|h| h.to_str().ok())
        .and_then(|s| s.parse().ok())
        .ok_or(StatusCode::CONFLICT)?;
    let fence = headers
        .get("x-bmux-fence")
        .and_then(|h| h.to_str().ok())
        .and_then(|s| s.parse().ok())
        .ok_or(StatusCode::CONFLICT)?;
    Ok((epoch, fence))
}
fn status(a: &Authority) -> Value {
    json!({"type":"state", "epoch":a.epoch, "fence":a.fence.to_string(), "state":a.state, "lease_ms":LEASE_MS, "lease_remaining_ms":a.expires_ms.saturating_sub(now()).min(LEASE_MS)})
}
fn permit(semaphore: &Arc<Semaphore>) -> Result<OwnedSemaphorePermit> {
    semaphore
        .clone()
        .try_acquire_owned()
        .map_err(|_| StatusCode::TOO_MANY_REQUESTS)
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Start {
    publisher_id: Uuid,
}
async fn start(
    State(app): State<App>,
    headers: HeaderMap,
    Json(body): Json<Start>,
) -> Result<Response> {
    let _request = permit(&app.requests)?;
    publisher_auth(&app, &headers)?;
    let mut relay = app.relay.lock().await;
    if !relay
        .start(body.publisher_id, now())
        .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?
    {
        return Err(StatusCode::CONFLICT);
    }
    app.updates.send_replace(relay.view());
    Ok((
        [("x-bmux-publisher-observability", "1")],
        Json(status(&relay.authority)),
    )
        .into_response())
}
async fn control(
    State(app): State<App>,
    Path(action): Path<String>,
    headers: HeaderMap,
) -> Result<Json<Value>> {
    let _request = permit(&app.requests)?;
    publisher_auth(&app, &headers)?;
    let (epoch, fence) = owner(&headers)?;
    let mut relay = app.relay.lock().await;
    if !relay.authority.owns(epoch, fence, now()) {
        return Err(StatusCode::CONFLICT);
    }
    if !relay
        .control(&action, now())
        .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?
    {
        return Err(StatusCode::BAD_REQUEST);
    }
    app.updates.send_replace(relay.view());
    Ok(Json(status(&relay.authority)))
}
async fn publisher_status(State(app): State<App>, headers: HeaderMap) -> Result<Json<Value>> {
    publisher_auth(&app, &headers)?;
    let epoch: Uuid = headers
        .get("x-bmux-epoch")
        .and_then(|h| h.to_str().ok())
        .and_then(|s| s.parse().ok())
        .ok_or(StatusCode::CONFLICT)?;
    let relay = app.relay.lock().await;
    if !relay.authority.active(now()) || relay.authority.epoch != Some(epoch) {
        return Err(StatusCode::CONFLICT);
    }
    Ok(Json(status(&relay.authority)))
}
async fn metrics(State(app): State<App>, headers: HeaderMap) -> Result<Response> {
    metrics_auth(&app, &headers)?;
    let body = format!(
        "# TYPE bmux_relay_frames_total counter\nbmux_relay_frames_total {}\n# TYPE bmux_relay_repairs_total counter\nbmux_relay_repairs_total {}\n# TYPE bmux_relay_viewers gauge\nbmux_relay_viewers {}\n",
        app.frames.load(Ordering::Relaxed),
        app.repairs.load(Ordering::Relaxed),
        app.limits.viewers - app.viewers.available_permits()
    );
    let body = format!(
        "# TYPE bmux_relay_upload_bytes_total counter\nbmux_relay_upload_bytes_total {}\n# TYPE bmux_relay_delivery_bytes_total counter\nbmux_relay_delivery_bytes_total {}\n",
        app.uploaded_bytes.load(Ordering::Relaxed),
        app.delivered_bytes.load(Ordering::Relaxed)
    ) + &body;
    let body = format!(
        "# TYPE bmux_relay_viewer_egress_throttled_total counter\nbmux_relay_viewer_egress_throttled_total {}\n# TYPE bmux_relay_aggregate_egress_throttled_total counter\nbmux_relay_aggregate_egress_throttled_total {}\n",
        app.viewer_throttled.load(Ordering::Relaxed),
        app.aggregate_throttled.load(Ordering::Relaxed)
    ) + &body;
    let body = app.telemetry.metrics() + &body;
    Ok(([("content-type", "text/plain; version=0.0.4")], body).into_response())
}
/// Explicitly selected v1 read-only diagnostics; never shares publisher authority.
async fn diagnostics(State(app): State<App>, headers: HeaderMap) -> Result<Response> {
    metrics_auth(&app, &headers)?;
    if headers
        .get("x-bmux-diagnostics-version")
        .and_then(|v| v.to_str().ok())
        != Some("1")
    {
        return Err(StatusCode::NOT_ACCEPTABLE);
    }
    let body = json!({"version":1, "build":{"package_version":env!("CARGO_PKG_VERSION"),
        "revision":option_env!("BMUX_BUILD_REVISION"),
        "binary_wire_version":bmux_web_renderer::BINARY_WIRE_VERSION},
        "progress":app.telemetry.diagnostics()});
    Ok((
        [
            ("cache-control", "no-store"),
            ("vary", "authorization, x-bmux-diagnostics-version"),
        ],
        Json(body),
    )
        .into_response())
}
async fn publisher_ws(
    State(app): State<App>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Result<Response> {
    publisher_auth(&app, &headers)?;
    let (epoch, fence) = owner(&headers)?;
    let permit = permit(&app.publisher)?;
    {
        let relay = app.relay.lock().await;
        if !relay.authority.owns(epoch, fence, now()) || relay.authority.state == Phase::Paused {
            return Err(StatusCode::CONFLICT);
        }
    }
    Ok(ws
        .protocols(["bmux-observe-v1"])
        .max_message_size(app.limits.message_bytes)
        .max_frame_size(app.limits.message_bytes)
        .on_upgrade(move |socket| publisher_loop(app, socket, epoch, fence, permit)))
}
async fn bounded_send<E>(
    budget: Duration,
    operation: impl std::future::Future<Output = std::result::Result<(), E>>,
) -> std::result::Result<(), Disconnect> {
    match timeout(budget, operation).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(_)) => Err(Disconnect::SendFailed),
        Err(_) => Err(Disconnect::SendTimeout),
    }
}
async fn send(socket: &mut WebSocket, message: Message) -> std::result::Result<(), Disconnect> {
    bounded_send(IO_TIMEOUT, socket.send(message)).await
}
async fn send_observed(
    socket: &mut WebSocket,
    message: Message,
    connection: &observability::Connection,
) -> std::result::Result<(), Disconnect> {
    connection.sending(true);
    let result = send(socket, message).await;
    connection.sending(false);
    result
}
fn text(value: Value) -> Message {
    Message::Text(value.to_string().into())
}
async fn publisher_loop(
    app: App,
    mut socket: WebSocket,
    epoch: Uuid,
    fence: u64,
    _permit: OwnedSemaphorePermit,
) {
    let observe = socket.protocol().is_some_and(|p| p == "bmux-observe-v1");
    let mut connection = app.telemetry.connection(Role::Publisher);
    let mut fresh = true;
    let mut updates = app.updates.subscribe();
    let mut ping = interval(Duration::from_secs(5));
    let mut last_pong = Instant::now();
    loop {
        tokio::select! {
            _ = ping.tick() => {
                let valid = app.relay.lock().await.authority.owns(epoch, fence, now());
                if !valid { connection.exit(Disconnect::AuthorityChanged); break; }
                if last_pong.elapsed() > Duration::from_secs(15) { connection.exit(Disconnect::PongTimeout); break; }
                if let Err(reason) = send_observed(&mut socket, Message::Ping(Default::default()), &connection).await { connection.exit(reason); break; }
            }
            changed = updates.changed() => {
                if changed.is_err() { connection.exit(Disconnect::UpdatesClosed); break; }
                let a = updates.borrow().authority.clone();
                if !a.owns(epoch, fence, now()) || a.state == Phase::Paused { connection.exit(Disconnect::AuthorityChanged); break; }
            }
            message = socket.next() => {
                match message {
                    Some(Ok(Message::Binary(bytes))) => {
                        if !app.ingress.lock().await.admit(bytes.len(), app.limits.frames_sec, app.limits.bytes_sec) { connection.exit(Disconnect::IngressThrottled); break; }
                        app.uploaded_bytes.fetch_add(bytes.len() as u64, Ordering::Relaxed);
                        let response = {
                            let mut relay = app.relay.lock().await;
                            if !relay.authority.owns(epoch, fence, now()) || relay.authority.state == Phase::Paused { connection.exit(Disconnect::AuthorityChanged); break; }
                            match relay.ingest_detailed(bytes, fresh) {
                                Ok(()) => {
                                    fresh = false; app.frames.fetch_add(1, Ordering::Relaxed);
                                    let frame = &relay.delivery.as_ref().expect("admitted frame").frame;
                                    app.telemetry.admitted(frame.revision());
                                    connection.publisher_received(Some(frame.revision()), false);
                                    let ack = json!({"type":"ack", "generation":frame.generation(), "revision":frame.revision().to_string()});
                                    app.updates.send_replace(relay.view()); ack
                                }
                                Err(reason) => { connection.publisher_received(None, true); connection.rejected(reason); fresh = true; app.repairs.fetch_add(1, Ordering::Relaxed); if observe { json!({"type":"repair", "reason":reason, "observability_version":1}) } else { json!({"type":"repair"}) } }
                            }
                        };
                        if let Err(reason) = send_observed(&mut socket, text(response), &connection).await { connection.exit(reason); break; }
                        connection.publisher_reply_sent();
                    }
                    Some(Ok(Message::Pong(_))) => last_pong = Instant::now(),
                    Some(Ok(Message::Ping(bytes))) => { if let Err(reason) = send_observed(&mut socket, Message::Pong(bytes), &connection).await { connection.exit(reason); break; } }
                    Some(Ok(Message::Close(_))) | None => { connection.exit(Disconnect::PeerClosed); break; },
                    Some(Err(_)) => { connection.exit(Disconnect::TransportError); break; }
                    _ => { connection.exit(Disconnect::UnexpectedMessage); break; },
                }
            }
        }
    }
    drop(connection);
    let _ = send(&mut socket, Message::Close(None)).await;
}
async fn viewer_ws(
    State(app): State<App>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Result<Response> {
    let origin = headers
        .get("origin")
        .and_then(|h| h.to_str().ok())
        .ok_or(StatusCode::FORBIDDEN)?;
    if !app.origins.contains(origin) {
        return Err(StatusCode::FORBIDDEN);
    }
    let permit = permit(&app.viewers)?;
    Ok(ws
        .max_message_size(1024)
        .max_frame_size(1024)
        .on_upgrade(move |socket| viewer_loop(app, socket, permit)))
}
#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "lowercase", deny_unknown_fields)]
enum ViewerControl {
    Ack {
        generation: String,
        revision: String,
    },
    Repair,
}
#[cfg(test)]
async fn admit_egress(app: &App, viewer: &mut Rate, bytes: usize) -> bool {
    admit_egress_detailed(app, viewer, bytes).await.is_ok()
}
async fn admit_egress_detailed(
    app: &App,
    viewer: &mut Rate,
    bytes: usize,
) -> std::result::Result<(), Disconnect> {
    if !viewer.admit(bytes, u64::MAX, app.limits.viewer_egress_sec) {
        app.viewer_throttled.fetch_add(1, Ordering::Relaxed);
        return Err(Disconnect::ViewerThrottled);
    }
    // Process-wide admission is serialized and survives connection churn. Charge
    // before writes; failed writes are conservatively not refunded.
    if !app
        .egress
        .lock()
        .await
        .admit(bytes, u64::MAX, app.limits.aggregate_egress_sec)
    {
        app.aggregate_throttled.fetch_add(1, Ordering::Relaxed);
        return Err(Disconnect::AggregateThrottled);
    }
    Ok(())
}
async fn viewer_loop(app: App, mut socket: WebSocket, _permit: OwnedSemaphorePermit) {
    let mut connection = app.telemetry.connection(Role::Viewer);
    let mut updates = app.updates.subscribe();
    let mut cursor: Option<(String, u64)> = None;
    let mut pending: Option<(String, u64)> = None;
    let mut deadline = Instant::now() + IO_TIMEOUT;
    let mut last_authority = None;
    let mut egress = Rate::new();
    let mut rate = Rate::new();
    let mut ping = interval(Duration::from_secs(5));
    let mut last_pong = Instant::now();
    loop {
        let view = updates.borrow_and_update().clone();
        let authority_identity = (
            view.authority.fence,
            view.authority.state.clone(),
            view.authority.expires_ms,
        );
        if last_authority.as_ref() != Some(&authority_identity) {
            if let Err(reason) =
                send_observed(&mut socket, text(status(&view.authority)), &connection).await
            {
                connection.exit(reason);
                break;
            }
            if last_authority
                .as_ref()
                .is_none_or(|(fence, _, _)| *fence != view.authority.fence)
            {
                cursor = None;
                pending = None;
                connection.reset();
            }
            last_authority = Some(authority_identity);
        }
        if pending.is_none()
            && view.authority.active(now())
            && view.authority.state == Phase::Live
            && let Some(delivery) = &view.delivery
        {
            let next = (
                delivery.frame.generation().to_owned(),
                delivery.frame.revision(),
            );
            if cursor.as_ref() != Some(&next) {
                let bytes = if delivery.base.is_some() && delivery.base == cursor {
                    delivery.patch.clone()
                } else {
                    match delivery.snapshot() {
                        Ok(bytes) => bytes,
                        Err(_) => {
                            connection.exit(Disconnect::SnapshotFailed);
                            break;
                        }
                    }
                };
                let sent_bytes = bytes.len() as u64;
                if let Err(reason) = admit_egress_detailed(&app, &mut egress, bytes.len()).await {
                    connection.exit(reason);
                    break;
                }
                if let Err(reason) =
                    send_observed(&mut socket, Message::Binary(bytes), &connection).await
                {
                    connection.exit(reason);
                    break;
                }
                app.delivered_bytes.fetch_add(sent_bytes, Ordering::Relaxed);
                connection.sent(next.1);
                pending = Some(next);
                deadline = Instant::now() + IO_TIMEOUT;
            }
        }
        tokio::select! {
            _ = tokio::time::sleep_until(deadline), if pending.is_some() => { connection.exit(Disconnect::AckTimeout); break; },
            _ = ping.tick() => {
                if last_pong.elapsed() > Duration::from_secs(15) { connection.exit(Disconnect::PongTimeout); break; }
                if let Err(reason) = send_observed(&mut socket, Message::Ping(Default::default()), &connection).await { connection.exit(reason); break; }
            }
            changed = updates.changed() => { if changed.is_err() { connection.exit(Disconnect::UpdatesClosed); break; } }
            message = socket.next() => match message {
                Some(Ok(Message::Text(value))) => {
                    if !rate.admit(value.len(), 120, 64 * 1024) { connection.exit(Disconnect::ControlThrottled); break; }
                    match serde_json::from_str::<ViewerControl>(&value) {
                    Ok(ViewerControl::Ack { generation, revision }) => {
                        let received = revision.parse::<u64>().ok().map(|r| (generation, r));
                        if pending.is_none() || received != pending { connection.exit(Disconnect::InvalidAck); break; }
                        connection.ack(pending.as_ref().expect("checked pending").1);
                        cursor = pending.take();
                    }
                    Ok(ViewerControl::Repair) => { connection.repair(); connection.reset(); cursor = None; pending = None; app.repairs.fetch_add(1, Ordering::Relaxed); }
                    Err(_) => { connection.exit(Disconnect::InvalidControl); break; },
                } },
                Some(Ok(Message::Pong(_))) => last_pong = Instant::now(),
                Some(Ok(Message::Ping(bytes))) => { if let Err(reason) = send_observed(&mut socket, Message::Pong(bytes), &connection).await { connection.exit(reason); break; } }
                Some(Ok(Message::Close(_))) | None => { connection.exit(Disconnect::PeerClosed); break; },
                Some(Err(_)) => { connection.exit(Disconnect::TransportError); break; }
                _ => { connection.exit(Disconnect::UnexpectedMessage); break; },
            }
        }
    }
    drop(connection);
    let _ = send(&mut socket, Message::Close(None)).await;
}
async fn request_budget(
    State(app): State<App>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    if request.uri().path() != "/health" && !app.http_rate.lock().await.admit(0, 32, u64::MAX) {
        app.telemetry.http_throttled();
        return StatusCode::TOO_MANY_REQUESTS.into_response();
    }
    let Ok(_permit) = permit(&app.requests) else {
        app.telemetry.http_throttled();
        return StatusCode::TOO_MANY_REQUESTS.into_response();
    };
    match timeout(IO_TIMEOUT, next.run(request)).await {
        Ok(response) => response,
        Err(_) => {
            app.telemetry.http_timeout();
            StatusCode::REQUEST_TIMEOUT.into_response()
        }
    }
}
fn router(app: App) -> Router {
    Router::new().route("/health", get(|| async { Json(json!({"status":"ok", "renderer":format!("bmwp-{}", bmux_web_renderer::BINARY_WIRE_VERSION)})) }))
        .route("/metrics", get(metrics))
        .route("/v1/diagnostics", get(diagnostics))
        .route("/v1/publisher/start", post(start))
        .route("/v1/publisher/status", get(publisher_status))
        .route("/v1/publisher/ws", get(publisher_ws))
        .route("/v1/publisher/{action}", post(control))
        .route("/v1/viewer/ws", get(viewer_ws))
        .layer(DefaultBodyLimit::max(1024))
        .layer(axum::middleware::from_fn_with_state(app.clone(), request_budget)).with_state(app)
}
#[cfg(test)]
mod tests;

mod startup;

fn main() -> std::result::Result<(), Box<dyn std::error::Error>> {
    startup::prepare()?;
    logging::initialize();
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(run())
}

async fn run() -> std::result::Result<(), Box<dyn std::error::Error>> {
    let publisher_key = hash_config("PUBLISHER_KEY_SHA256")?;
    let metrics_key = hash_config("METRICS_KEY_SHA256")?;
    if publisher_key == metrics_key {
        return Err("metrics key must differ from publisher key".into());
    }
    let origins: HashSet<String> = env::var("ALLOWED_ORIGINS")
        .unwrap_or_else(|_| "https://bmux.dev,https://www.bmux.dev".into())
        .split(',')
        .map(str::to_owned)
        .collect();
    if origins.is_empty()
        || origins.iter().any(|s| {
            let Some(host) = s.strip_prefix("https://") else {
                return true;
            };
            !(host == "bmux.dev" || host.ends_with(".bmux.dev"))
                || host
                    .bytes()
                    .any(|b| !b.is_ascii_alphanumeric() && b != b'.' && b != b'-')
        })
    {
        return Err("ALLOWED_ORIGINS must contain exact https bmux.dev origins".into());
    }
    let path = PathBuf::from(
        env::var("AUTHORITY_PATH").unwrap_or_else(|_| startup::AUTHORITY_PATH.into()),
    );
    let limits = Limits {
        viewers: usize::try_from(limit("MAX_VIEWERS", 64, 1024)?)?,
        message_bytes: usize::try_from(limit(
            "MAX_MESSAGE_BYTES",
            MAX_BYTES as u64,
            MAX_BYTES as u64,
        )?)?,
        frames_sec: limit("MAX_FRAMES_PER_SECOND", 60, 240)?,
        bytes_sec: limit("MAX_BYTES_PER_SECOND", 32 * 1024 * 1024, 128 * 1024 * 1024)?,
        viewer_egress_sec: limit(
            "MAX_VIEWER_EGRESS_BYTES_PER_SECOND",
            16 * 1024 * 1024,
            128 * 1024 * 1024,
        )?,
        aggregate_egress_sec: limit(
            "MAX_EGRESS_BYTES_PER_SECOND",
            64 * 1024 * 1024,
            1024 * 1024 * 1024,
        )?,
    };
    let relay = Relay::open(path, now())?;
    let (updates, _) = watch::channel(relay.view());
    let app = App {
        telemetry: Arc::new(Observability::default()),
        relay: Arc::new(Mutex::new(relay)),
        updates,
        publisher_key,
        metrics_key,
        origins: Arc::new(origins),
        viewers: Arc::new(Semaphore::new(limits.viewers)),
        publisher: Arc::new(Semaphore::new(1)),
        requests: Arc::new(Semaphore::new(16)),
        frames: Arc::new(AtomicU64::new(0)),
        repairs: Arc::new(AtomicU64::new(0)),
        limits,
        ingress: Arc::new(Mutex::new(Rate::new())),
        http_rate: Arc::new(Mutex::new(Rate::new())),
        uploaded_bytes: Arc::new(AtomicU64::new(0)),
        delivered_bytes: Arc::new(AtomicU64::new(0)),
        egress: Arc::new(Mutex::new(Rate::new())),
        viewer_throttled: Arc::new(AtomicU64::new(0)),
        aggregate_throttled: Arc::new(AtomicU64::new(0)),
    };
    let expiry = app.clone();
    tokio::spawn(async move {
        let mut timer = interval(Duration::from_secs(1));
        loop {
            timer.tick().await;
            let mut relay = expiry.relay.lock().await;
            match relay.expire(now()) {
                Ok(true) => {
                    expiry.updates.send_replace(relay.view());
                }
                Ok(false) => {}
                Err(_) => {
                    logging::emit(json!({"event":"authority_persistence_failed"}));
                    std::process::exit(1);
                }
            }
        }
    });
    let listener = tokio::net::TcpListener::bind("0.0.0.0:8080").await?;
    axum::serve(listener, router(app))
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}
