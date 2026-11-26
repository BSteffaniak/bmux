//! The `bmux/pair/1` exchange: bounded frames, the transcript, its proof of
//! the link secret, and the comparison phrase.
//!
//! ```text
//! device -> host  Hello { version, role, device_name, proof }
//! host -> device  Pending { phrase }            (after the proof checks)
//! host -> device  Approved { principal, host_name, role } | Refused { reason }
//! ```
//!
//! The QUIC handshake already proves both keys: the device dials the host key
//! from the link, and the host reads the device key from the connection.
use crate::link::Secret;
use crate::trust::Role;
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use uuid::Uuid;

pub const VERSION: u32 = 1;
pub const MAX_FRAME: usize = 4_096;
pub const MAX_DEVICE_NAME: usize = 64;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case", tag = "type")]
pub enum DeviceMessage {
    Hello {
        version: u32,
        role: Role,
        device_name: String,
        /// Hex SHA-256 proof of the link secret over the transcript.
        proof: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case", tag = "type")]
pub enum HostMessage {
    Pending { phrase: String },
    /// `role` is the role the owner granted (the pairing session's), which
    /// may differ from the one the device asked for.
    Approved {
        principal: Uuid,
        host_name: String,
        role: Role,
    },
    Refused { reason: Refusal },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Refusal {
    /// The owner declined.
    Rejected,
    /// The link expired or the pairing session ended.
    Expired,
    /// The owner cancelled pairing.
    Cancelled,
    /// The proof did not match the link secret, or the request was malformed.
    Invalid,
    /// Too many requests are waiting.
    Busy,
    /// The host could not complete approval.
    Failed,
}

impl Refusal {
    pub const fn describe(self) -> &'static str {
        match self {
            Self::Rejected => "the host owner rejected this device",
            Self::Expired => "the pairing link expired or was already used",
            Self::Cancelled => "pairing was cancelled on the host",
            Self::Invalid => "the pairing link is not valid for this host",
            Self::Busy => "the host is handling too many pairing requests; try again",
            Self::Failed => "the host could not complete pairing",
        }
    }
}

const fn role_byte(role: Role) -> u8 {
    match role {
        Role::Full => 1,
        Role::ViewOnly => 2,
    }
}

/// Binds the host key, device key, secret, requested role and protocol version.
pub fn transcript(
    host_key: &iroh::PublicKey,
    device_key: &iroh::PublicKey,
    secret: &Secret,
    role: Role,
) -> [u8; 32] {
    let mut hash = Sha256::new();
    hash.update(b"bmux/pair/1 transcript\0");
    hash.update(VERSION.to_be_bytes());
    hash.update(host_key.as_bytes());
    hash.update(device_key.as_bytes());
    hash.update([role_byte(role)]);
    hash.update(Sha256::digest(secret.0));
    hash.finalize().into()
}

fn keyed(label: &[u8], secret: &Secret, transcript: &[u8; 32]) -> [u8; 32] {
    let mut hash = Sha256::new();
    hash.update(label);
    hash.update(secret.0);
    hash.update(transcript);
    hash.finalize().into()
}

pub fn proof(secret: &Secret, transcript: &[u8; 32]) -> [u8; 32] {
    keyed(b"bmux/pair/1 proof\0", secret, transcript)
}

/// `xxxx-xxxx-xxxx-xxxx` (lowercase hex), shown on both devices.
pub fn phrase(secret: &Secret, transcript: &[u8; 32]) -> String {
    let digest = keyed(b"bmux/pair/1 phrase\0", secret, transcript);
    digest[..8]
        .chunks(2)
        .map(|pair| format!("{:02x}{:02x}", pair[0], pair[1]))
        .collect::<Vec<_>>()
        .join("-")
}

pub fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes.iter().fold(String::new(), |mut out, byte| {
        let _ = write!(out, "{byte:02x}");
        out
    })
}

/// Constant-time comparison of a presented hex proof with the expected one.
pub fn proof_matches(presented: &str, expected: &[u8; 32]) -> bool {
    let expected = hex(expected);
    presented.len() == expected.len()
        && presented
            .bytes()
            .zip(expected.bytes())
            .fold(0_u8, |diff, (a, b)| diff | (a ^ b))
            == 0
}

/// Untrusted labels: printable, bounded, no control characters.
pub fn sanitize_name(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        .take(MAX_DEVICE_NAME)
        .collect();
    let cleaned = cleaned.trim_start_matches(['-', '.']).to_owned();
    if cleaned.is_empty() {
        "device".to_owned()
    } else {
        cleaned
    }
}

pub async fn write_frame<T: Serialize + Sync>(
    writer: &mut (impl AsyncWrite + Unpin),
    value: &T,
) -> Result<()> {
    let bytes = serde_json::to_vec(value)?;
    ensure!(bytes.len() <= MAX_FRAME, "pairing frame too large");
    writer.write_u32_le(u32::try_from(bytes.len())?).await?;
    writer.write_all(&bytes).await?;
    writer.flush().await?;
    Ok(())
}

/// Read one frame of at most [`MAX_FRAME`] bytes. Decoder errors carry no input.
pub async fn read_frame<T: for<'de> Deserialize<'de>>(
    reader: &mut (impl AsyncRead + Unpin),
) -> Result<T> {
    let length = usize::try_from(reader.read_u32_le().await?)?;
    ensure!(length <= MAX_FRAME, "pairing frame too large");
    let mut bytes = vec![0; length];
    reader.read_exact(&mut bytes).await?;
    serde_json::from_slice(&bytes).map_err(|_| anyhow::anyhow!("malformed pairing frame"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(n: u8) -> iroh::PublicKey {
        iroh::SecretKey::from_bytes(&[n; 32]).public()
    }

    #[test]
    fn transcript_binds_every_input() {
        let secret = Secret([9; 32]);
        let base = transcript(&key(1), &key(2), &secret, Role::Full);
        assert_ne!(base, transcript(&key(3), &key(2), &secret, Role::Full));
        assert_ne!(base, transcript(&key(1), &key(3), &secret, Role::Full));
        assert_ne!(
            base,
            transcript(&key(1), &key(2), &Secret([8; 32]), Role::Full)
        );
        assert_ne!(base, transcript(&key(1), &key(2), &secret, Role::ViewOnly));
        let phrase = phrase(&secret, &base);
        assert_eq!(phrase.len(), 19);
        assert!(phrase.split('-').all(|group| {
            group.len() == 4
                && group
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        }));
        assert_ne!(
            phrase,
            super::phrase(&secret, &transcript(&key(1), &key(4), &secret, Role::Full))
        );
        let proof = proof(&secret, &base);
        assert!(proof_matches(&hex(&proof), &proof));
        assert!(!proof_matches(
            &hex(&super::proof(&Secret([8; 32]), &base)),
            &proof
        ));
        assert!(!proof_matches("", &proof));
    }

    #[test]
    fn names_are_sanitized() {
        assert_eq!(sanitize_name("my laptop\u{1b}[31m"), "mylaptop31m");
        assert_eq!(sanitize_name("--"), "device");
        assert_eq!(sanitize_name(&"a".repeat(200)).len(), MAX_DEVICE_NAME);
    }

    #[tokio::test]
    async fn frames_are_bounded() {
        let (mut a, mut b) = tokio::io::duplex(16_384);
        write_frame(&mut a, &HostMessage::Pending { phrase: "x".into() })
            .await
            .unwrap();
        assert_eq!(
            read_frame::<HostMessage>(&mut b).await.unwrap(),
            HostMessage::Pending { phrase: "x".into() }
        );
        a.write_u32_le(u32::try_from(MAX_FRAME + 1).unwrap())
            .await
            .unwrap();
        assert!(read_frame::<HostMessage>(&mut b).await.is_err());
        let (mut a, mut b) = tokio::io::duplex(16_384);
        a.write_u32_le(3).await.unwrap();
        a.write_all(b"{x}").await.unwrap();
        assert!(read_frame::<DeviceMessage>(&mut b).await.is_err());
    }
}
