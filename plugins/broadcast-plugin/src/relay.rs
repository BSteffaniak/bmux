//! Content-free HTTP adapter. Credentials never implement Debug and errors are codes.
use serde::Deserialize;
use std::{io::Read, path::Path, time::Duration};
use uuid::Uuid;

pub const MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;

pub struct Relay {
    client: reqwest::Client,
    base: reqwest::Url,
    key: String,
    publisher: Uuid,
    pub epoch: Option<Uuid>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Started {
    protocol: u32,
    renderer: String,
    epoch: Uuid,
    lease_ms: u64,
}

impl Relay {
    pub fn new(endpoint: &str, channel: &str, credential: &Path) -> Result<Self, &'static str> {
        let base = validate_endpoint(endpoint, channel)?;
        Self::from_base(base, credential)
    }
    fn from_base(base: reqwest::Url, credential: &Path) -> Result<Self, &'static str> {
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
        })
    }
    pub async fn start(&mut self) -> Result<(), &'static str> {
        let url = self.base.join("start").map_err(|_| "invalid_endpoint")?;
        let mut response = self.client.post(url).bearer_auth(&self.key)
            .json(&serde_json::json!({"protocol":1,"renderer":"bmwp-3","publisher_id":self.publisher}))
            .send().await.map_err(|_| "relay_unreachable")?;
        if response.status() != reqwest::StatusCode::OK {
            return Err("relay_rejected");
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|_| "relay_unreachable")? {
            if bytes.len() + chunk.len() > 4096 {
                return Err("relay_protocol");
            }
            bytes.extend_from_slice(&chunk);
        }
        let started: Started = serde_json::from_slice(&bytes).map_err(|_| "relay_protocol")?;
        if started.protocol != 1 || started.renderer != "bmwp-3" || started.lease_ms != 15_000 {
            return Err("relay_protocol");
        }
        self.epoch = Some(started.epoch);
        Ok(())
    }
    pub async fn mutation(
        &self,
        operation: &str,
        frame: Option<(u64, &[u8])>,
    ) -> Result<(), &'static str> {
        let epoch = self.epoch.ok_or("not_started")?;
        let url = self.base.join(operation).map_err(|_| "invalid_endpoint")?;
        let request = if let Some((revision, bytes)) = frame {
            if revision == 0 || bytes.is_empty() || bytes.len() > MAX_FRAME_BYTES {
                return Err("invalid_frame");
            }
            self.client
                .put(url)
                .header("Content-Type", "application/octet-stream")
                .header("X-Bmux-Revision", revision.to_string())
                .body(bytes.to_vec())
        } else {
            self.client.post(url)
        };
        let response = request
            .bearer_auth(&self.key)
            .header("X-Bmux-Epoch", epoch.to_string())
            .send()
            .await
            .map_err(|_| "relay_unreachable")?;
        if response.status() == reqwest::StatusCode::NO_CONTENT {
            Ok(())
        } else if response.status() == reqwest::StatusCode::CONFLICT
            || response.status() == reqwest::StatusCode::GONE
        {
            Err("relay_epoch_lost")
        } else if response.status().is_server_error()
            || response.status() == reqwest::StatusCode::TOO_MANY_REQUESTS
        {
            Err("relay_transient")
        } else {
            Err("relay_rejected")
        }
    }
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
    base.set_path(&format!("/api/live/{channel}/publisher/"));
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
