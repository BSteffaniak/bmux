use crate::target::{CompressionMode, IrohTarget, ResolvedTarget, SshTarget, TlsTarget};
use anyhow::{Context, Result};
use bmux_client::BmuxClient;
use bmux_config::{BmuxConfig, ConfigPaths, RemoteServerStartMode, TlsTrustMode};
use bmux_connections_plugin_api::connection_types::ConnectionError;
use bmux_ipc::compressed_stream::CompressedStream;
use bmux_ipc::transport::ErasedIpcStream;
use git_sshripped_ssh_agent::{
    ChallengeProof, DEFAULT_SSHSIG_NAMESPACE, sign_challenge_with_any_agent_key,
};
use iroh::{Endpoint, EndpointAddr, EndpointId, endpoint::presets};
use rustls::RootCertStore;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, Error as RustlsError, SignatureScheme};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fmt::Write as _;
use std::pin::Pin;
use std::process::Stdio;
use std::sync::Arc;
use std::task::{Context as TaskContext, Poll};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader, ReadBuf};
use tokio::net::TcpStream;
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio_rustls::TlsConnector;
use uuid::Uuid;

const BMUX_IROH_ALPN: &[u8] = b"bmux/gateway/iroh/1";
const AUTH_PROTOCOL_VERSION: u8 = 1;

pub async fn connect(
    config: &BmuxConfig,
    paths: &ConfigPaths,
    target: &ResolvedTarget,
) -> Result<BmuxClient, ConnectionError> {
    match target {
        ResolvedTarget::Local { .. } => {
            BmuxClient::connect_with_paths(paths, "bmux-connections-local")
                .await
                .map_err(|error| connection_error(target, error))
        }
        ResolvedTarget::Ssh(target) => connect_ssh(paths, target).await,
        ResolvedTarget::Tls(target) => connect_tls(config, paths, target).await,
        ResolvedTarget::Iroh(target) => connect_iroh(config, paths, target).await,
    }
}

struct SshBridgeStream {
    _child: Child,
    stdin: ChildStdin,
    stdout: ChildStdout,
}

impl AsyncRead for SshBridgeStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().stdout).poll_read(cx, buffer)
    }
}

impl AsyncWrite for SshBridgeStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buffer: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.get_mut().stdin).poll_write(cx, buffer)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().stdin).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().stdin).poll_shutdown(cx)
    }
}

async fn connect_ssh(
    paths: &ConfigPaths,
    target: &SshTarget,
) -> Result<BmuxClient, ConnectionError> {
    ensure_ssh_server_ready(target)?;
    let mut command = ssh_command(target);
    command.arg(&target.remote_bmux_path);
    command.args(["server", "bridge", "--stdio"]);
    command.stdin(Stdio::piped());
    command.stdout(Stdio::piped());
    command.stderr(Stdio::inherit());
    let mut child = command
        .spawn()
        .map_err(|error| connection_error(target, error))?;
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| connection_failure(target, "SSH bridge stdin unavailable"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| connection_failure(target, "SSH bridge stdout unavailable"))?;
    let stream = SshBridgeStream {
        _child: child,
        stdin,
        stdout,
    };
    BmuxClient::connect_with_bridge_stream(
        ErasedIpcStream::new(Box::new(stream)),
        Duration::from_millis(target.connect_timeout_ms),
        "bmux-connections-ssh".to_string(),
        load_or_create_principal_id(paths).map_err(|error| connection_error(target, error))?,
    )
    .await
    .map_err(|error| connection_error(target, error))
}

fn ensure_ssh_server_ready(target: &SshTarget) -> Result<(), ConnectionError> {
    let status = ssh_bmux_status(target, &["server", "status"])?;
    if status.success() {
        return Ok(());
    }
    if target.server_start_mode == RemoteServerStartMode::RequireRunning {
        return Err(connection_failure(
            target,
            "remote bmux server is not running and server_start_mode=require_running",
        ));
    }
    let status = ssh_bmux_status(target, &["server", "start", "--daemon"])?;
    if !status.success() {
        return Err(connection_failure(
            target,
            "remote bmux server failed to start automatically",
        ));
    }
    let status = ssh_bmux_status(target, &["server", "status"])?;
    if status.success() {
        Ok(())
    } else {
        Err(connection_failure(
            target,
            "remote bmux server did not become ready after automatic start",
        ))
    }
}

fn ssh_bmux_status(
    target: &SshTarget,
    arguments: &[&str],
) -> Result<std::process::ExitStatus, ConnectionError> {
    let mut command = ssh_command(target);
    command.arg(&target.remote_bmux_path).args(arguments);
    command.stdin(Stdio::null());
    command.stdout(Stdio::null());
    command.stderr(Stdio::null());
    command
        .as_std_mut()
        .status()
        .map_err(|error| connection_error(target, error))
}

fn ssh_command(target: &SshTarget) -> Command {
    let mut command = Command::new("ssh");
    command.arg("-T");
    if let Some(port) = target.port {
        command.args(["-p", &port.to_string()]);
    }
    if let Some(path) = &target.identity_file {
        command.arg("-i").arg(path);
    }
    if let Some(jump) = &target.jump {
        command.args(["-J", jump]);
    }
    command.args([
        "-o",
        if target.strict_host_key_checking {
            "StrictHostKeyChecking=yes"
        } else {
            "StrictHostKeyChecking=no"
        },
    ]);
    if let Some(path) = &target.known_hosts_file {
        command
            .arg("-o")
            .arg(format!("UserKnownHostsFile={}", path.display()));
    }
    command.arg("-o").arg(format!(
        "ConnectTimeout={}",
        target.connect_timeout_ms.saturating_add(999) / 1_000
    ));
    command.args(["-o", "ServerAliveInterval=15"]);
    command.args(["-o", "ServerAliveCountMax=3"]);
    command.args(["-o", "BatchMode=yes"]);
    command.arg(target.user.as_ref().map_or_else(
        || target.host.clone(),
        |user| format!("{user}@{}", target.host),
    ));
    command
}

async fn connect_tls(
    config: &BmuxConfig,
    paths: &ConfigPaths,
    target: &TlsTarget,
) -> Result<BmuxClient, ConnectionError> {
    ensure_tls_trust(config, paths, target)
        .await
        .map_err(|error| trust_error(target, error))?;
    let connector =
        tls_connector(config, paths, target).map_err(|error| trust_error(target, error))?;
    let address = format!("{}:{}", target.host, target.port);
    let tcp = tokio::time::timeout(
        Duration::from_millis(target.connect_timeout_ms),
        TcpStream::connect(address),
    )
    .await
    .map_err(|error| connection_error(target, error))?
    .map_err(|error| connection_error(target, error))?;
    let server_name = ServerName::try_from(target.server_name.clone())
        .map_err(|_| trust_failure(target, "invalid TLS server name"))?;
    let stream = connector
        .connect(server_name, tcp)
        .await
        .map_err(|error| trust_error(target, error))?;
    BmuxClient::connect_with_bridge_stream(
        ErasedIpcStream::new(Box::new(stream)),
        Duration::from_millis(target.connect_timeout_ms),
        "bmux-connections-tls".to_string(),
        load_or_create_principal_id(paths).map_err(|error| connection_error(target, error))?,
    )
    .await
    .map_err(|error| connection_error(target, error))
}

async fn ensure_tls_trust(
    config: &BmuxConfig,
    paths: &ConfigPaths,
    target: &TlsTarget,
) -> Result<()> {
    if target.ca_file.is_some() {
        return Ok(());
    }
    let key = format!("{}:{}", target.host.trim(), target.port);
    if config
        .connections
        .tls_trust
        .known_gateways
        .contains_key(&key)
        || load_local_tls_pin(paths, &key)?.is_some()
    {
        return Ok(());
    }
    if config.connections.tls_trust.mode != TlsTrustMode::TrustNew {
        return Ok(());
    }
    let fingerprint = probe_tls_fingerprint(target).await?;
    save_local_tls_pin(paths, &key, &fingerprint, target)?;
    Ok(())
}

async fn probe_tls_fingerprint(target: &TlsTarget) -> Result<String> {
    let verifier = Arc::new(ProbeCertificateVerifier::default());
    let connector = TlsConnector::from(Arc::new(
        rustls::ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(verifier.clone())
            .with_no_client_auth(),
    ));
    let address = format!("{}:{}", target.host, target.port);
    let tcp = tokio::time::timeout(
        Duration::from_millis(target.connect_timeout_ms),
        TcpStream::connect(address),
    )
    .await
    .context("TLS trust probe timed out")??;
    let name = ServerName::try_from(target.server_name.clone())
        .map_err(|_| anyhow::anyhow!("invalid TLS server name"))?;
    let _stream = connector.connect(name, tcp).await?;
    verifier
        .fingerprint
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
        .context("TLS gateway did not present a certificate")
}

fn tls_connector(
    config: &BmuxConfig,
    paths: &ConfigPaths,
    target: &TlsTarget,
) -> Result<TlsConnector> {
    let mut roots = RootCertStore::empty();
    let native = rustls_native_certs::load_native_certs();
    for certificate in native.certs {
        let _ = roots.add(certificate);
    }
    if let Some(path) = &target.ca_file {
        let pem =
            std::fs::read(path).with_context(|| format!("reading CA bundle {}", path.display()))?;
        let mut reader = std::io::Cursor::new(pem);
        for certificate in rustls_pemfile::certs(&mut reader) {
            roots
                .add(certificate.context("parsing CA certificate")?)
                .context("adding CA certificate")?;
        }
    }
    let webpki = if roots.is_empty() {
        None
    } else {
        Some(
            rustls::client::WebPkiServerVerifier::builder(Arc::new(roots))
                .build()
                .context("building TLS CA verifier")?,
        )
    };
    let key = format!("{}:{}", target.host.trim(), target.port);
    let pin = config
        .connections
        .tls_trust
        .known_gateways
        .get(&key)
        .map(|entry| entry.fingerprint_sha256.clone())
        .or(load_local_tls_pin(paths, &key)?);
    let verifier: Arc<dyn ServerCertVerifier> = Arc::new(ConnectionCertificateVerifier {
        key,
        mode: config.connections.tls_trust.mode,
        pin,
        webpki,
    });
    Ok(TlsConnector::from(Arc::new(
        rustls::ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(verifier)
            .with_no_client_auth(),
    )))
}

#[derive(Debug, Serialize, Deserialize, Default)]
struct KnownGatewaysStore {
    #[serde(default)]
    gateways: std::collections::BTreeMap<String, KnownGatewayState>,
}

#[derive(Debug, Serialize, Deserialize)]
struct KnownGatewayState {
    fingerprint_sha256: String,
    #[serde(default)]
    trusted_at_unix_ms: u64,
    #[serde(default)]
    server_name: Option<String>,
    #[serde(default)]
    label: Option<String>,
}

fn load_local_tls_pin(paths: &ConfigPaths, key: &str) -> Result<Option<String>> {
    let path = paths.known_gateways_file();
    if !path.exists() {
        return Ok(None);
    }
    let contents = std::fs::read_to_string(&path)
        .with_context(|| format!("reading TLS trust store {}", path.display()))?;
    let store: KnownGatewaysStore = toml::from_str(&contents)
        .with_context(|| format!("parsing TLS trust store {}", path.display()))?;
    Ok(store
        .gateways
        .get(key)
        .map(|entry| entry.fingerprint_sha256.clone()))
}

fn save_local_tls_pin(
    paths: &ConfigPaths,
    key: &str,
    fingerprint: &str,
    target: &TlsTarget,
) -> Result<()> {
    let path = paths.known_gateways_file();
    let mut store = if path.exists() {
        toml::from_str::<KnownGatewaysStore>(&std::fs::read_to_string(&path)?)?
    } else {
        KnownGatewaysStore::default()
    };
    store.gateways.insert(
        key.to_string(),
        KnownGatewayState {
            fingerprint_sha256: fingerprint.to_string(),
            trusted_at_unix_ms: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis()
                .try_into()
                .unwrap_or(u64::MAX),
            server_name: Some(target.server_name.clone()),
            label: Some(target.label.clone()),
        },
    );
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, toml::to_string_pretty(&store)?)?;
    Ok(())
}

#[derive(Debug)]
struct ConnectionCertificateVerifier {
    key: String,
    mode: TlsTrustMode,
    pin: Option<String>,
    webpki: Option<Arc<rustls::client::WebPkiServerVerifier>>,
}

impl ServerCertVerifier for ConnectionCertificateVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp_response: &[u8],
        now: UnixTime,
    ) -> std::result::Result<ServerCertVerified, RustlsError> {
        if let Some(webpki) = &self.webpki
            && webpki
                .verify_server_cert(end_entity, intermediates, server_name, ocsp_response, now)
                .is_ok()
        {
            return Ok(ServerCertVerified::assertion());
        }
        let actual = certificate_fingerprint(end_entity.as_ref());
        if let Some(pin) = &self.pin {
            return if pin.trim().eq_ignore_ascii_case(&actual) {
                Ok(ServerCertVerified::assertion())
            } else {
                Err(RustlsError::General(format!(
                    "TLS gateway {} fingerprint mismatch: expected {pin}, received {actual}",
                    self.key
                )))
            };
        }
        match self.mode {
            TlsTrustMode::TrustNew => Ok(ServerCertVerified::assertion()),
            TlsTrustMode::Prompt => Err(RustlsError::General(format!(
                "TLS gateway {} is unknown; fingerprint {actual}; trust prompt required",
                self.key
            ))),
            TlsTrustMode::RequireKnown => Err(RustlsError::General(format!(
                "TLS gateway {} is unknown; configure a CA or known pin",
                self.key
            ))),
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        certificate: &CertificateDer<'_>,
        signature: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, RustlsError> {
        if let Some(webpki) = &self.webpki {
            return webpki.verify_tls12_signature(message, certificate, signature);
        }
        verify_tls12_signature(message, certificate, signature)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        certificate: &CertificateDer<'_>,
        signature: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, RustlsError> {
        if let Some(webpki) = &self.webpki {
            return webpki.verify_tls13_signature(message, certificate, signature);
        }
        verify_tls13_signature(message, certificate, signature)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.webpki
            .as_ref()
            .map_or_else(supported_verify_schemes, |webpki| {
                webpki.supported_verify_schemes()
            })
    }
}

#[derive(Debug, Default)]
struct ProbeCertificateVerifier {
    fingerprint: std::sync::Mutex<Option<String>>,
}

impl ServerCertVerifier for ProbeCertificateVerifier {
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
            Some(certificate_fingerprint(end_entity.as_ref()));
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        certificate: &CertificateDer<'_>,
        signature: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, RustlsError> {
        verify_tls12_signature(message, certificate, signature)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        certificate: &CertificateDer<'_>,
        signature: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, RustlsError> {
        verify_tls13_signature(message, certificate, signature)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        supported_verify_schemes()
    }
}

fn verify_tls12_signature(
    message: &[u8],
    certificate: &CertificateDer<'_>,
    signature: &DigitallySignedStruct,
) -> std::result::Result<HandshakeSignatureValid, RustlsError> {
    let algorithms = rustls::crypto::CryptoProvider::get_default()
        .ok_or_else(|| RustlsError::General("rustls crypto provider unavailable".to_string()))?
        .signature_verification_algorithms;
    rustls::crypto::verify_tls12_signature(message, certificate, signature, &algorithms)
}

fn verify_tls13_signature(
    message: &[u8],
    certificate: &CertificateDer<'_>,
    signature: &DigitallySignedStruct,
) -> std::result::Result<HandshakeSignatureValid, RustlsError> {
    let algorithms = rustls::crypto::CryptoProvider::get_default()
        .ok_or_else(|| RustlsError::General("rustls crypto provider unavailable".to_string()))?
        .signature_verification_algorithms;
    rustls::crypto::verify_tls13_signature(message, certificate, signature, &algorithms)
}

fn supported_verify_schemes() -> Vec<SignatureScheme> {
    rustls::crypto::CryptoProvider::get_default().map_or_else(Vec::new, |provider| {
        provider
            .signature_verification_algorithms
            .supported_schemes()
    })
}

fn certificate_fingerprint(certificate: &[u8]) -> String {
    let digest = Sha256::digest(certificate);
    let mut value = String::with_capacity(digest.len() * 2 + 7);
    value.push_str("SHA256:");
    for byte in digest {
        write!(&mut value, "{byte:02x}").expect("writing to String cannot fail");
    }
    value
}

async fn connect_iroh(
    config: &BmuxConfig,
    paths: &ConfigPaths,
    target: &IrohTarget,
) -> Result<BmuxClient, ConnectionError> {
    let endpoint = Endpoint::builder(presets::N0)
        .alpns(vec![BMUX_IROH_ALPN.to_vec()])
        .bind()
        .await
        .map_err(|error| connection_error(target, error))?;
    endpoint.online().await;
    let endpoint_id: EndpointId = target
        .endpoint_id
        .parse()
        .map_err(|error| connection_error(target, error))?;
    let address = if let Some(relay) = &target.relay_url {
        EndpointAddr::new(endpoint_id).with_relay_url(
            relay
                .parse()
                .map_err(|error| connection_error(target, error))?,
        )
    } else {
        EndpointAddr::new(endpoint_id)
    };
    let connection = tokio::time::timeout(
        Duration::from_millis(target.connect_timeout_ms),
        endpoint.connect(address, BMUX_IROH_ALPN),
    )
    .await
    .map_err(|error| connection_error(target, error))?
    .map_err(|error| connection_error(target, error))?;
    if target.require_ssh_auth {
        authenticate_iroh(&connection)
            .await
            .map_err(|error| authentication_error(target, error))?;
    }
    let (mut send, mut receive) = connection
        .open_bi()
        .await
        .map_err(|error| connection_error(target, error))?;
    let (client_stream, bridge_stream) = tokio::io::duplex(64 * 1024);
    let (mut bridge_read, mut bridge_write) = tokio::io::split(bridge_stream);
    tokio::spawn(async move {
        let _ = tokio::io::copy(&mut receive, &mut bridge_write).await;
        let _ = bridge_write.shutdown().await;
    });
    tokio::spawn(async move {
        let _endpoint = endpoint;
        let _connection = connection;
        let _ = tokio::io::copy(&mut bridge_read, &mut send).await;
        let _ = send.finish();
    });
    let use_compression = match target.compression {
        CompressionMode::None => false,
        CompressionMode::Zstd => true,
        CompressionMode::Auto => {
            config.behavior.compression.enabled
                && matches!(
                    config.behavior.compression.remote,
                    bmux_config::CompressionMode::Auto | bmux_config::CompressionMode::Zstd
                )
        }
    };
    let stream = if use_compression {
        ErasedIpcStream::new(Box::new(CompressedStream::new(client_stream, 1)))
    } else {
        ErasedIpcStream::new(Box::new(client_stream))
    };
    BmuxClient::connect_with_bridge_stream(
        stream,
        Duration::from_millis(target.connect_timeout_ms),
        "bmux-connections-iroh".to_string(),
        load_or_create_principal_id(paths).map_err(|error| connection_error(target, error))?,
    )
    .await
    .map_err(|error| connection_error(target, error))
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

#[derive(Debug, Serialize, Deserialize)]
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

async fn authenticate_iroh(connection: &iroh::endpoint::Connection) -> Result<()> {
    let (mut send, receive) = connection.open_bi().await.context("opening auth stream")?;
    let mut reader = BufReader::new(receive);
    write_json_line(
        &mut send,
        &ClientAuthMessage::Hello {
            version: AUTH_PROTOCOL_VERSION,
        },
    )
    .await?;
    let (challenge, allowed_fingerprints) = match read_json_line(&mut reader).await? {
        ServerAuthMessage::Challenge {
            challenge,
            allowed_fingerprints,
        } => (challenge, allowed_fingerprints),
        ServerAuthMessage::Result { error, .. } => {
            anyhow::bail!("authentication rejected: {}", error.unwrap_or_default());
        }
    };
    let proof = sign_challenge_with_any_agent_key(
        &challenge,
        DEFAULT_SSHSIG_NAMESPACE,
        &allowed_fingerprints,
    )?
    .context("no SSH agent key matches the remote allowlist")?;
    write_json_line(
        &mut send,
        &ClientAuthMessage::Proof {
            proof: proof.into(),
        },
    )
    .await?;
    match read_json_line(&mut reader).await? {
        ServerAuthMessage::Result { ok: true, .. } => {
            send.finish().context("finishing auth stream")?;
            Ok(())
        }
        ServerAuthMessage::Result { error, .. } => {
            anyhow::bail!("authentication failed: {}", error.unwrap_or_default());
        }
        ServerAuthMessage::Challenge { .. } => anyhow::bail!("unexpected second challenge"),
    }
}

async fn write_json_line<T: Serialize + Sync>(
    send: &mut iroh::endpoint::SendStream,
    value: &T,
) -> Result<()> {
    let mut bytes = serde_json::to_vec(value)?;
    bytes.push(b'\n');
    send.write_all(&bytes).await?;
    send.flush().await?;
    Ok(())
}

async fn read_json_line<T: serde::de::DeserializeOwned>(
    reader: &mut BufReader<iroh::endpoint::RecvStream>,
) -> Result<T> {
    let mut line = String::new();
    if reader.read_line(&mut line).await? == 0 {
        anyhow::bail!("authentication stream closed");
    }
    Ok(serde_json::from_str(line.trim_end())?)
}

fn load_or_create_principal_id(paths: &ConfigPaths) -> Result<Uuid> {
    let path = paths.principal_id_file();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    match std::fs::read_to_string(&path) {
        Ok(value) => Ok(Uuid::parse_str(value.trim())?),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let id = Uuid::new_v4();
            std::fs::write(path, id.to_string())?;
            Ok(id)
        }
        Err(error) => Err(error.into()),
    }
}

trait TargetLabel {
    fn reference(&self) -> &str;
}

impl TargetLabel for ResolvedTarget {
    fn reference(&self) -> &str {
        self.reference()
    }
}

impl TargetLabel for SshTarget {
    fn reference(&self) -> &str {
        &self.reference
    }
}

impl TargetLabel for TlsTarget {
    fn reference(&self) -> &str {
        &self.reference
    }
}

impl TargetLabel for IrohTarget {
    fn reference(&self) -> &str {
        &self.reference
    }
}

fn connection_error(target: &impl TargetLabel, error: impl std::fmt::Display) -> ConnectionError {
    ConnectionError::ConnectionFailed {
        target: target.reference().to_string(),
        reason: error.to_string(),
    }
}

fn connection_failure(target: &impl TargetLabel, reason: &str) -> ConnectionError {
    ConnectionError::ConnectionFailed {
        target: target.reference().to_string(),
        reason: reason.to_string(),
    }
}

fn authentication_error(
    target: &impl TargetLabel,
    error: impl std::fmt::Display,
) -> ConnectionError {
    ConnectionError::AuthenticationFailed {
        target: target.reference().to_string(),
        reason: error.to_string(),
    }
}

fn trust_error(target: &impl TargetLabel, error: impl std::fmt::Display) -> ConnectionError {
    ConnectionError::TrustFailed {
        target: target.reference().to_string(),
        reason: error.to_string(),
    }
}

fn trust_failure(target: &impl TargetLabel, reason: &str) -> ConnectionError {
    ConnectionError::TrustFailed {
        target: target.reference().to_string(),
        reason: reason.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ssh_command_preserves_transport_security_options() {
        let target = SshTarget {
            reference: "prod".to_string(),
            label: "prod".to_string(),
            host: "example.com".to_string(),
            user: Some("ops".to_string()),
            port: Some(2222),
            identity_file: Some(std::path::Path::new("/tmp/id").to_path_buf()),
            known_hosts_file: Some(std::path::Path::new("/tmp/known_hosts").to_path_buf()),
            strict_host_key_checking: true,
            jump: Some("jump.example.com".to_string()),
            remote_bmux_path: "bmux".to_string(),
            connect_timeout_ms: 8_000,
            server_start_mode: RemoteServerStartMode::Auto,
        };
        let command = ssh_command(&target);
        let args = command
            .as_std()
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert!(args.contains(&"StrictHostKeyChecking=yes".to_string()));
        assert!(args.contains(&"UserKnownHostsFile=/tmp/known_hosts".to_string()));
        assert!(args.contains(&"ops@example.com".to_string()));
    }
}
