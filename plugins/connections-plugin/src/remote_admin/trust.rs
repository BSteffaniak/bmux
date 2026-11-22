use anyhow::{Context, Result};
use bmux_config::{BmuxConfig, ConfigPaths};
use bmux_connections_admin::known_gateways::{
    KnownGatewayState, certificate_fingerprint_sha256, fingerprints_equal,
    load_known_gateways_store, save_known_gateways_store, tls_gateway_key,
};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, Error as RustlsError, SignatureScheme};
use std::{
    collections::BTreeSet,
    io::{self, Write},
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
#[derive(Debug, Clone)]
struct TlsTarget {
    label: String,
    host: String,
    port: u16,
    server_name: String,
    connect_timeout_ms: u64,
}

fn parse_inline_tls_target(target: &str) -> Result<TlsTarget> {
    let raw = target
        .trim()
        .strip_prefix("tls://")
        .ok_or_else(|| anyhow::anyhow!("TLS target must start with tls://"))?;
    let (host, port) = if let Some((host, port_raw)) = raw.rsplit_once(':') {
        if port_raw.is_empty() {
            (raw.to_string(), 443)
        } else {
            let parsed = port_raw
                .parse::<u16>()
                .with_context(|| format!("invalid TLS port in target '{target}'"))?;
            (host.to_string(), parsed)
        }
    } else {
        (raw.to_string(), 443)
    };
    if host.trim().is_empty() {
        anyhow::bail!("TLS target must include a host");
    }
    Ok(TlsTarget {
        label: target.to_string(),
        host: host.clone(),
        port,
        server_name: host,
        connect_timeout_ms: 8_000,
    })
}
pub(super) async fn run_remote_trust_add(target: &str, yes: bool) -> Result<u8> {
    let target = if target.starts_with("tls://") {
        parse_inline_tls_target(target)?
    } else {
        parse_inline_tls_target(&format!("tls://{target}"))?
    };
    let fingerprint = probe_tls_gateway_fingerprint(&target).await?;
    let key = tls_gateway_key(&target.host, target.port);
    let config = BmuxConfig::load().unwrap_or_default();
    if let Some(pin) = config.connections.tls_trust.known_gateways.get(&key) {
        if fingerprints_equal(&pin.fingerprint_sha256, &fingerprint) {
            println!("TLS gateway {key} already trusted by declarative config");
            return Ok(0);
        }
        anyhow::bail!(
            "TLS gateway {key} conflicts with declarative config pin: expected {}, received {fingerprint}",
            pin.fingerprint_sha256
        );
    }
    if !yes {
        println!("The bmux TLS gateway at {key} presented:");
        println!("  {fingerprint}\n");
        print!("Trust this gateway and remember it? [y/N] ");
        io::stdout().flush()?;
        let mut answer = String::new();
        io::stdin().read_line(&mut answer)?;
        if !matches!(answer.trim(), "y" | "Y" | "yes" | "YES" | "Yes") {
            anyhow::bail!("TLS gateway trust declined for {key}");
        }
    }
    trust_tls_gateway(&target, &fingerprint, None)?;
    println!("trusted TLS gateway {key}: {fingerprint}");
    Ok(0)
}
pub(super) fn run_remote_trust_list() -> Result<u8> {
    let config = BmuxConfig::load()?;
    let store = load_known_gateways_store(&ConfigPaths::default())?;
    let mut keys = BTreeSet::new();
    keys.extend(config.connections.tls_trust.known_gateways.keys().cloned());
    keys.extend(store.gateways.keys().cloned());
    if keys.is_empty() {
        println!("no TLS gateway trust pins configured");
        return Ok(0);
    }
    for key in keys {
        if let Some(pin) = config.connections.tls_trust.known_gateways.get(&key) {
            println!(
                "{key}\t{}\tsource=config\tlabel={}",
                pin.fingerprint_sha256,
                pin.label.as_deref().unwrap_or("-")
            );
        }
        if let Some(pin) = store.gateways.get(&key)
            && !config
                .connections
                .tls_trust
                .known_gateways
                .contains_key(&key)
        {
            println!(
                "{key}\t{}\tsource=local\tlabel={}",
                pin.fingerprint_sha256,
                pin.label.as_deref().unwrap_or("-")
            );
        }
    }
    Ok(0)
}

pub(super) fn run_remote_trust_remove(endpoint: &str) -> Result<u8> {
    let config = BmuxConfig::load().unwrap_or_default();
    if config
        .connections
        .tls_trust
        .known_gateways
        .contains_key(endpoint)
    {
        anyhow::bail!(
            "cannot remove declarative TLS trust pin for {endpoint}; edit bmux config instead"
        );
    }
    let paths = ConfigPaths::default();
    let mut store = load_known_gateways_store(&paths)?;
    if store.gateways.remove(endpoint).is_some() {
        save_known_gateways_store(&paths, &store)?;
        println!("removed local TLS trust pin for {endpoint}");
        return Ok(0);
    }
    anyhow::bail!("local TLS trust pin not found for {endpoint}")
}

fn trust_tls_gateway(target: &TlsTarget, fingerprint: &str, label: Option<&str>) -> Result<()> {
    let paths = ConfigPaths::default();
    let mut store = load_known_gateways_store(&paths)?;
    let key = tls_gateway_key(&target.host, target.port);
    store.gateways.insert(
        key,
        KnownGatewayState {
            fingerprint_sha256: fingerprint.to_string(),
            trusted_at_unix_ms: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis()
                .try_into()
                .unwrap_or(u64::MAX),
            server_name: Some(target.server_name.clone()),
            label: label
                .map(str::to_string)
                .or_else(|| Some(target.label.clone())),
        },
    );
    save_known_gateways_store(&paths, &store)
}

async fn probe_tls_gateway_fingerprint(target: &TlsTarget) -> Result<String> {
    let verifier = Arc::new(ProbeTlsServerVerifier::default());
    let connector = TlsConnector::from(Arc::new(
        rustls::ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .context("configuring TLS protocol versions")?
        .dangerous()
        .with_custom_certificate_verifier(verifier.clone())
        .with_no_client_auth(),
    ));
    let address = format!("{}:{}", target.host, target.port);
    let tcp_stream = tokio::time::timeout(
        Duration::from_millis(target.connect_timeout_ms.max(1)),
        TcpStream::connect(&address),
    )
    .await
    .with_context(|| format!("timed out probing TLS target '{}'", target.label))?
    .with_context(|| format!("failed probing TLS target '{}'", target.label))?;
    let server_name = ServerName::try_from(target.server_name.clone())
        .map_err(|_| anyhow::anyhow!("invalid TLS server name '{}'", target.server_name))?;
    let _ = connector
        .connect(server_name, tcp_stream)
        .await
        .with_context(|| {
            format!(
                "failed probing TLS certificate for target '{}'",
                target.label
            )
        })?;
    verifier
        .fingerprint
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
        .ok_or_else(|| {
            anyhow::anyhow!(
                "TLS gateway '{}' did not present a certificate",
                target.label
            )
        })
}
#[derive(Debug, Default)]
struct ProbeTlsServerVerifier {
    fingerprint: Mutex<Option<String>>,
}

impl ServerCertVerifier for ProbeTlsServerVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> std::result::Result<ServerCertVerified, RustlsError> {
        *self
            .fingerprint
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            Some(certificate_fingerprint_sha256(end_entity.as_ref()));
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, RustlsError> {
        let algorithms = rustls::crypto::ring::default_provider().signature_verification_algorithms;
        rustls::crypto::verify_tls12_signature(message, cert, dss, &algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, RustlsError> {
        let algorithms = rustls::crypto::ring::default_provider().signature_verification_algorithms;
        rustls::crypto::verify_tls13_signature(message, cert, dss, &algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

pub(super) fn validate_tls_target(value: &str) -> Result<()> {
    parse_inline_tls_target(value).map(drop)
}
