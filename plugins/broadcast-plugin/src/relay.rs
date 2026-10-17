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
    socket: Option<Socket>,
    base: Option<bmux_web_renderer::BrowserFrame>,
}
pub struct Relay {
    client: reqwest::Client,
    base: reqwest::Url,
    key: String,
    publisher: Uuid,
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
            client,
            base,
            key,
            publisher: Uuid::new_v4(),
            epoch: None,
            fence: Mutex::new(None),
            stream: Mutex::new(Stream::default()),
        })
    }
    pub async fn start(&mut self) -> Result<(), &'static str> {
        let response = self
            .client
            .post(self.base.join("start").map_err(|_| "invalid_endpoint")?)
            .bearer_auth(&self.key)
            .json(&serde_json::json!({"publisher_id":self.publisher}))
            .send()
            .await
            .map_err(|_| "relay_unreachable")?;
        let authority = authority(response).await?;
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
        if operation == "frame" {
            let (_, bytes) = frame.ok_or("invalid_frame")?;
            return self.publish(bytes).await;
        }
        if !["heartbeat", "pause", "resume", "stop"].contains(&operation) {
            return Err("invalid_operation");
        }
        if operation == "heartbeat" {
            self.service_keepalive().await?;
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
            .map_err(|_| "relay_unreachable")?;
        let next = match authority(response).await {
            Err("relay_epoch_lost") if operation == "heartbeat" => {
                self.discover_fence().await?;
                return Err("relay_transient");
            }
            result => result?,
        };
        if next.epoch.is_some_and(|next| next != epoch) {
            return Err("relay_epoch_lost");
        }
        *self.fence.lock().await = Some(next.fence);
        if operation != "heartbeat" {
            *self.stream.lock().await = Stream::default();
        }
        Ok(())
    }
    /// Reconcile a durable relay restart without guessing a new authority.
    async fn discover_fence(&self) -> Result<(), &'static str> {
        let response = self
            .client
            .get(self.base.join("status").map_err(|_| "invalid_endpoint")?)
            .bearer_auth(&self.key)
            .header("X-Bmux-Epoch", self.epoch.ok_or("not_started")?.to_string())
            .send()
            .await
            .map_err(|_| "relay_unreachable")?;
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
        let Some(socket) = stream.socket.as_mut() else {
            return Ok(());
        };
        let mut failed = false;
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
                        failed = true;
                        break;
                    }
                }
                Ok(Some(Ok(Message::Pong(_)))) => {}
                _ => {
                    failed = true;
                    break;
                }
            }
        }
        if failed {
            *stream = Stream::default();
        }
        Ok(())
    }
    async fn connect(&self) -> Result<Socket, &'static str> {
        let mut url = self.base.join("ws").map_err(|_| "invalid_endpoint")?;
        let scheme = if url.scheme() == "https" { "wss" } else { "ws" };
        url.set_scheme(scheme).map_err(|_| "invalid_endpoint")?;
        let mut request = url
            .as_str()
            .into_client_request()
            .map_err(|_| "invalid_endpoint")?;
        let headers = request.headers_mut();
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
        .map_err(|_| "relay_unreachable")?
        .map(|(socket, _)| socket)
        .map_err(|_| "relay_unreachable")
    }
    async fn publish(&self, bytes: &[u8]) -> Result<(), &'static str> {
        if bytes.is_empty() || bytes.len() > MAX_FRAME_BYTES {
            return Err("invalid_frame");
        }
        let frame = bmux_web_renderer::decode_binary(bytes, None).map_err(|_| "invalid_frame")?;
        let mut stream = self.stream.lock().await;
        // Take the socket before awaiting: cancellation drops it and its unACKed
        // baseline, so the next upload must reconnect with a full repair.
        let mut socket = match stream.socket.take() {
            Some(socket) => socket,
            None => {
                stream.base = None;
                self.connect().await?
            }
        };
        let body = stream
            .base
            .as_ref()
            .and_then(|base| bmux_web_renderer::encode_delta(&frame, base).ok())
            .filter(|delta| delta.len() < bytes.len())
            .unwrap_or_else(|| bytes.to_vec());
        stream.base = None;
        let result = tokio::time::timeout(Duration::from_secs(3), async {
            socket
                .send(Message::Binary(body.into()))
                .await
                .map_err(|_| "relay_unreachable")?;
            let mut repaired = false;
            loop {
                match socket
                    .next()
                    .await
                    .ok_or("relay_unreachable")?
                    .map_err(|_| "relay_unreachable")?
                {
                    Message::Text(text) => {
                        if text.len() > 4096 {
                            return Err("relay_rejected");
                        }
                        let control: serde_json::Value =
                            serde_json::from_str(text.as_str()).map_err(|_| "relay_rejected")?;
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
                                socket
                                    .send(Message::Binary(bytes.to_vec().into()))
                                    .await
                                    .map_err(|_| "relay_unreachable")?;
                            }
                            _ => return Err("relay_rejected"),
                        }
                    }
                    Message::Ping(bytes) => socket
                        .send(Message::Pong(bytes))
                        .await
                        .map_err(|_| "relay_unreachable")?,
                    Message::Pong(_) => {}
                    _ => return Err("relay_unreachable"),
                }
            }
        })
        .await
        .map_err(|_| "relay_unreachable")?;
        if result.is_ok() {
            stream.socket = Some(socket);
            stream.base = Some(frame);
        }
        result
    }
}
async fn authority(response: reqwest::Response) -> Result<Authority, &'static str> {
    if !response.status().is_success() {
        return Err(
            if matches!(response.status().as_u16(), 409 | 410 | 401 | 403) {
                "relay_epoch_lost"
            } else if response.status().is_server_error() || response.status().as_u16() == 429 {
                "relay_transient"
            } else {
                "relay_rejected"
            },
        );
    }
    if response.content_length().is_some_and(|size| size > 4096) {
        return Err("relay_rejected");
    }
    let mut response = response;
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| "relay_unreachable")? {
        if bytes.len() + chunk.len() > 4096 {
            return Err("relay_rejected");
        }
        bytes.extend_from_slice(&chunk);
    }
    let value: Authority = serde_json::from_slice(&bytes).map_err(|_| "relay_rejected")?;
    if !decimal(&value.fence)
        || !["starting", "live", "paused", "offline"].contains(&value.state.as_str())
        || value.state != "offline" && value.epoch.is_none()
    {
        return Err("relay_rejected");
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
#[cfg(test)]
mod tests {
    use super::*;
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
