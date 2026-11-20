//! The known-gateways trust store: TLS gateway certificate pins recorded on
//! first trust, shared by the CLI's remote attach and `remote trust` commands.
use anyhow::{Context, Result};
use bmux_config::ConfigPaths;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fmt::Write as _;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct KnownGatewaysStore {
    pub gateways: BTreeMap<String, KnownGatewayState>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KnownGatewayState {
    pub fingerprint_sha256: String,
    pub trusted_at_unix_ms: u64,
    pub server_name: Option<String>,
    pub label: Option<String>,
}

/// The store key for a gateway endpoint.
#[must_use]
pub fn tls_gateway_key(host: &str, port: u16) -> String {
    format!("{}:{port}", host.trim())
}

/// `SHA256:<hex>` fingerprint of a DER certificate.
///
/// # Panics
/// Never: formatting into a `String` cannot fail.
#[must_use]
pub fn certificate_fingerprint_sha256(cert: &[u8]) -> String {
    let digest = Sha256::digest(cert);
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        write!(&mut hex, "{byte:02x}").expect("writing to String cannot fail");
    }
    format!("SHA256:{hex}")
}

/// Load the store, or an empty one when none has been written.
///
/// # Errors
/// Returns read or parse failures.
pub fn load_known_gateways_store(paths: &ConfigPaths) -> Result<KnownGatewaysStore> {
    let path = paths.known_gateways_file();
    if !path.exists() {
        return Ok(KnownGatewaysStore::default());
    }
    let contents = std::fs::read_to_string(&path)
        .with_context(|| format!("failed reading known gateways store {}", path.display()))?;
    toml::from_str(&contents)
        .with_context(|| format!("failed parsing known gateways store {}", path.display()))
}

/// Persist the store, creating its directory.
///
/// # Errors
/// Returns directory, serialization or write failures.
pub fn save_known_gateways_store(paths: &ConfigPaths, store: &KnownGatewaysStore) -> Result<()> {
    let path = paths.known_gateways_file();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| {
            format!(
                "failed creating known gateways directory {}",
                parent.display()
            )
        })?;
    }
    let contents = toml::to_string_pretty(store).context("failed serializing known gateways")?;
    std::fs::write(&path, contents)
        .with_context(|| format!("failed writing known gateways store {}", path.display()))?;
    Ok(())
}

/// Compare fingerprints, ignoring surrounding whitespace and hex case.
#[must_use]
pub fn fingerprints_equal(left: &str, right: &str) -> bool {
    left.trim().eq_ignore_ascii_case(right.trim())
}
