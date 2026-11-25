//! The compact pairing link: `bmux-pair:` followed by lowercase base32 of
//! `version | host key | secret | relay hint | direct addresses | checksum`.
use anyhow::{Result, bail, ensure};
use sha2::{Digest, Sha256};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

pub const PREFIX: &str = "bmux-pair:";
const VERSION: u8 = 1;
const MAX_ADDRS: usize = 8;
const MAX_LINK: usize = 1_024;

/// The 256-bit single-use pairing secret. Never printed by `Debug`.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(pub [u8; 32]);

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret(..)")
    }
}

impl Secret {
    pub fn generate() -> Result<Self> {
        let mut bytes = [0; 32];
        getrandom::fill(&mut bytes).map_err(|_| anyhow::anyhow!("no secure randomness"))?;
        Ok(Self(bytes))
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct PairingLink {
    pub host_key: iroh::PublicKey,
    pub secret: Secret,
    pub relay: Option<String>,
    pub addrs: Vec<SocketAddr>,
}

impl std::fmt::Debug for PairingLink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PairingLink")
            .field("host_key", &self.host_key)
            .field("relay", &self.relay)
            .field("addrs", &self.addrs)
            .finish_non_exhaustive()
    }
}

const ALPHABET: &[u8; 32] = b"abcdefghijklmnopqrstuvwxyz234567";

fn base32_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 8 / 5 + 1);
    let mut buffer = 0_u32;
    let mut bits = 0;
    for &byte in bytes {
        buffer = (buffer << 8) | u32::from(byte);
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(char::from(ALPHABET[((buffer >> bits) & 31) as usize]));
        }
    }
    if bits > 0 {
        out.push(char::from(ALPHABET[((buffer << (5 - bits)) & 31) as usize]));
    }
    out
}

fn base32_decode(text: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(text.len() * 5 / 8);
    let mut buffer = 0_u32;
    let mut bits = 0;
    for byte in text.bytes() {
        let value = ALPHABET
            .iter()
            .position(|&c| c == byte.to_ascii_lowercase())?;
        buffer = (buffer << 5) | u32::try_from(value).ok()?;
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            out.push(u8::try_from((buffer >> bits) & 0xff).ok()?);
        }
    }
    // Leftover bits must be zero padding.
    (buffer & ((1 << bits) - 1) == 0).then_some(out)
}

impl PairingLink {
    pub fn encode(&self) -> Result<String> {
        let mut bytes = vec![VERSION];
        bytes.extend_from_slice(self.host_key.as_bytes());
        bytes.extend_from_slice(&self.secret.0);
        let relay = self.relay.as_deref().unwrap_or("");
        bytes.push(u8::try_from(relay.len()).map_err(|_| anyhow::anyhow!("relay URL too long"))?);
        bytes.extend_from_slice(relay.as_bytes());
        let addrs = &self.addrs[..self.addrs.len().min(MAX_ADDRS)];
        bytes.push(u8::try_from(addrs.len())?);
        for addr in addrs {
            match addr.ip() {
                IpAddr::V4(ip) => {
                    bytes.push(4);
                    bytes.extend_from_slice(&ip.octets());
                }
                IpAddr::V6(ip) => {
                    bytes.push(6);
                    bytes.extend_from_slice(&ip.octets());
                }
            }
            bytes.extend_from_slice(&addr.port().to_be_bytes());
        }
        let checksum = Sha256::digest(&bytes);
        bytes.extend_from_slice(&checksum[..4]);
        Ok(format!("{PREFIX}{}", base32_encode(&bytes)))
    }

    /// Parse a pasted link. Errors never echo the input.
    pub fn decode(text: &str) -> Result<Self> {
        let text = text.trim();
        ensure!(text.len() <= MAX_LINK, "pairing link is too long");
        let Some(body) = text.strip_prefix(PREFIX) else {
            bail!("not a bmux pairing link (it should start with {PREFIX})");
        };
        let bytes =
            base32_decode(body).ok_or_else(|| anyhow::anyhow!("pairing link is damaged"))?;
        ensure!(bytes.len() > 4, "pairing link is damaged");
        let (payload, checksum) = bytes.split_at(bytes.len() - 4);
        ensure!(
            Sha256::digest(payload)[..4] == *checksum,
            "pairing link is damaged (checksum mismatch); copy it again"
        );
        let mut reader = Reader(payload);
        ensure!(
            reader.byte()? == VERSION,
            "unsupported pairing link version"
        );
        let host_key = iroh::PublicKey::from_bytes(&reader.array::<32>()?)
            .map_err(|_| anyhow::anyhow!("pairing link has an invalid host key"))?;
        let secret = Secret(reader.array::<32>()?);
        let relay_len = usize::from(reader.byte()?);
        let relay = if relay_len == 0 {
            None
        } else {
            let relay = std::str::from_utf8(reader.take(relay_len)?)
                .map_err(|_| anyhow::anyhow!("pairing link is damaged"))?;
            relay
                .parse::<iroh::RelayUrl>()
                .map_err(|_| anyhow::anyhow!("pairing link has an invalid relay"))?;
            Some(relay.to_owned())
        };
        let count = usize::from(reader.byte()?);
        ensure!(count <= MAX_ADDRS, "pairing link is damaged");
        let mut addrs = Vec::with_capacity(count);
        for _ in 0..count {
            let ip = match reader.byte()? {
                4 => IpAddr::V4(Ipv4Addr::from(reader.array::<4>()?)),
                6 => IpAddr::V6(Ipv6Addr::from(reader.array::<16>()?)),
                _ => bail!("pairing link is damaged"),
            };
            let port = u16::from_be_bytes(reader.array::<2>()?);
            addrs.push(SocketAddr::new(ip, port));
        }
        ensure!(reader.0.is_empty(), "pairing link is damaged");
        Ok(Self {
            host_key,
            secret,
            relay,
            addrs,
        })
    }
}

struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        ensure!(self.0.len() >= n, "pairing link is damaged");
        let (head, tail) = self.0.split_at(n);
        self.0 = tail;
        Ok(head)
    }
    fn byte(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }
    fn array<const N: usize>(&mut self) -> Result<[u8; N]> {
        Ok(self.take(N)?.try_into()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn link() -> PairingLink {
        PairingLink {
            host_key: iroh::SecretKey::from_bytes(&[3; 32]).public(),
            secret: Secret([7; 32]),
            relay: Some("https://relay.example/".into()),
            addrs: vec![
                "127.0.0.1:4433".parse().unwrap(),
                "[::1]:9".parse().unwrap(),
            ],
        }
    }

    #[test]
    fn round_trips_and_detects_damage() {
        let text = link().encode().unwrap();
        assert!(text.starts_with(PREFIX));
        assert!(text.len() < 220, "{}", text.len());
        assert_eq!(PairingLink::decode(&text).unwrap(), link());
        assert_eq!(
            PairingLink::decode(&format!(
                "  {}\n",
                text.to_uppercase().replace("BMUX-PAIR:", PREFIX)
            ))
            .unwrap(),
            link()
        );
        let mut damaged = text.into_bytes();
        let last = damaged.len() - 3;
        damaged[last] = if damaged[last] == b'a' { b'b' } else { b'a' };
        assert!(PairingLink::decode(std::str::from_utf8(&damaged).unwrap()).is_err());
        assert!(PairingLink::decode("bmux-pair:").is_err());
        assert!(PairingLink::decode("https://x").is_err());
        let minimal = PairingLink {
            relay: None,
            addrs: vec![],
            ..link()
        };
        assert_eq!(
            PairingLink::decode(&minimal.encode().unwrap()).unwrap(),
            minimal
        );
    }

    #[test]
    fn errors_and_debug_never_show_the_secret() {
        let text = link().encode().unwrap();
        let body = &text[PREFIX.len()..];
        let error =
            PairingLink::decode(&format!("{PREFIX}{}x", &body[..body.len() - 1])).unwrap_err();
        assert!(!error.to_string().contains(body));
        assert!(!format!("{:?}", link()).contains("7, 7"));
    }
}
