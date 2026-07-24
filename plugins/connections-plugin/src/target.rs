use bmux_config::{
    BmuxConfig, ConfigPaths, ConnectionTargetConfig, ConnectionTransport, RemoteServerStartMode,
};
use bmux_connections_plugin_api::connection_types::{
    ConnectionError, ConnectionTransport as ApiTransport, ResolvedEndpoint,
};
use bmux_plugin_sdk::HostConnectionInfo;
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolvedTarget {
    Local { reference: String },
    Ssh(SshTarget),
    Tls(TlsTarget),
    Iroh(IrohTarget),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SshTarget {
    pub reference: String,
    pub label: String,
    pub host: String,
    pub user: Option<String>,
    pub port: Option<u16>,
    pub identity_file: Option<PathBuf>,
    pub known_hosts_file: Option<PathBuf>,
    pub strict_host_key_checking: bool,
    pub jump: Option<String>,
    pub remote_bmux_path: String,
    pub connect_timeout_ms: u64,
    pub server_start_mode: RemoteServerStartMode,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TlsTarget {
    pub reference: String,
    pub label: String,
    pub host: String,
    pub port: u16,
    pub server_name: String,
    pub ca_file: Option<PathBuf>,
    pub connect_timeout_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompressionMode {
    Auto,
    None,
    Zstd,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IrohTarget {
    pub reference: String,
    pub label: String,
    pub endpoint_id: String,
    pub relay_url: Option<String>,
    pub ip_addr: Option<std::net::SocketAddr>,
    pub require_ssh_auth: bool,
    pub compression: CompressionMode,
    pub connect_timeout_ms: u64,
}

#[derive(serde::Deserialize)]
struct AuthState {
    access_token: String,
}

pub fn context_paths(connection: &HostConnectionInfo) -> ConfigPaths {
    ConfigPaths::new(
        PathBuf::from(&connection.config_dir),
        PathBuf::from(&connection.runtime_dir),
        PathBuf::from(&connection.data_dir),
        PathBuf::from(&connection.state_dir),
    )
}

pub fn load_config(connection: &HostConnectionInfo) -> anyhow::Result<BmuxConfig> {
    let path = connection
        .probe_config_file("bmux.toml")
        .unwrap_or_else(|| PathBuf::from(&connection.config_dir).join("bmux.toml"));
    Ok(BmuxConfig::load_from_path(&path)?)
}

pub async fn expand_reference(
    config: &BmuxConfig,
    paths: &ConfigPaths,
    target: &str,
) -> Result<String, ConnectionError> {
    let Some(name) = target.trim().strip_prefix("bmux://") else {
        return Ok(target.trim().to_string());
    };
    if let Some(mapped) = config.connections.share_links.get(name) {
        return Ok(mapped.clone());
    }
    let auth_path = paths.runtime_dir.join("auth-state.json");
    let auth = match std::fs::read_to_string(&auth_path) {
        Ok(content) => serde_json::from_str::<AuthState>(&content).map_err(|error| {
            ConnectionError::AuthenticationFailed {
                target: target.to_string(),
                reason: format!("invalid auth state {}: {error}", auth_path.display()),
            }
        })?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(ConnectionError::TargetNotFound {
                target: target.to_string(),
            });
        }
        Err(error) => {
            return Err(ConnectionError::AuthenticationFailed {
                target: target.to_string(),
                reason: format!("reading auth state {}: {error}", auth_path.display()),
            });
        }
    };
    let base = std::env::var("BMUX_CONTROL_PLANE_URL")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| config.connections.control_plane_url.clone())
        .unwrap_or_else(|| "https://api.bmux.run".to_string());
    let response = reqwest::Client::new()
        .get(format!("{base}/v1/share-links/{name}"))
        .bearer_auth(auth.access_token)
        .send()
        .await
        .map_err(|error| ConnectionError::ConnectionFailed {
            target: target.to_string(),
            reason: format!("control-plane lookup failed: {error}"),
        })?;
    if !response.status().is_success() {
        return Err(ConnectionError::TargetNotFound {
            target: target.to_string(),
        });
    }
    let payload = response
        .json::<serde_json::Value>()
        .await
        .map_err(|error| ConnectionError::InvalidTarget {
            target: target.to_string(),
            reason: format!("invalid control-plane response: {error}"),
        })?;
    payload
        .get("target")
        .and_then(serde_json::Value::as_str)
        .map(ToString::to_string)
        .ok_or_else(|| ConnectionError::InvalidTarget {
            target: target.to_string(),
            reason: "control-plane response omitted target".to_string(),
        })
}

pub fn list(config: &BmuxConfig) -> Vec<ResolvedEndpoint> {
    let mut endpoints = vec![
        ResolvedTarget::Local {
            reference: "local".to_string(),
        }
        .public(),
    ];
    endpoints.extend(
        config
            .connections
            .targets
            .keys()
            .filter_map(|name| resolve_internal(config, name).ok())
            .map(|target| target.public()),
    );
    endpoints
}

pub fn resolve(config: &BmuxConfig, target: &str) -> Result<ResolvedEndpoint, ConnectionError> {
    resolve_internal(config, target).map(|target| target.public())
}

pub fn resolve_internal(
    config: &BmuxConfig,
    target: &str,
) -> Result<ResolvedTarget, ConnectionError> {
    let target = target.trim();
    if target.is_empty() || target == "local" {
        return Ok(ResolvedTarget::Local {
            reference: "local".to_string(),
        });
    }
    if let Some(name) = target.strip_prefix("bmux://") {
        let mapped = config.connections.share_links.get(name).ok_or_else(|| {
            ConnectionError::TargetNotFound {
                target: target.to_string(),
            }
        })?;
        return resolve_internal(config, mapped);
    }
    if let Some(named) = config.connections.targets.get(target) {
        return resolve_named(target, named);
    }
    if target.starts_with("tls://") || target.starts_with("https://") {
        return parse_tls(target);
    }
    if target.starts_with("iroh://") {
        return parse_iroh(target);
    }
    parse_ssh(target)
}

fn resolve_named(
    name: &str,
    target: &ConnectionTargetConfig,
) -> Result<ResolvedTarget, ConnectionError> {
    match target.transport {
        ConnectionTransport::Local => Ok(ResolvedTarget::Local {
            reference: name.to_string(),
        }),
        ConnectionTransport::Ssh => {
            let host = required(name, "host", target.host.as_deref())?;
            Ok(ResolvedTarget::Ssh(SshTarget {
                reference: name.to_string(),
                label: name.to_string(),
                host,
                user: target.user.clone(),
                port: target.port,
                identity_file: target.identity_file.clone(),
                known_hosts_file: target.known_hosts_file.clone(),
                strict_host_key_checking: target.strict_host_key_checking,
                jump: target.jump.clone(),
                remote_bmux_path: target.remote_bmux_path.clone(),
                connect_timeout_ms: target.connect_timeout_ms.max(1),
                server_start_mode: target.server_start_mode,
            }))
        }
        ConnectionTransport::Tls => {
            let host = required(name, "host", target.host.as_deref())?;
            Ok(ResolvedTarget::Tls(TlsTarget {
                reference: name.to_string(),
                label: name.to_string(),
                port: target.port.unwrap_or(443),
                server_name: target.server_name.clone().unwrap_or_else(|| host.clone()),
                host,
                ca_file: target.ca_file.clone(),
                connect_timeout_ms: target.connect_timeout_ms.max(1),
            }))
        }
        ConnectionTransport::Iroh => Ok(ResolvedTarget::Iroh(IrohTarget {
            reference: name.to_string(),
            label: name.to_string(),
            endpoint_id: required(name, "endpoint_id", target.endpoint_id.as_deref())?,
            relay_url: target.relay_url.clone(),
            ip_addr: target.iroh_ip_addr,
            require_ssh_auth: target.iroh_ssh_auth,
            compression: CompressionMode::Auto,
            connect_timeout_ms: target.connect_timeout_ms.max(1),
        })),
    }
}

fn required(target: &str, field: &str, value: Option<&str>) -> Result<String, ConnectionError> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
        .ok_or_else(|| ConnectionError::InvalidTarget {
            target: target.to_string(),
            reason: format!("configured target requires {field}"),
        })
}

fn parse_ssh(reference: &str) -> Result<ResolvedTarget, ConnectionError> {
    let raw = reference.strip_prefix("ssh://").unwrap_or(reference);
    let (user, host_port) = raw
        .split_once('@')
        .map_or((None, raw), |(user, rest)| (Some(user.to_string()), rest));
    let (host, port) = parse_host_port(host_port, 22, reference)?;
    Ok(ResolvedTarget::Ssh(SshTarget {
        reference: reference.to_string(),
        label: reference.to_string(),
        host,
        user,
        port: Some(port),
        identity_file: None,
        known_hosts_file: None,
        strict_host_key_checking: true,
        jump: None,
        remote_bmux_path: "bmux".to_string(),
        connect_timeout_ms: 8_000,
        server_start_mode: RemoteServerStartMode::Auto,
    }))
}

fn parse_tls(reference: &str) -> Result<ResolvedTarget, ConnectionError> {
    let raw = reference
        .strip_prefix("tls://")
        .or_else(|| reference.strip_prefix("https://"))
        .ok_or_else(|| invalid(reference, "TLS target requires tls:// or https://"))?;
    let authority = raw.split('/').next().unwrap_or_default();
    let (host, port) = parse_host_port(authority, 443, reference)?;
    Ok(ResolvedTarget::Tls(TlsTarget {
        reference: reference.to_string(),
        label: reference.to_string(),
        server_name: host.clone(),
        host,
        port,
        ca_file: None,
        connect_timeout_ms: 8_000,
    }))
}

fn parse_iroh(reference: &str) -> Result<ResolvedTarget, ConnectionError> {
    let raw = reference
        .strip_prefix("iroh://")
        .ok_or_else(|| invalid(reference, "iroh target requires iroh://"))?;
    let (endpoint_id, query) = raw
        .split_once('?')
        .map_or((raw, None), |(id, q)| (id, Some(q)));
    if endpoint_id.trim().is_empty() {
        return Err(invalid(reference, "iroh target requires endpoint id"));
    }
    let mut relay_url = None;
    let mut ip_addr = None;
    let mut require_ssh_auth = false;
    let mut compression = CompressionMode::Auto;
    if let Some(query) = query {
        for part in query.split('&').filter(|part| !part.is_empty()) {
            let (key, value) = part.split_once('=').unwrap_or((part, ""));
            match key {
                "relay" if !value.is_empty() => relay_url = Some(value.to_string()),
                "addr" if !value.is_empty() => {
                    ip_addr = Some(
                        value
                            .parse()
                            .map_err(|_| invalid(reference, "addr must be a socket address"))?,
                    );
                }
                "auth" if value.eq_ignore_ascii_case("ssh") => require_ssh_auth = true,
                "compression" if value.eq_ignore_ascii_case("none") => {
                    compression = CompressionMode::None;
                }
                "compression" if value.eq_ignore_ascii_case("zstd") => {
                    compression = CompressionMode::Zstd;
                }
                "compression" if value.eq_ignore_ascii_case("auto") => {}
                "compression" => {
                    return Err(invalid(
                        reference,
                        "compression must be auto, none, or zstd",
                    ));
                }
                _ => {}
            }
        }
    }
    Ok(ResolvedTarget::Iroh(IrohTarget {
        reference: reference.to_string(),
        label: reference.to_string(),
        endpoint_id: endpoint_id.to_string(),
        relay_url,
        ip_addr,
        require_ssh_auth,
        compression,
        connect_timeout_ms: 8_000,
    }))
}

fn parse_host_port(
    raw: &str,
    default_port: u16,
    reference: &str,
) -> Result<(String, u16), ConnectionError> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err(invalid(reference, "target requires host"));
    }
    if raw.starts_with('[') {
        let end = raw
            .find(']')
            .ok_or_else(|| invalid(reference, "invalid bracketed host"))?;
        let host = raw[1..end].to_string();
        let rest = &raw[end + 1..];
        let port = if rest.is_empty() {
            default_port
        } else {
            rest.strip_prefix(':')
                .ok_or_else(|| invalid(reference, "invalid host suffix"))?
                .parse()
                .map_err(|_| invalid(reference, "invalid port"))?
        };
        return Ok((host, port));
    }
    if let Some((host, port)) = raw.rsplit_once(':')
        && !host.contains(':')
        && !port.is_empty()
    {
        let port = port
            .parse()
            .map_err(|_| invalid(reference, "invalid port"))?;
        return Ok((host.to_string(), port));
    }
    Ok((raw.to_string(), default_port))
}

fn invalid(target: &str, reason: &str) -> ConnectionError {
    ConnectionError::InvalidTarget {
        target: target.to_string(),
        reason: reason.to_string(),
    }
}

fn file_identity(path: Option<&std::path::Path>) -> String {
    let Some(path) = path else {
        return String::new();
    };
    match std::fs::read(path) {
        Ok(contents) => format!("{}:{:016x}", path.display(), stable_bytes_hash(&contents)),
        Err(error) => format!("{}:unreadable:{:?}", path.display(), error.kind()),
    }
}

fn stable_bytes_hash(bytes: &[u8]) -> u64 {
    // FNV-1a is sufficient for cache invalidation; this is not a security
    // decision or certificate verification primitive.
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

impl ResolvedTarget {
    pub fn reference(&self) -> &str {
        match self {
            Self::Local { reference } => reference,
            Self::Ssh(target) => &target.reference,
            Self::Tls(target) => &target.reference,
            Self::Iroh(target) => &target.reference,
        }
    }

    /// Private identity for connection reuse. Unlike the public reference,
    /// this includes resolved transport and security-relevant configuration so
    /// aliases or separate runtime roots cannot share the wrong connection.
    pub fn connection_identity(&self, config: &BmuxConfig, paths: &ConfigPaths) -> String {
        match self {
            Self::Local { .. } => format!("local:{}", paths.server_socket().display()),
            Self::Ssh(target) => format!(
                "ssh:{}@{}:{}|identity={}|known_hosts={}|strict={}|jump={}|path={}",
                target.user.as_deref().unwrap_or(""),
                target.host,
                target.port.unwrap_or(22),
                file_identity(target.identity_file.as_deref()),
                file_identity(target.known_hosts_file.as_deref()),
                target.strict_host_key_checking,
                target.jump.as_deref().unwrap_or(""),
                target.remote_bmux_path,
            ),
            Self::Tls(target) => format!(
                "tls:{}:{}|server_name={}|ca={}|trust_mode={:?}|declared_pin={}|local_pins={}",
                target.host,
                target.port,
                target.server_name,
                file_identity(target.ca_file.as_deref()),
                config.connections.tls_trust.mode,
                config
                    .connections
                    .tls_trust
                    .known_gateways
                    .get(&format!("{}:{}", target.host.trim(), target.port))
                    .map_or("", |entry| entry.fingerprint_sha256.as_str()),
                file_identity(Some(&paths.known_gateways_file())),
            ),
            Self::Iroh(target) => format!(
                "iroh:{}|relay={}|addr={}|ssh_auth={}|compression={:?}",
                target.endpoint_id,
                target.relay_url.as_deref().unwrap_or(""),
                target
                    .ip_addr
                    .map_or_else(String::new, |addr| addr.to_string()),
                target.require_ssh_auth,
                target.compression,
            ),
        }
    }

    fn public(&self) -> ResolvedEndpoint {
        match self {
            Self::Local { reference } => ResolvedEndpoint {
                endpoint_id: reference.clone(),
                reference: reference.clone(),
                label: "local".to_string(),
                transport: ApiTransport::Local,
                address: "local".to_string(),
                server_name: None,
                connect_timeout_ms: 0,
            },
            Self::Ssh(target) => ResolvedEndpoint {
                endpoint_id: target.reference.clone(),
                reference: target.reference.clone(),
                label: target.label.clone(),
                transport: ApiTransport::Ssh,
                address: format!("{}:{}", target.host, target.port.unwrap_or(22)),
                server_name: None,
                connect_timeout_ms: target.connect_timeout_ms,
            },
            Self::Tls(target) => ResolvedEndpoint {
                endpoint_id: target.reference.clone(),
                reference: target.reference.clone(),
                label: target.label.clone(),
                transport: ApiTransport::Tls,
                address: format!("{}:{}", target.host, target.port),
                server_name: Some(target.server_name.clone()),
                connect_timeout_ms: target.connect_timeout_ms,
            },
            Self::Iroh(target) => ResolvedEndpoint {
                endpoint_id: target.reference.clone(),
                reference: target.reference.clone(),
                label: target.label.clone(),
                transport: ApiTransport::Iroh,
                address: target.endpoint_id.clone(),
                server_name: None,
                connect_timeout_ms: target.connect_timeout_ms,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_all_inline_transports() {
        let config = BmuxConfig::default();
        assert!(matches!(
            resolve_internal(&config, "local"),
            Ok(ResolvedTarget::Local { .. })
        ));
        assert!(matches!(
            resolve_internal(&config, "ops@example.com:2222"),
            Ok(ResolvedTarget::Ssh(_))
        ));
        assert!(matches!(
            resolve_internal(&config, "tls://example.com:7443"),
            Ok(ResolvedTarget::Tls(_))
        ));
        let resolved = resolve_internal(
            &config,
            "iroh://endpoint?addr=127.0.0.1:7443&compression=none",
        )
        .expect("direct iroh target should resolve");
        let ResolvedTarget::Iroh(target) = resolved else {
            panic!("expected iroh target");
        };
        assert_eq!(
            target.ip_addr,
            Some("127.0.0.1:7443".parse().expect("socket address"))
        );
        assert_eq!(target.compression, CompressionMode::None);
    }

    #[test]
    fn resolves_named_direct_iroh_target() {
        let mut config = BmuxConfig::default();
        config.connections.targets.insert(
            "peer".to_string(),
            ConnectionTargetConfig {
                transport: ConnectionTransport::Iroh,
                endpoint_id: Some("endpoint".to_string()),
                iroh_ip_addr: Some("127.0.0.1:7443".parse().expect("socket address")),
                ..ConnectionTargetConfig::default()
            },
        );
        let ResolvedTarget::Iroh(target) = resolve_internal(&config, "peer").unwrap() else {
            panic!("expected iroh target");
        };
        assert_eq!(
            target.ip_addr,
            config.connections.targets["peer"].iroh_ip_addr
        );
        let resolved = ResolvedTarget::Iroh(target);
        assert!(
            resolved
                .connection_identity(&config, &ConfigPaths::default())
                .contains("addr=127.0.0.1:7443")
        );
    }

    #[test]
    fn rejects_invalid_direct_iroh_socket_address() {
        let error = resolve_internal(&BmuxConfig::default(), "iroh://endpoint?addr=invalid")
            .expect_err("invalid direct address should fail");
        assert!(matches!(error, ConnectionError::InvalidTarget { .. }));
    }

    #[test]
    fn resolves_named_target_and_share_link() {
        let mut config = BmuxConfig::default();
        config.connections.targets.insert(
            "prod".to_string(),
            ConnectionTargetConfig {
                transport: ConnectionTransport::Ssh,
                host: Some("prod.example.com".to_string()),
                ..ConnectionTargetConfig::default()
            },
        );
        config
            .connections
            .share_links
            .insert("main".to_string(), "prod".to_string());
        let endpoint = resolve(&config, "bmux://main").expect("share should resolve");
        assert_eq!(endpoint.reference, "prod");
        assert_eq!(endpoint.transport, ApiTransport::Ssh);
    }

    #[test]
    fn rejects_unknown_bmux_share() {
        let error = resolve(&BmuxConfig::default(), "bmux://missing").expect_err("missing share");
        assert!(matches!(error, ConnectionError::TargetNotFound { .. }));
    }
}
