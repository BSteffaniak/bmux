use anyhow::{Context, Result};
use bmux_config::{BmuxConfig, CompressionMode, IrohSshAuthorizedKey};
use git_sshripped_ssh_agent::{
    ChallengeProof, DEFAULT_SSHSIG_NAMESPACE, sign_challenge_with_any_agent_key,
    verify_challenge_proof,
};
use iroh::endpoint::{Connection, RecvStream, SendStream};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use uuid::Uuid;

const AUTH_PROTOCOL_VERSION: u8 = 1;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedIrohTarget {
    pub endpoint_id: String,
    pub relay_url: Option<String>,
    pub require_ssh_auth: bool,
    pub transport_compression: IrohTargetCompression,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IrohTargetCompression {
    Auto,
    None,
    Zstd,
}

impl IrohTargetCompression {
    #[must_use]
    pub const fn as_query_value(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::None => "none",
            Self::Zstd => "zstd",
        }
    }

    fn parse(value: &str) -> Result<Self> {
        if value.eq_ignore_ascii_case("auto") {
            Ok(Self::Auto)
        } else if value.eq_ignore_ascii_case("none") {
            Ok(Self::None)
        } else if value.eq_ignore_ascii_case("zstd") {
            Ok(Self::Zstd)
        } else {
            anyhow::bail!("unsupported iroh compression mode '{value}' (expected auto|none|zstd)")
        }
    }
}

#[must_use]
pub const fn iroh_target_compression_from_config(config: &BmuxConfig) -> IrohTargetCompression {
    if !config.behavior.compression.enabled {
        return IrohTargetCompression::None;
    }
    match config.behavior.compression.remote {
        CompressionMode::Auto | CompressionMode::Zstd => IrohTargetCompression::Zstd,
        CompressionMode::None | CompressionMode::Lz4 => IrohTargetCompression::None,
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ClientAuthMessage {
    Hello { version: u8 },
    Proof { proof: ChallengeProofWire },
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ServerAuthMessage {
    Challenge {
        challenge: Vec<u8>,
        allowed_fingerprints: Vec<String>,
    },
    Result {
        ok: bool,
        error: Option<String>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ChallengeProofWire {
    fingerprint: String,
    public_key_openssh: String,
    signature_pem: String,
}

impl From<ChallengeProof> for ChallengeProofWire {
    fn from(value: ChallengeProof) -> Self {
        Self {
            fingerprint: value.fingerprint,
            public_key_openssh: value.public_key_openssh,
            signature_pem: value.signature_pem,
        }
    }
}

impl From<ChallengeProofWire> for ChallengeProof {
    fn from(value: ChallengeProofWire) -> Self {
        Self {
            fingerprint: value.fingerprint,
            public_key_openssh: value.public_key_openssh,
            signature_pem: value.signature_pem,
        }
    }
}

#[must_use]
pub const fn iroh_ssh_access_enabled(config: &BmuxConfig) -> bool {
    config.connections.iroh_ssh_access.enabled
}

pub fn ensure_iroh_ssh_access_ready(config: &BmuxConfig) -> Result<()> {
    let access = &config.connections.iroh_ssh_access;
    if access.enabled && access.allowlist.is_empty() {
        anyhow::bail!(
            "iroh SSH access is enabled but no keys are authorized; run `bmux access add ...`"
        );
    }
    Ok(())
}

#[must_use]
pub fn iroh_target_url(
    endpoint_id: &str,
    relay_url: Option<&str>,
    require_ssh_auth: bool,
    transport_compression: IrohTargetCompression,
) -> String {
    let mut query_parts = Vec::new();
    if let Some(relay) = relay_url.filter(|value| !value.trim().is_empty()) {
        query_parts.push(format!("relay={relay}"));
    }
    if require_ssh_auth {
        query_parts.push("auth=ssh".to_string());
    }
    if !matches!(transport_compression, IrohTargetCompression::Auto) {
        query_parts.push(format!(
            "compression={}",
            transport_compression.as_query_value()
        ));
    }

    if query_parts.is_empty() {
        format!("iroh://{endpoint_id}")
    } else {
        format!("iroh://{endpoint_id}?{}", query_parts.join("&"))
    }
}

pub fn parse_iroh_target(input: &str) -> Result<ParsedIrohTarget> {
    let trimmed = input.trim();
    let raw = trimmed.strip_prefix("iroh://").unwrap_or(trimmed).trim();
    let (endpoint_raw, query_raw) = match raw.split_once('?') {
        Some((endpoint, query)) => (endpoint.trim(), Some(query)),
        None => (raw, None),
    };
    if endpoint_raw.is_empty() {
        anyhow::bail!("iroh target must include an endpoint id");
    }

    let mut relay_url = None;
    let mut require_ssh_auth = false;
    let mut transport_compression = IrohTargetCompression::Auto;
    if let Some(query) = query_raw {
        for pair in query.split('&').filter(|part| !part.trim().is_empty()) {
            let (key, value) = match pair.split_once('=') {
                Some((key, value)) => (key.trim(), value.trim()),
                None => (pair.trim(), ""),
            };
            if key.eq_ignore_ascii_case("relay") && !value.is_empty() {
                relay_url = Some(value.to_string());
            }
            if key.eq_ignore_ascii_case("auth") && value.eq_ignore_ascii_case("ssh") {
                require_ssh_auth = true;
            }
            if key.eq_ignore_ascii_case("compression") {
                transport_compression = IrohTargetCompression::parse(value)?;
            }
        }
    }

    Ok(ParsedIrohTarget {
        endpoint_id: endpoint_raw.to_string(),
        relay_url,
        require_ssh_auth,
        transport_compression,
    })
}

pub async fn authenticate_client_connection(connection: &Connection) -> Result<()> {
    let (mut send, recv) = connection
        .open_bi()
        .await
        .context("failed opening iroh auth stream")?;
    let mut reader = BufReader::new(recv);
    write_message(
        &mut send,
        &ClientAuthMessage::Hello {
            version: AUTH_PROTOCOL_VERSION,
        },
    )
    .await?;

    let challenge = match read_message::<ServerAuthMessage>(&mut reader).await? {
        ServerAuthMessage::Challenge {
            challenge,
            allowed_fingerprints,
        } => (challenge, allowed_fingerprints),
        ServerAuthMessage::Result { ok: false, error } => {
            anyhow::bail!(
                "iroh SSH auth rejected: {}",
                error.unwrap_or_else(|| "unknown error".to_string())
            );
        }
        ServerAuthMessage::Result { .. } => {
            anyhow::bail!("unexpected iroh SSH auth response from host")
        }
    };

    let Some(proof) =
        sign_challenge_with_any_agent_key(&challenge.0, DEFAULT_SSHSIG_NAMESPACE, &challenge.1)?
    else {
        anyhow::bail!(
            "no suitable SSH agent key available for host allowlist; load a matching key into ssh-agent"
        );
    };

    write_message(
        &mut send,
        &ClientAuthMessage::Proof {
            proof: proof.into(),
        },
    )
    .await?;

    match read_message::<ServerAuthMessage>(&mut reader).await? {
        ServerAuthMessage::Result { ok: true, .. } => {
            send.finish()
                .context("failed finalizing iroh auth stream")?;
            Ok(())
        }
        ServerAuthMessage::Result { ok: false, error } => anyhow::bail!(
            "iroh SSH auth failed: {}",
            error.unwrap_or_else(|| "unknown error".to_string())
        ),
        _ => anyhow::bail!("unexpected iroh SSH auth result from host"),
    }
}

pub async fn authenticate_host_connection(
    connection: &Connection,
    allowlist: &BTreeMap<String, IrohSshAuthorizedKey>,
) -> Result<String> {
    let (mut send, recv) = connection
        .accept_bi()
        .await
        .context("missing iroh auth stream")?;
    let mut reader = BufReader::new(recv);

    let hello = read_message::<ClientAuthMessage>(&mut reader).await;
    let ClientAuthMessage::Hello { version } =
        hello.map_err(|error| anyhow::anyhow!("failed reading iroh SSH auth hello: {error}"))?
    else {
        let _ = send_rejection(&mut send, "expected auth hello").await;
        anyhow::bail!("missing iroh SSH auth hello");
    };
    if version != AUTH_PROTOCOL_VERSION {
        let _ = send_rejection(&mut send, "unsupported auth protocol version").await;
        anyhow::bail!("unsupported iroh SSH auth protocol version");
    }

    let challenge = generate_challenge();
    let allowed_fingerprints = allowlist.keys().cloned().collect::<Vec<_>>();
    write_message(
        &mut send,
        &ServerAuthMessage::Challenge {
            challenge: challenge.to_vec(),
            allowed_fingerprints,
        },
    )
    .await
    .context("failed writing iroh SSH challenge")?;

    let proof = match read_message::<ClientAuthMessage>(&mut reader).await {
        Ok(ClientAuthMessage::Proof { proof }) => proof,
        Ok(_) => {
            let _ = send_rejection(&mut send, "expected auth proof").await;
            anyhow::bail!("invalid iroh SSH auth flow");
        }
        Err(error) => {
            let _ = send_rejection(&mut send, "failed reading auth proof").await;
            return Err(error).context("failed reading iroh SSH auth proof");
        }
    };

    let proof = ChallengeProof::from(proof);
    let Some(allowed_key) = allowlist.get(&proof.fingerprint) else {
        let _ = send_rejection(&mut send, "SSH key fingerprint is not authorized").await;
        anyhow::bail!("SSH key fingerprint is not authorized");
    };

    if !public_key_material_matches(&allowed_key.public_key, &proof.public_key_openssh) {
        let _ = send_rejection(&mut send, "SSH proof key does not match allowlisted key").await;
        anyhow::bail!("SSH proof key does not match allowlisted key");
    }

    if let Err(error) = verify_challenge_proof(&challenge, DEFAULT_SSHSIG_NAMESPACE, &proof) {
        let _ = send_rejection(&mut send, "invalid SSH challenge signature").await;
        return Err(error).context("invalid SSH challenge signature");
    }

    write_message(
        &mut send,
        &ServerAuthMessage::Result {
            ok: true,
            error: None,
        },
    )
    .await
    .context("failed writing iroh SSH auth success")?;
    send.finish()
        .context("failed finalizing iroh SSH auth success")?;
    Ok(proof.fingerprint)
}

async fn send_rejection(send: &mut SendStream, message: &str) -> Result<()> {
    write_message(
        send,
        &ServerAuthMessage::Result {
            ok: false,
            error: Some(message.to_string()),
        },
    )
    .await?;
    send.finish().context("failed finishing rejection stream")
}

fn public_key_material_matches(left: &str, right: &str) -> bool {
    key_material(left) == key_material(right)
}

fn key_material(line: &str) -> String {
    line.split_whitespace()
        .take(2)
        .collect::<Vec<_>>()
        .join(" ")
}

fn generate_challenge() -> [u8; 32] {
    let mut challenge = [0_u8; 32];
    challenge[..16].copy_from_slice(Uuid::new_v4().as_bytes());
    challenge[16..].copy_from_slice(Uuid::new_v4().as_bytes());
    challenge
}

async fn write_message<T: Serialize + Sync>(send: &mut SendStream, message: &T) -> Result<()> {
    let bytes = serde_json::to_vec(message).context("failed serializing iroh SSH auth message")?;
    send.write_all(&bytes)
        .await
        .context("failed writing iroh SSH auth message")?;
    send.write_all(b"\n")
        .await
        .context("failed writing iroh SSH auth delimiter")?;
    send.flush()
        .await
        .context("failed flushing iroh SSH auth message")
}

async fn read_message<T: DeserializeOwned>(reader: &mut BufReader<RecvStream>) -> Result<T> {
    let mut line = String::new();
    let bytes = reader
        .read_line(&mut line)
        .await
        .context("failed reading iroh SSH auth message")?;
    if bytes == 0 {
        anyhow::bail!("iroh SSH auth stream closed unexpectedly");
    }
    serde_json::from_str::<T>(line.trim_end()).context("failed parsing iroh SSH auth message")
}

#[cfg(test)]
mod tests {
    use super::{IrohTargetCompression, iroh_target_url, parse_iroh_target};

    #[test]
    fn parse_iroh_target_extracts_relay_and_auth_query() {
        let parsed = parse_iroh_target(
            "iroh://node-123?relay=https://relay.example&auth=ssh&compression=zstd",
        )
        .expect("parse target");
        assert_eq!(parsed.endpoint_id, "node-123");
        assert_eq!(parsed.relay_url.as_deref(), Some("https://relay.example"));
        assert!(parsed.require_ssh_auth);
        assert_eq!(parsed.transport_compression, IrohTargetCompression::Zstd);
    }

    #[test]
    fn iroh_target_url_emits_expected_query_order() {
        let url = iroh_target_url(
            "node-abc",
            Some("https://relay.example"),
            true,
            IrohTargetCompression::None,
        );
        assert_eq!(
            url,
            "iroh://node-abc?relay=https://relay.example&auth=ssh&compression=none"
        );
    }

    #[test]
    fn parse_iroh_target_accepts_plain_endpoint_without_scheme() {
        let parsed = parse_iroh_target("node-plain").expect("parse target");
        assert_eq!(parsed.endpoint_id, "node-plain");
        assert!(parsed.relay_url.is_none());
        assert!(!parsed.require_ssh_auth);
        assert_eq!(parsed.transport_compression, IrohTargetCompression::Auto);
    }
}
