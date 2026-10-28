//! Fenced publisher transport. Only acknowledged frames become delta bases.
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use std::{io::Read, path::Path, time::Duration};
use tokio::sync::Mutex;
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream,
    tungstenite::{Message, client::IntoClientRequest},
};
use uuid::Uuid;

pub const MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;
type Socket = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;
#[derive(Default)]
struct Stream {
    socket: Option<(Socket, bool)>,
    base: Option<bmux_web_renderer::BrowserFrame>,
}
pub struct Relay {
    observations: crate::diagnostics::Shared,
    client: reqwest::Client,
    base: reqwest::Url,
    key: String,
    publisher: Uuid,
    observe_protocol: bool,
    pub epoch: Option<Uuid>,
    fence: Mutex<Option<String>>,
    stream: Mutex<Stream>,
}
#[derive(Deserialize)]
struct Authority {
    epoch: Option<Uuid>,
    fence: String,
    state: String,
}
fn decimal(value: &str) -> bool {
    !value.is_empty()
        && (value == "0" || !value.starts_with('0'))
        && value.bytes().all(|byte| byte.is_ascii_digit())
        && value.parse::<u64>().is_ok()
}
impl Relay {
    pub fn new(endpoint: &str, channel: &str, credential: &Path) -> Result<Self, &'static str> {
        let base = validate_endpoint(endpoint, channel)?;
        let key = credential_key(credential)?;
        let _ = rustls::crypto::ring::default_provider().install_default();
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(3))
            .connect_timeout(Duration::from_secs(2))
            .build()
            .map_err(|_| "transport_unavailable")?;
        Ok(Self {
            observations: crate::diagnostics::Shared::default(),
            client,
            base,
            key,
            publisher: Uuid::new_v4(),
            observe_protocol: false,
            epoch: None,
            fence: Mutex::new(None),
            stream: Mutex::new(Stream::default()),
        })
    }
    pub(crate) fn observe(&mut self, observations: crate::diagnostics::Shared) {
        self.observations = observations;
    }
    pub async fn start(&mut self) -> Result<(), &'static str> {
        let attempt = crate::diagnostics::Attempt::begin(&self.observations, "start", None);
        let result = self.start_inner(&attempt).await;
        attempt.finish(result).map_err(legacy_failure)
    }
    async fn start_inner(
        &mut self,
        attempt: &crate::diagnostics::Attempt,
    ) -> Result<(), &'static str> {
        let response = self
            .client
            .post(self.base.join("start").map_err(|_| "invalid_endpoint")?)
            .bearer_auth(&self.key)
            .json(&serde_json::json!({"publisher_id":self.publisher}))
            .send()
            .await
            .map_err(http_failure)?;
        attempt.http_status(response.status().as_u16());
        let supports_observe = response
            .headers()
            .get("x-bmux-publisher-observability")
            .is_some_and(|value| value == "1");
        let authority = authority(response).await?;
        self.observe_protocol = supports_observe;
        self.epoch = authority.epoch;
        *self.fence.lock().await = Some(authority.fence);
        *self.stream.lock().await = Stream::default();
        Ok(())
    }
    pub async fn mutation(
        &self,
        operation: &str,
        frame: Option<(u64, &[u8])>,
    ) -> Result<(), &'static str> {
        let operation = match operation {
            "frame" => "frame",
            "heartbeat" => "heartbeat",
            "pause" => "pause",
            "resume" => "resume",
            "stop" => "stop",
            _ => return Err("invalid_operation"),
        };
        let decoded =
            frame.and_then(|(_, bytes)| bmux_web_renderer::decode_binary(bytes, None).ok());
        let revision = decoded.as_ref().map(|frame| frame.revision());
        let mut attempt =
            crate::diagnostics::Attempt::begin(&self.observations, operation, revision);
        if let Some(frame) = decoded {
            attempt.generation(frame.generation());
        }
        let result = self.mutation_inner(operation, frame, &attempt).await;
        attempt.finish(result).map_err(legacy_failure)
    }
    async fn mutation_inner(
        &self,
        operation: &str,
        frame: Option<(u64, &[u8])>,
        attempt: &crate::diagnostics::Attempt,
    ) -> Result<(), &'static str> {
        if operation == "frame" {
            let (_, bytes) = frame.ok_or("invalid_frame")?;
            return self.publish(bytes, attempt).await;
        }
        if !["heartbeat", "pause", "resume", "stop"].contains(&operation) {
            return Err("invalid_operation");
        }
        if operation == "heartbeat" {
            self.service_keepalive().await?;
        } else {
            // Local pause/source-switch admission already invalidated the view.
            // Even a failed remote transition must not retain its delta baseline.
            *self.stream.lock().await = Stream::default();
        }
        let epoch = self.epoch.ok_or("not_started")?;
        let fence = self.fence.lock().await.clone().ok_or("not_started")?;
        let response = self
            .client
            .post(self.base.join(operation).map_err(|_| "invalid_endpoint")?)
            .bearer_auth(&self.key)
            .header("X-Bmux-Epoch", epoch.to_string())
            .header("X-Bmux-Fence", fence)
            .send()
            .await
            .map_err(http_failure)?;
        attempt.http_status(response.status().as_u16());
        let next = match authority(response).await {
            Err("relay_epoch_lost" | "http_auth_rejected") if operation == "heartbeat" => {
                self.discover_fence(attempt).await?;
                return Err("relay_transient");
            }
            result => result?,
        };
        if next.epoch.is_some_and(|next| next != epoch) {
            return Err("relay_epoch_lost");
        }
        *self.fence.lock().await = Some(next.fence);
        Ok(())
    }
    /// Reconcile a durable relay restart without guessing a new authority.
    async fn discover_fence(
        &self,
        attempt: &crate::diagnostics::Attempt,
    ) -> Result<(), &'static str> {
        let response = self
            .client
            .get(self.base.join("status").map_err(|_| "invalid_endpoint")?)
            .bearer_auth(&self.key)
            .header("X-Bmux-Epoch", self.epoch.ok_or("not_started")?.to_string())
            .send()
            .await
            .map_err(http_failure)?;
        attempt.http_status(response.status().as_u16());
        let next = authority(response).await?;
        if next.epoch != self.epoch {
            return Err("relay_epoch_lost");
        }
        *self.fence.lock().await = Some(next.fence);
        *self.stream.lock().await = Stream::default();
        Ok(())
    }
    async fn service_keepalive(&self) -> Result<(), &'static str> {
        let mut stream = self.stream.lock().await;
        let Some((socket, _)) = stream.socket.as_mut() else {
            return Ok(());
        };
        let mut failure = None;
        // Bounded drain of transport-only traffic, independent of scene capture.
        for _ in 0..16 {
            match tokio::time::timeout(Duration::from_millis(1), socket.next()).await {
                Err(_) => break,
                Ok(Some(Ok(Message::Ping(bytes)))) => {
                    if !matches!(
                        tokio::time::timeout(
                            Duration::from_secs(2),
                            socket.send(Message::Pong(bytes))
                        )
                        .await,
                        Ok(Ok(()))
                    ) {
                        failure = Some("ws_keepalive_send_failed");
                        break;
                    }
                }
                Ok(Some(Ok(Message::Pong(_)))) => {}
                Ok(Some(Err(error))) => {
                    failure = Some(ws_failure(error));
                    break;
                }
                Ok(None | Some(Ok(Message::Close(_)))) => {
                    failure = Some("ws_idle_closed");
                    break;
                }
                _ => {
                    failure = Some("ws_idle_unexpected_message");
                    break;
                }
            }
        }
        if let Some(reason) = failure {
            *stream = Stream::default();
            // Keep the historical heartbeat result (HTTP authority is checked next),
            // but never hide loss of the idle publisher socket in diagnostics.
            let _ = crate::diagnostics::Attempt::begin(&self.observations, "keepalive", None)
                .finish(Err(reason));
        } else {
            crate::diagnostics::Attempt::begin(&self.observations, "keepalive", None)
                .finish(Ok(()))?;
        }
        Ok(())
    }
    async fn connect(
        &self,
        attempt: &crate::diagnostics::Attempt,
    ) -> Result<(Socket, bool), &'static str> {
        let mut url = self.base.join("ws").map_err(|_| "invalid_endpoint")?;
        let scheme = if url.scheme() == "https" { "wss" } else { "ws" };
        url.set_scheme(scheme).map_err(|_| "invalid_endpoint")?;
        let mut request = url
            .as_str()
            .into_client_request()
            .map_err(|_| "invalid_endpoint")?;
        let headers = request.headers_mut();
        // Offer only after an authenticated start advertises support. Legacy
        // relays retain their original handshake and repair envelope.
        if self.observe_protocol {
            headers.insert(
                "Sec-WebSocket-Protocol",
                "bmux-observe-v1".parse().expect("fixed protocol"),
            );
        }
        headers.insert(
            "Authorization",
            format!("Bearer {}", self.key)
                .parse()
                .map_err(|_| "invalid_credential")?,
        );
        headers.insert(
            "X-Bmux-Epoch",
            self.epoch
                .ok_or("not_started")?
                .to_string()
                .parse()
                .map_err(|_| "not_started")?,
        );
        headers.insert(
            "X-Bmux-Fence",
            self.fence
                .lock()
                .await
                .clone()
                .ok_or("not_started")?
                .parse()
                .map_err(|_| "not_started")?,
        );
        let config = tokio_tungstenite::tungstenite::protocol::WebSocketConfig::default()
            .max_message_size(Some(MAX_FRAME_BYTES))
            .max_frame_size(Some(MAX_FRAME_BYTES));
        tokio::time::timeout(
            Duration::from_secs(3),
            tokio_tungstenite::connect_async_with_config(request, Some(config), false),
        )
        .await
        .map_err(|_| "ws_connect_timeout")?
        .map(|(socket, response)| {
            attempt.http_status(response.status().as_u16());
            let observed = response
                .headers()
                .get("sec-websocket-protocol")
                .is_some_and(|value| value == "bmux-observe-v1");
            (socket, observed)
        })
        .map_err(|error| {
            if let tokio_tungstenite::tungstenite::Error::Http(response) = &error {
                attempt.http_status(response.status().as_u16());
            }
            ws_failure(error)
        })
    }
    async fn publish(
        &self,
        bytes: &[u8],
        attempt: &crate::diagnostics::Attempt,
    ) -> Result<(), &'static str> {
        if bytes.is_empty() || bytes.len() > MAX_FRAME_BYTES {
            return Err("invalid_frame");
        }
        let frame = bmux_web_renderer::decode_binary(bytes, None).map_err(|_| "invalid_frame")?;
        let mut stream = self.stream.lock().await;
        // Take the socket before awaiting: cancellation drops it and its unACKed
        // baseline, so the next upload must reconnect with a full repair.
        let (mut socket, observed) = match stream.socket.take() {
            Some(socket) => socket,
            None => {
                stream.base = None;
                self.connect(attempt).await?
            }
        };
        let body = stream
            .base
            .as_ref()
            .and_then(|base| bmux_web_renderer::encode_delta(&frame, base).ok())
            .filter(|delta| delta.len() < bytes.len())
            .unwrap_or_else(|| bytes.to_vec());
        let sent_len = body.len();
        let full = sent_len == bytes.len();
        stream.base = None;
        let mut repaired = false;
        let mut sent = false;
        let mut repair_sending = false;
        let result = tokio::time::timeout(Duration::from_secs(3), async {
            socket
                .send(Message::Binary(body.into()))
                .await
                .map_err(|_| "ws_send_failed")?;
            sent = true;
            attempt.sent(sent_len, full);
            loop {
                match socket
                    .next()
                    .await
                    .ok_or("ws_closed_before_ack")?
                    .map_err(ws_failure)?
                {
                    Message::Text(text) => {
                        if text.len() > 4096 {
                            return Err("ack_control_oversized");
                        }
                        let control: serde_json::Value =
                            serde_json::from_str(text.as_str()).map_err(|_| "ack_invalid_json")?;
                        if observed && control["type"].as_str() == Some("repair") {
                            attempt.peer_repair(repair_reason(&control)?);
                        }
                        match control["type"].as_str() {
                            Some("ack")
                                if control["generation"].as_str() == Some(frame.generation())
                                    && control["revision"].as_str()
                                        == Some(frame.revision().to_string().as_str()) =>
                            {
                                return Ok(());
                            }
                            Some("repair") if !repaired => {
                                repaired = true;
                                attempt.repair();
                                repair_sending = true;
                                socket
                                    .send(Message::Binary(bytes.to_vec().into()))
                                    .await
                                    .map_err(|_| "repair_send_failed")?;
                                repair_sending = false;
                                attempt.sent(bytes.len(), true);
                            }
                            Some("ack") => return Err("ack_mismatch"),
                            Some("repair") => return Err("repair_limit_exceeded"),
                            Some("error") => return Err("publisher_peer_error"),
                            _ => return Err("ack_unexpected_control"),
                        }
                    }
                    Message::Ping(bytes) => socket
                        .send(Message::Pong(bytes))
                        .await
                        .map_err(ws_failure)?,
                    Message::Pong(_) => {}
                    Message::Close(_) => return Err("ws_closed_before_ack"),
                    _ => return Err("ack_unexpected_message"),
                }
            }
        })
        .await
        .map_err(|_| {
            if !sent {
                "ws_send_timeout"
            } else if repair_sending {
                "repair_send_timeout"
            } else if repaired {
                "repair_ack_timeout"
            } else {
                "ack_timeout"
            }
        })?;
        if result.is_ok() {
            stream.socket = Some((socket, observed));
            stream.base = Some(frame);
        }
        result
    }
}
async fn authority(response: reqwest::Response) -> Result<Authority, &'static str> {
    if !response.status().is_success() {
        return Err(if matches!(response.status().as_u16(), 401 | 403) {
            "http_auth_rejected"
        } else if matches!(response.status().as_u16(), 409 | 410) {
            "relay_epoch_lost"
        } else if response.status().as_u16() == 429 {
            "http_rate_limited"
        } else if response.status().is_server_error() {
            "http_server_error"
        } else {
            "http_status_rejected"
        });
    }
    if response.content_length().is_some_and(|size| size > 4096) {
        return Err("http_authority_oversized");
    }
    let mut response = response;
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(http_failure)? {
        if bytes.len() + chunk.len() > 4096 {
            return Err("http_authority_oversized");
        }
        bytes.extend_from_slice(&chunk);
    }
    let value: Authority =
        serde_json::from_slice(&bytes).map_err(|_| "http_authority_invalid_json")?;
    if !decimal(&value.fence)
        || !["starting", "live", "paused", "offline"].contains(&value.state.as_str())
        || value.state != "offline" && value.epoch.is_none()
    {
        return Err("http_authority_invalid");
    }
    Ok(value)
}
pub(crate) fn validate_endpoint(
    endpoint: &str,
    channel: &str,
) -> Result<reqwest::Url, &'static str> {
    if channel.is_empty()
        || channel.len() > 48
        || !channel.as_bytes()[0].is_ascii_lowercase() && !channel.as_bytes()[0].is_ascii_digit()
        || !channel
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    {
        return Err("invalid_channel");
    }
    let mut base = reqwest::Url::parse(endpoint).map_err(|_| "invalid_endpoint")?;
    let loopback = base.host_str().is_some_and(|host| {
        host == "localhost"
            || host
                .parse::<std::net::IpAddr>()
                .is_ok_and(|ip| ip.is_loopback())
    });
    if !(base.scheme() == "https" || base.scheme() == "http" && loopback)
        || !base.username().is_empty()
        || base.password().is_some()
        || base.query().is_some()
        || base.fragment().is_some()
        || base.path() != "/"
    {
        return Err("invalid_endpoint");
    }
    base.set_path("/v1/publisher/");
    Ok(base)
}
pub(crate) fn credential_key(path: &Path) -> Result<String, &'static str> {
    #[cfg(unix)]
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    let file = options.open(path).map_err(|_| "credential_unavailable")?;
    let metadata = file.metadata().map_err(|_| "credential_unavailable")?;
    if !metadata.is_file() || metadata.len() > 4096 {
        return Err("invalid_credential");
    }
    #[cfg(unix)]
    if metadata.mode() & 0o077 != 0
        // SAFETY: geteuid has no arguments and observes only this process's identity.
        || metadata.uid() != unsafe { libc::geteuid() }
    {
        return Err("private_credential_required");
    }
    let mut key = String::new();
    file.take(4097)
        .read_to_string(&mut key)
        .map_err(|_| "invalid_credential")?;
    let key = key.trim();
    if !(32..=256).contains(&key.len()) || !key.bytes().all(|b| b.is_ascii_graphic()) {
        return Err("invalid_credential");
    }
    Ok(key.to_owned())
}
// Preserve the control-v1 compatibility classes; detailed reasons are diagnostics-v1 only.
fn repair_reason(control: &serde_json::Value) -> Result<&'static str, &'static str> {
    if control["observability_version"].as_u64() != Some(1) {
        return Err("repair_diagnostics_version_invalid");
    }
    // Closed labels only; arbitrary peer text must never reach logs/history.
    Ok(match control["reason"].as_str() {
        Some("oversize") => "oversize",
        Some("poisoned_authority") => "poisoned_authority",
        Some("resources") => "resources",
        Some("wire_version") => "wire_version",
        Some("decode_limit") => "decode_limit",
        Some("missing_delta_base") => "missing_delta_base",
        Some("delta_base_mismatch") => "delta_base_mismatch",
        Some("malformed") => "malformed",
        Some("unsupported") => "unsupported",
        Some("zero_revision") => "zero_revision",
        Some("stale_revision") => "stale_revision",
        _ => "unknown_relay_reason",
    })
}
fn legacy_failure(reason: &'static str) -> &'static str {
    match reason {
        "http_auth_rejected" => "relay_epoch_lost",
        "http_rate_limited" | "http_server_error" => "relay_transient",
        "http_timeout"
        | "http_connect_failed"
        | "http_transport_failed"
        | "ws_connect_timeout"
        | "ws_transport_failed"
        | "ws_send_failed"
        | "ws_closed_before_ack"
        | "ws_protocol_error"
        | "ack_timeout"
        | "repair_ack_timeout"
        | "repair_send_failed"
        | "ws_send_timeout"
        | "repair_send_timeout" => "relay_unreachable",
        "ws_auth_rejected" => "relay_unreachable",
        "ws_upgrade_rejected" => "relay_unreachable",
        "http_status_rejected"
        | "http_authority_invalid"
        | "http_authority_invalid_json"
        | "http_authority_oversized"
        | "ack_control_oversized"
        | "ack_invalid_json"
        | "ack_mismatch"
        | "repair_limit_exceeded"
        | "publisher_peer_error"
        | "ack_unexpected_control" => "relay_rejected",
        "ack_unexpected_message" => "relay_unreachable",
        other => other,
    }
}
fn http_failure(error: reqwest::Error) -> &'static str {
    if error.is_timeout() {
        "http_timeout"
    } else if error.is_connect() {
        "http_connect_failed"
    } else {
        "http_transport_failed"
    }
}
fn ws_failure(error: tokio_tungstenite::tungstenite::Error) -> &'static str {
    use tokio_tungstenite::tungstenite::Error;
    match error {
        Error::Http(response) if matches!(response.status().as_u16(), 401 | 403) => {
            "ws_auth_rejected"
        }
        Error::Http(_) => "ws_upgrade_rejected",
        Error::Protocol(_) | Error::Capacity(_) | Error::Utf8(_) => "ws_protocol_error",
        Error::AlreadyClosed | Error::ConnectionClosed => "ws_closed_before_ack",
        _ => "ws_transport_failed",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn negotiated_repair_reasons_are_closed_and_versioned() {
        assert_eq!(
            repair_reason(
                &serde_json::json!({"reason":"stale_revision","observability_version":1})
            ),
            Ok("stale_revision")
        );
        assert_eq!(
            repair_reason(
                &serde_json::json!({"reason":"private-terminal-secret","observability_version":1})
            ),
            Ok("unknown_relay_reason")
        );
        assert_eq!(
            repair_reason(&serde_json::json!({"reason":"malformed"})),
            Err("repair_diagnostics_version_invalid")
        );
    }
    #[tokio::test]
    async fn failed_source_transition_discards_acknowledged_baseline() {
        // Nextest isolates each test: do not rely on Relay::new in another test
        // installing the provider before this fixture constructs its HTTP client.
        let _ = rustls::crypto::ring::default_provider().install_default();
        let mut relay = Relay {
            observations: crate::diagnostics::Shared::default(),
            client: reqwest::Client::new(),
            base: reqwest::Url::parse("http://127.0.0.1:1/v1/publisher/").unwrap(),
            key: "unused".into(),
            publisher: Uuid::new_v4(),
            observe_protocol: false,
            epoch: None,
            fence: Mutex::new(None),
            stream: Mutex::new(Stream::default()),
        };
        let scene = bmux_tui::presentation::PresentationState::new(bmux_tui::geometry::Rect::new(
            0, 0, 2, 1,
        ));
        let prepared = scene
            .prepare(bmux_tui::damage::Damage::Full, |_| {})
            .unwrap();
        relay.stream.get_mut().base = Some(
            bmux_web_renderer::BrowserFrame::from_prepared_in_generation(
                "previous-view",
                1,
                &prepared,
            )
            .unwrap(),
        );
        assert_eq!(relay.mutation("resume", None).await, Err("not_started"));
        assert!(relay.stream.lock().await.base.is_none());
    }

    async fn control_fixture(
        messages: &[&str],
        lost_ack: bool,
    ) -> (
        Result<(), &'static str>,
        bmux_broadcast_plugin_api::broadcast_diagnostics_v1::Snapshot,
        String,
    ) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let messages: Vec<String> = messages.iter().map(|s| (*s).into()).collect();
        let worker = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
            socket.next().await.unwrap().unwrap();
            for (index, message) in messages.into_iter().enumerate() {
                let repair = message.contains("repair");
                socket.send(Message::Text(message.into())).await.unwrap();
                if repair && index == 0 {
                    socket.next().await.unwrap().unwrap();
                }
            }
            if lost_ack {
                tokio::time::sleep(Duration::from_secs(4)).await;
            }
        });
        let (socket, _) = tokio_tungstenite::connect_async(format!("ws://{address}"))
            .await
            .unwrap();
        let observations = crate::diagnostics::Shared::default();
        observations.lock().unwrap().source(1, false);
        let relay = Relay {
            observations: observations.clone(),
            client: reqwest::Client::new(),
            base: reqwest::Url::parse("http://127.0.0.1:1/").unwrap(),
            key: "PRIVATE-CREDENTIAL".into(),
            publisher: Uuid::new_v4(),
            observe_protocol: false,
            epoch: Some(Uuid::new_v4()),
            fence: Mutex::new(Some("PRIVATE-FENCE".into())),
            stream: Mutex::new(Stream {
                socket: Some((socket, false)),
                base: None,
            }),
        };
        let scene = bmux_tui::presentation::PresentationState::new(bmux_tui::geometry::Rect::new(
            0, 0, 2, 1,
        ));
        let prepared = scene
            .prepare(bmux_tui::damage::Damage::Full, |_| {})
            .unwrap();
        let frame = bmux_web_renderer::BrowserFrame::from_prepared_in_generation(
            "PRIVATE-GENERATION",
            1,
            &prepared,
        )
        .unwrap();
        let bytes = bmux_web_renderer::encode_binary(&frame).unwrap();
        let result = relay.mutation("frame", Some((1, &bytes))).await;
        assert!(relay.stream.lock().await.base.is_none());
        let state = observations.lock().unwrap();
        let snapshot = state.snapshot("running", "unknown", 1, 0, 0);
        let json = serde_json::to_string(&state.events(50)).unwrap();
        worker.abort();
        (result, snapshot, json)
    }
    #[tokio::test]
    async fn peer_text_is_never_a_diagnostic() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let (result, snapshot, json) = control_fixture(
            &[r#"{"type":"error","message":"PRIVATE-PEER-TEXT"}"#],
            false,
        )
        .await;
        assert_eq!(result, Err("relay_rejected"));
        assert_eq!(
            snapshot.current_failure.unwrap().reason,
            "publisher_peer_error"
        );
        for secret in [
            "PRIVATE-PEER-TEXT",
            "PRIVATE-CREDENTIAL",
            "PRIVATE-FENCE",
            "PRIVATE-GENERATION",
        ] {
            assert!(!json.contains(secret));
        }
        let (result, snapshot, _) = control_fixture(
            &[r#"{"type":"ack","generation":"wrong","revision":"1"}"#],
            false,
        )
        .await;
        assert_eq!(result, Err("relay_rejected"));
        assert_eq!(snapshot.current_failure.unwrap().reason, "ack_mismatch");
        let (_, snapshot, _) = control_fixture(&["not json"], false).await;
        assert_eq!(snapshot.current_failure.unwrap().reason, "ack_invalid_json");
    }
    #[tokio::test]
    async fn lost_ack_is_precise_and_does_not_commit_a_base() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let (result, snapshot, _) = control_fixture(&[], true).await;
        assert_eq!(result, Err("relay_unreachable"));
        assert_eq!(snapshot.current_failure.unwrap().reason, "ack_timeout");
        assert_eq!(snapshot.acknowledgements, 0);
        assert_eq!(snapshot.progress, "pending");
    }
    #[tokio::test]
    async fn repair_lost_ack_and_oversized_control_have_distinct_reasons() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let (result, snapshot, _) = control_fixture(&[r#"{"type":"repair"}"#], true).await;
        assert_eq!(result, Err("relay_unreachable"));
        assert_eq!(
            snapshot.current_failure.unwrap().reason,
            "repair_ack_timeout"
        );
        assert_eq!(snapshot.repairs, 1);
        let oversized = "x".repeat(4097);
        let (result, snapshot, _) = control_fixture(&[&oversized], false).await;
        assert_eq!(result, Err("relay_rejected"));
        assert_eq!(
            snapshot.current_failure.unwrap().reason,
            "ack_control_oversized"
        );
    }

    #[tokio::test]
    async fn repeated_repair_is_bounded_and_correlated() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let (result, snapshot, json) =
            control_fixture(&[r#"{"type":"repair"}"#, r#"{"type":"repair"}"#], false).await;
        assert_eq!(result, Err("relay_rejected"));
        assert_eq!(snapshot.repairs, 1);
        assert_eq!(
            snapshot.current_failure.unwrap().reason,
            "repair_limit_exceeded"
        );
        assert!(json.contains("repair_limit_exceeded"));
    }

    #[tokio::test]
    async fn authority_http_failures_are_safe_precise_and_compatible() {
        use std::io::{Read, Write};
        let _ = rustls::crypto::ring::default_provider().install_default();
        for (status, body, reason, legacy) in [
            (
                401,
                "PRIVATE-PEER-TEXT",
                "http_auth_rejected",
                "relay_epoch_lost",
            ),
            (
                409,
                "PRIVATE-PEER-TEXT",
                "relay_epoch_lost",
                "relay_epoch_lost",
            ),
            (
                429,
                "PRIVATE-PEER-TEXT",
                "http_rate_limited",
                "relay_transient",
            ),
            (
                503,
                "PRIVATE-PEER-TEXT",
                "http_server_error",
                "relay_transient",
            ),
            (
                400,
                "PRIVATE-PEER-TEXT",
                "http_status_rejected",
                "relay_rejected",
            ),
            (
                200,
                "PRIVATE-PEER-TEXT",
                "http_authority_invalid_json",
                "relay_rejected",
            ),
        ] {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let address = listener.local_addr().unwrap();
            let worker = std::thread::spawn(move || {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = [0; 4096];
                let _ = stream.read(&mut request).unwrap();
                write!(stream, "HTTP/1.1 {status} Test\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
            });
            let response = reqwest::Client::new()
                .get(format!("http://{address}"))
                .send()
                .await
                .unwrap();
            let error = authority(response).await.err().unwrap();
            assert_eq!(error, reason);
            assert_eq!(legacy_failure(error), legacy);
            assert!(!error.contains("PRIVATE"));
            worker.join().unwrap();
        }
    }

    #[test]
    fn rejects_insecure_or_credential_bearing_endpoints_before_file_io() {
        for endpoint in [
            "http://example.com",
            "https://user:secret@example.com",
            "https://example.com/?key=x",
            "https://example.com/#secret",
            "https://example.com/path",
        ] {
            assert!(matches!(
                Relay::new(endpoint, "main", Path::new("/missing")),
                Err("invalid_endpoint")
            ));
        }
    }
    #[test]
    fn channel_is_a_single_safe_component() {
        for channel in ["", "../main", "Main", "a/b", "-main"] {
            assert!(matches!(
                Relay::new("https://example.com", channel, Path::new("/missing")),
                Err("invalid_channel")
            ));
        }
    }
}
