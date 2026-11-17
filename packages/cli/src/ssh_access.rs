use anyhow::Result;
use bmux_config::{BmuxConfig, CompressionMode};
#[cfg(feature = "remote-iroh")]
mod auth;
#[cfg(feature = "remote-iroh")]
pub use auth::{authenticate_client_connection, authenticate_host_connection};

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

#[cfg(not(feature = "remote-iroh"))]
pub fn iroh_unavailable() -> anyhow::Error {
    anyhow::anyhow!(
        "Iroh transport is not enabled in this build; rebuild with the `remote-iroh` feature"
    )
}
