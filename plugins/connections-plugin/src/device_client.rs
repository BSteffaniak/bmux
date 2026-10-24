//! One private generated pairing/admission consumer; no SSH agent or host secrets.
use crate::identity::DeviceIdentity;
use crate::listener::{ALPN, read_envelope, write_envelope};
use anyhow::{Result, ensure};
use bmux_connections_plugin_api::{
    pairing_commands::client as requests, pairing_types::Invitation,
};
use bmux_ipc::{Envelope, EnvelopeKind, InvokeServiceKind, Request, Response, ResponsePayload};
use bmux_plugin_sdk::{TypedDispatchClient, TypedDispatchClientError};
use std::time::Duration;

struct RetainedStream {
    stream: tokio::io::Join<iroh::endpoint::RecvStream, iroh::endpoint::SendStream>,
    _endpoint: iroh::Endpoint,
    _connection: iroh::endpoint::Connection,
}

impl tokio::io::AsyncRead for RetainedStream {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buffer: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.get_mut().stream).poll_read(cx, buffer)
    }
}
impl tokio::io::AsyncWrite for RetainedStream {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buffer: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        std::pin::Pin::new(&mut self.get_mut().stream).poll_write(cx, buffer)
    }
    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.get_mut().stream).poll_flush(cx)
    }
    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.get_mut().stream).poll_shutdown(cx)
    }
}

struct PreAdmission {
    stream: tokio::io::Join<iroh::endpoint::RecvStream, iroh::endpoint::SendStream>,
    request_id: u64,
}

impl TypedDispatchClient for PreAdmission {
    fn requires_service_contracts(&self) -> bool {
        true
    }

    async fn invoke_service_raw(
        &mut self,
        capability: &str,
        kind: InvokeServiceKind,
        interface_id: &str,
        operation: &str,
        payload: Vec<u8>,
    ) -> Result<Vec<u8>, TypedDispatchClientError> {
        let result = async {
            self.request_id += 1;
            let request = Request::InvokeService {
                capability: capability.into(),
                kind,
                interface_id: interface_id.into(),
                operation: operation.into(),
                payload,
            };
            write_envelope(
                &mut self.stream,
                &Envelope::new(
                    self.request_id,
                    EnvelopeKind::Request,
                    bmux_ipc::encode(&request)?,
                ),
            )
            .await?;
            let envelope = read_envelope(&mut self.stream).await?;
            ensure!(
                envelope.kind == EnvelopeKind::Response && envelope.request_id == self.request_id,
                "invalid admission response"
            );
            let response: Response = bmux_ipc::decode(&envelope.payload)?;
            let Response::Ok(ResponsePayload::ServiceInvoked { payload }) = response else {
                anyhow::bail!("admission rejected");
            };
            Ok::<_, anyhow::Error>(payload)
        }
        .await;
        result.map_err(|_| {
            TypedDispatchClientError::transport(
                interface_id,
                operation,
                "admission exchange failed",
            )
        })
    }
}

/// Direct daemonless invitation client. Caller creates/loads its device identity
/// locally and persists the returned host/device alias only after `result`.
/// Every poll uses a fresh bounded stream; no local bmux daemon is involved.
pub struct PairingClient {
    endpoint: iroh::Endpoint,
    connection: iroh::endpoint::Connection,
    link: crate::pairing::invitation::InvitationLink,
    nonce: [u8; 32],
    peer_key: String,
    phrase: String,
    expiry_task: tokio::task::JoinHandle<()>,
}
// Cancellation/drop closes transport even while connect/submit is suspended.
struct PairingEndpointGuard(Option<iroh::Endpoint>);
fn drop_guard_without_close(mut guard: PairingEndpointGuard) {
    guard.0.take();
}
impl Drop for PairingEndpointGuard {
    fn drop(&mut self) {
        let Some(endpoint) = self.0.take() else {
            return;
        };
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                endpoint.close().await;
            });
        }
    }
}
impl Drop for PairingClient {
    fn drop(&mut self) {
        self.expiry_task.abort();
        self.connection.close(0u32.into(), b"pairing finished");
        let endpoint = self.endpoint.clone();
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                endpoint.close().await;
            });
        }
    }
}
impl PairingClient {
    /// Local relay policy is authoritative; invitation routing never enables a
    /// disabled relay or adds an unapproved custom relay.
    pub async fn connect_named_with_policy(
        identity: &DeviceIdentity,
        link: crate::pairing::invitation::InvitationLink,
        mut address: iroh::EndpointAddr,
        device_name: Option<String>,
        policy: &crate::config::RelayPolicy,
        timeout: Duration,
    ) -> Result<Self> {
        ensure!(
            address.id.to_string() == link.host_key,
            "pairing host mismatch"
        );
        match policy {
            crate::config::RelayPolicy::Disabled => {
                address = address
                    .ip_addrs()
                    .fold(iroh::EndpointAddr::new(address.id), |value, ip| {
                        value.with_ip_addr(*ip)
                    });
            }
            crate::config::RelayPolicy::Custom { urls } => {
                ensure!(
                    address.relay_urls().all(|relay| urls.iter().any(|url| url
                        .parse::<iroh::RelayUrl>()
                        .ok()
                        .as_ref()
                        == Some(relay))),
                    "pairing relay denied by local policy"
                );
            }
            crate::config::RelayPolicy::Default => {}
        }
        // Validate before acquiring endpoint/network resources.
        link.transcript_named(&identity.public_key(), &[0; 32], device_name.as_deref())?;
        let endpoint = policy
            .apply(identity.endpoint_builder())?
            .alpns(vec![ALPN.to_vec()])
            .bind()
            .await?;
        let guard = PairingEndpointGuard(Some(endpoint.clone()));
        let result = tokio::time::timeout(timeout.min(Duration::from_secs(30)), async {
            let connection = endpoint.connect(address, ALPN).await?;
            Self::on_connection_named(
                endpoint.clone(),
                connection,
                identity,
                link,
                device_name,
                timeout,
            )
            .await
        })
        .await;
        if let Ok(Ok(client)) = result {
            drop_guard_without_close(guard);
            Ok(client)
        } else {
            endpoint.close().await;
            anyhow::bail!("pairing connection failed")
        }
    }
    async fn on_connection_named(
        endpoint: iroh::Endpoint,
        connection: iroh::endpoint::Connection,
        identity: &DeviceIdentity,
        link: crate::pairing::invitation::InvitationLink,
        device_name: Option<String>,
        timeout: Duration,
    ) -> Result<Self> {
        ensure!(
            connection.remote_id().to_string() == link.host_key,
            "pairing host mismatch"
        );
        let nonce = crate::pairing::random_secret();
        let transcript =
            link.transcript_named(&identity.public_key(), &nonce, device_name.as_deref())?;
        let phrase = {
            let hash = crate::pairing::digest(&transcript);
            hash[..8]
                .chunks_exact(2)
                .map(|pair| format!("{:02x}{:02x}", pair[0], pair[1]))
                .collect::<Vec<_>>()
                .join("-")
        };
        let signature = identity.sign(&transcript).to_bytes().to_vec();
        let expires_in = link
            .expires_at
            .saturating_sub(crate::pairing::now()?)
            .min(600);
        let expiring = connection.clone();
        let expiry_task = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(expires_in)).await;
            expiring.close(0u32.into(), b"pairing expired");
        });
        let client = Self {
            endpoint,
            connection,
            link,
            nonce,
            peer_key: identity.public_key(),
            phrase,
            expiry_task,
        };
        let info = tokio::time::timeout(timeout, async {
            let mut stream = client.stream().await?;
            Ok::<_, anyhow::Error>(
                bmux_connections_plugin_api::pairing_peer_commands::client::submit_pairing(
                    &mut stream,
                    client.link.session_id.clone(),
                    client.nonce.to_vec(),
                    signature,
                    device_name,
                )
                .await?,
            )
        })
        .await??;
        ensure!(
            info.verification_phrase.as_deref() == Some(&client.phrase),
            "pairing transcript mismatch"
        );
        Ok(client)
    }
    async fn stream(&self) -> Result<PreAdmission> {
        ensure!(
            self.link.expires_at > crate::pairing::now()?,
            "pairing invitation expired"
        );
        let (send, receive) = self.connection.open_bi().await?;
        Ok(PreAdmission {
            stream: tokio::io::join(receive, send),
            request_id: 0,
        })
    }
    pub fn session_id(&self) -> &str {
        &self.link.session_id
    }
    pub fn verification_phrase(&self) -> &str {
        &self.phrase
    }
    pub async fn status(
        &self,
        timeout: Duration,
    ) -> Result<bmux_connections_plugin_api::pairing_types::PairingRequest> {
        tokio::time::timeout(timeout.min(Duration::from_secs(10)), async {
            let mut stream = self.stream().await?;
            Ok(
                bmux_connections_plugin_api::pairing_peer_commands::client::poll_pairing(
                    &mut stream,
                    self.link.session_id.clone(),
                    self.nonce.to_vec(),
                )
                .await?,
            )
        })
        .await?
    }
    pub async fn result(
        &self,
        timeout: Duration,
    ) -> Result<bmux_connections_plugin_api::pairing_types::Device> {
        tokio::time::timeout(timeout.min(Duration::from_secs(10)), async {
            let mut stream = self.stream().await?;
            let device = bmux_connections_plugin_api::pairing_peer_commands::client::pairing_result(
                &mut stream,
                self.link.session_id.clone(),
                self.nonce.to_vec(),
            ).await?;
            ensure!(device.device_key == self.peer_key && device.device_id == self.link.session_id
                && device.access == bmux_connections_plugin_api::pairing_types::DeviceAccess::FullLocalRuntime,
                "pairing result binding mismatch");
            Ok(device)
        })
        .await?
    }
    pub async fn close(self) {
        self.endpoint.close().await;
    }
}

/// Establish a proof-bound stream ready for ordinary Hello. The returned subject
/// MUST be used for Hello, not a local principal file. No downgrade is attempted.
/// The caller retains Endpoint/Connection until the IPC stream is dropped.
pub async fn admit(
    endpoint: &iroh::Endpoint,
    connection: &iroh::endpoint::Connection,
    identity: &DeviceIdentity,
    device_id: &str,
    invitation: Option<&Invitation>,
    timeout: Duration,
) -> Result<(bmux_ipc::transport::ErasedIpcStream, uuid::Uuid)> {
    tokio::time::timeout(timeout, async {
        let (send, receive) = connection.open_bi().await?;
        let mut client = PreAdmission {
            stream: tokio::io::join(receive, send),
            request_id: 0,
        };
        let subject = if let Some(invitation) = invitation {
            ensure!(
                invitation.endpoint_id == connection.remote_id().to_string(),
                "invitation host mismatch"
            );
            ensure!(
                invitation.device_key == identity.public_key(),
                "invitation device mismatch"
            );
            let signature = identity
                .sign(&invitation.possession_message)
                .to_bytes()
                .to_vec();
            let device = requests::redeem_invitation(
                &mut client,
                invitation.invitation_id.clone(),
                invitation.secret.clone(),
                signature,
            )
            .await?;
            ensure!(
                device.device_key == identity.public_key(),
                "redeemed device mismatch"
            );
            device.device_id
        } else {
            device_id.to_string()
        };
        let proof = requests::begin_admission(&mut client, subject.clone()).await?;
        let signature = identity.sign(&proof.possession_message).to_bytes().to_vec();
        let ticket = requests::finish_admission(&mut client, proof.challenge, signature).await?;
        let subject = subject.parse()?;
        let preamble = Request::AdmitStream {
            admission_version: 1,
            ticket,
        };
        write_envelope(
            &mut client.stream,
            &Envelope::new(
                client.request_id + 1,
                EnvelopeKind::Request,
                bmux_ipc::encode(&preamble)?,
            ),
        )
        .await?;
        Ok((
            bmux_ipc::transport::ErasedIpcStream::new(Box::new(RetainedStream {
                stream: client.stream,
                _endpoint: endpoint.clone(),
                _connection: connection.clone(),
            })),
            subject,
        ))
    })
    .await?
}

pub const fn alpn() -> &'static [u8] {
    ALPN
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::admission::AdmissionAuthority;
    use crate::identity::RuntimeIdentity;
    use crate::listener::Listener;
    use crate::pairing::{LocalOwner, PairingAuthority};
    use bmux_connections_plugin_api::pairing_types::DeviceAccess;
    use std::sync::Arc;

    fn direct(builder: iroh::endpoint::Builder) -> iroh::endpoint::Builder {
        builder
            .clear_address_lookup()
            .clear_relay_transports()
            .clear_ip_transports()
            .bind_addr("127.0.0.1:0")
            .unwrap()
    }

    #[tokio::test]
    async fn invitation_pairing_loopback_grant_gate_and_no_backend() {
        use crate::pairing::invitation::InvitationLink;
        use bmux_connections_plugin_api::pairing_types::SessionPhase;
        let root = tempfile::tempdir().unwrap();
        let state = root.path().join("state");
        let runtime = root.path().join("runtime");
        std::fs::create_dir_all(&state).unwrap();
        std::fs::create_dir_all(&runtime).unwrap();
        let host = RuntimeIdentity::load_or_create(&state, &runtime).unwrap();
        let device = DeviceIdentity::load_or_create(&state, &runtime).unwrap();
        let pairing =
            Arc::new(PairingAuthority::open(&state, &runtime, host.endpoint_id()).unwrap());
        let admission = Arc::new(AdmissionAuthority::new(Arc::clone(&pairing)));
        // No backend socket exists: pairing must never attempt generic IPC.
        let backend = bmux_ipc::IpcEndpoint::unix_socket(runtime.join("absent.sock"));
        let mut listener = Listener::start(
            direct(host.endpoint_builder()),
            Arc::clone(&pairing),
            admission,
            backend,
        )
        .await
        .unwrap();
        let owner = LocalOwner::from_trusted_local_owner();
        let session = pairing
            .sessions
            .begin_session(&owner, &pairing, 60, listener.targets())
            .unwrap();
        let endpoint = direct(device.endpoint_builder())
            .alpns(vec![alpn().to_vec()])
            .bind()
            .await
            .unwrap();
        let connection = endpoint.connect(listener.status(), alpn()).await.unwrap();
        let client = PairingClient::on_connection_named(
            endpoint,
            connection,
            &device,
            InvitationLink::parse(&session.invitation_link).unwrap(),
            Some("Loopback laptop".into()),
            Duration::from_secs(3),
        )
        .await
        .unwrap();
        let pending = client.status(Duration::from_secs(3)).await.unwrap();
        assert_eq!(pending.phase, SessionPhase::PendingApproval);
        assert_eq!(pending.device_name.as_deref(), Some("Loopback laptop"));
        assert_eq!(
            pending.verification_phrase.as_deref(),
            Some(client.verification_phrase())
        );
        assert_eq!(pending.session_id, session.session_id);
        assert!(client.result(Duration::from_secs(3)).await.is_err());
        let staged = pairing
            .sessions
            .approve_session(
                &owner,
                &pairing,
                &session.session_id,
                client.verification_phrase(),
            )
            .unwrap();
        assert_eq!(staged.access, DeviceAccess::Denied);
        assert!(client.result(Duration::from_secs(3)).await.is_err());
        pairing
            .sessions
            .activate_session(&owner, &pairing, &session.session_id)
            .unwrap();
        let enrolled = client.result(Duration::from_secs(3)).await.unwrap();
        assert_eq!(enrolled.device_key, device.public_key());
        pairing
            .sessions
            .abort_session(&owner, &pairing, &session.session_id, false)
            .unwrap();
        assert!(client.result(Duration::from_secs(3)).await.is_err());
        client.close().await;
        listener.stop().await;
    }

    #[tokio::test]
    async fn admitted_preamble_is_proof_bound_and_stop_cancels_active_bridge() {
        let root = tempfile::tempdir().unwrap();
        let state = root.path().join("state");
        let runtime = root.path().join("runtime");
        std::fs::create_dir_all(&state).unwrap();
        std::fs::create_dir_all(&runtime).unwrap();
        let host = RuntimeIdentity::load_or_create(&state, &runtime).unwrap();
        let device = DeviceIdentity::load_or_create(&state, &runtime).unwrap();
        let pairing =
            Arc::new(PairingAuthority::open(&state, &runtime, host.endpoint_id()).unwrap());
        let invitation = pairing
            .create_invitation(
                &LocalOwner::from_trusted_local_owner(),
                &device.public_key(),
                60,
                DeviceAccess::FullLocalRuntime,
            )
            .unwrap();
        let admission = Arc::new(AdmissionAuthority::new(Arc::clone(&pairing)));
        let backend = bmux_ipc::IpcEndpoint::unix_socket(runtime.join("backend.sock"));
        let backend_listener = bmux_ipc::transport::LocalIpcListener::bind(&backend).unwrap();
        let mut listener =
            Listener::start(direct(host.endpoint_builder()), pairing, admission, backend)
                .await
                .unwrap();
        let endpoint = direct(device.endpoint_builder())
            .alpns(vec![alpn().to_vec()])
            .bind()
            .await
            .unwrap();
        let connection = tokio::time::timeout(
            Duration::from_secs(5),
            endpoint.connect(listener.status(), alpn()),
        )
        .await
        .unwrap()
        .unwrap();
        let (mut stream, subject) = admit(
            &endpoint,
            &connection,
            &device,
            "",
            Some(&invitation),
            Duration::from_secs(5),
        )
        .await
        .unwrap();
        assert!(!subject.is_nil());
        let mut backend_stream =
            tokio::time::timeout(Duration::from_secs(2), backend_listener.accept())
                .await
                .unwrap()
                .unwrap();
        let envelope = backend_stream.recv_envelope().await.unwrap();
        assert!(
            matches!(bmux_ipc::decode::<Request>(&envelope.payload).unwrap(), Request::AdmitStream { admission_version: 1, ticket } if ticket.len() == 32)
        );
        listener.stop().await;
        // Stop must cancel established bridge tasks, not only future accepts.
        let closed =
            tokio::time::timeout(Duration::from_secs(2), backend_stream.recv_envelope()).await;
        assert!(closed.expect("stop closes active backend streams").is_err());
        assert!(
            tokio::time::timeout(Duration::from_secs(2), stream.recv_envelope())
                .await
                .unwrap()
                .is_err()
        );
        endpoint.close().await;
    }

    fn activate_execution_fixture(paths: &bmux_config::ConfigPaths) {
        use bmux_plugin_sdk::RustPlugin;
        use sha2::Digest;
        use std::fmt::Write as _;
        let helper = std::env::current_exe()
            .unwrap()
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join("bmux-pane-execution-supervisor");
        let bytes = std::fs::read(&helper)
            .expect("build bmux-pane-execution-supervisor before this integration test");
        let hash = sha2::Sha256::digest(bytes).iter().fold(
            String::with_capacity(64),
            |mut output, byte| {
                write!(&mut output, "{byte:02x}").unwrap();
                output
            },
        );
        let context = bmux_plugin_sdk::NativeLifecycleContext {
            plugin_id: "bmux.pane_runtime".into(),
            required_capabilities: vec![],
            provided_capabilities: vec![],
            services: vec![],
            available_capabilities: vec![],
            enabled_plugins: vec![],
            plugin_search_roots: vec![],
            registered_plugins: vec![],
            host: bmux_plugin_sdk::HostMetadata {
                product_name: "isolated-fixture".into(),
                product_version: "0".into(),
                plugin_api_version: bmux_plugin_sdk::CURRENT_PLUGIN_API_VERSION,
                plugin_abi_version: bmux_plugin_sdk::CURRENT_PLUGIN_ABI_VERSION,
            },
            connection: bmux_plugin_sdk::HostConnectionInfo {
                config_dir: paths.config_dir.to_string_lossy().into(),
                config_dir_candidates: vec![],
                runtime_dir: paths.runtime_dir.to_string_lossy().into(),
                data_dir: paths.data_dir.to_string_lossy().into(),
                state_dir: paths.state_dir.to_string_lossy().into(),
            },
            settings: None,
            plugin_settings_map: std::collections::BTreeMap::new(),
            host_kernel_bridge: None,
            auxiliary_executables: std::collections::BTreeMap::from([(
                "execution-supervisor".into(),
                bmux_plugin_sdk::ResolvedAuxiliaryExecutable {
                    path: helper.to_string_lossy().into(),
                    protocol_version: 7,
                    artifact_sha256: hash,
                },
            )]),
            auxiliary_executable_errors: std::collections::BTreeMap::new(),
        };
        bmux_sessions_plugin::SessionsPlugin
            .activate(context.clone())
            .unwrap();
        bmux_pane_runtime_plugin::PaneRuntimePlugin
            .activate(context)
            .unwrap();
    }

    type FixtureContexts =
        std::collections::BTreeMap<uuid::Uuid, (bmux_server::ServiceInvokeContext, usize)>;

    fn fixture_contexts() -> &'static std::sync::Mutex<FixtureContexts> {
        static CONTEXTS: std::sync::OnceLock<std::sync::Mutex<FixtureContexts>> =
            std::sync::OnceLock::new();
        CONTEXTS.get_or_init(std::sync::Mutex::default)
    }

    unsafe extern "C" fn fixture_ownership(token: *const u8, retain: bool) -> bool {
        if token.is_null() {
            return false;
        }
        let bytes = unsafe { std::slice::from_raw_parts(token, 16) };
        let Ok(token) = uuid::Uuid::from_slice(bytes) else {
            return false;
        };
        let Ok(mut contexts) = fixture_contexts().lock() else {
            return false;
        };
        let Some((_, owners)) = contexts.get_mut(&token) else {
            return false;
        };
        if retain {
            let Some(next) = owners.checked_add(1) else {
                return false;
            };
            *owners = next;
        } else {
            let Some(next) = owners.checked_sub(1) else {
                return false;
            };
            *owners = next;
            if *owners == 0 {
                contexts.remove(&token);
            }
        }
        true
    }

    fn fixture_bridge(
        context: &bmux_server::ServiceInvokeContext,
    ) -> bmux_plugin_sdk::HostKernelBridge {
        let token = uuid::Uuid::new_v4();
        let mut contexts = fixture_contexts().lock().unwrap();
        assert!(contexts.len() < 4096, "fixture context capacity");
        contexts.insert(token, (context.clone(), 1));
        drop(contexts);
        let bridge = bmux_plugin_sdk::HostKernelBridge::bound(
            fixture_kernel_bridge,
            token,
            fixture_ownership,
        );
        // Release the registration owner; the SDK handle now owns the reference.
        assert!(unsafe { fixture_ownership(token.as_bytes().as_ptr(), false) });
        assert_eq!(fixture_contexts().lock().unwrap()[&token].1, 1);
        let cloned = bridge.clone();
        assert_eq!(fixture_contexts().lock().unwrap()[&token].1, 2);
        let encoded = bmux_plugin_sdk::encode_service_message(&bridge).unwrap();
        let restored: bmux_plugin_sdk::HostKernelBridge =
            bmux_plugin_sdk::decode_service_message(&encoded).unwrap();
        assert_eq!(fixture_contexts().lock().unwrap()[&token].1, 3);
        drop(cloned);
        drop(restored);
        assert_eq!(fixture_contexts().lock().unwrap()[&token].1, 1);
        bridge
    }

    unsafe extern "C" fn fixture_kernel_bridge(
        input: *const u8,
        length: usize,
        output: *mut u8,
        capacity: usize,
        written: *mut usize,
    ) -> i32 {
        std::panic::catch_unwind(|| unsafe {
            fixture_kernel_bridge_inner(input, length, output, capacity, written)
        })
        .unwrap_or(5)
    }

    unsafe fn fixture_kernel_bridge_inner(
        input: *const u8,
        length: usize,
        output: *mut u8,
        capacity: usize,
        written: *mut usize,
    ) -> i32 {
        if input.is_null() || output.is_null() || written.is_null() {
            return 1;
        }
        let input = unsafe { std::slice::from_raw_parts(input, length) };
        let Ok(request) = bmux_plugin_sdk::decode_service_message::<
            bmux_plugin_sdk::HostKernelBridgeRequest,
        >(input) else {
            return 1;
        };
        let Some(token) = request.context_token else {
            return 2;
        };
        let context = fixture_contexts()
            .lock()
            .ok()
            .and_then(|contexts| contexts.get(&token).map(|(context, _)| context.clone()));
        let Some(context) = context else { return 2 };
        // Match the CLI bridge: preserve kernel request bytes and origin, including
        // contract evidence, and allow callbacks from a Tokio runtime worker.
        let result = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(context.execute_raw(request.payload))
        });
        let Ok(payload) = result else { return 2 };
        let Ok(bytes) =
            bmux_plugin_sdk::encode_service_message(&bmux_plugin_sdk::HostKernelBridgeResponse {
                payload,
            })
        else {
            return 3;
        };
        if bytes.len() > capacity {
            return 4;
        }
        unsafe {
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), output, bytes.len());
            *written = bytes.len();
        }
        0
    }

    fn native_fixture_context(
        paths: &bmux_config::ConfigPaths,
        route: &bmux_server::ServiceRoute,
        host: &bmux_server::ServiceInvokeContext,
        payload: Vec<u8>,
    ) -> bmux_plugin_sdk::NativeServiceContext {
        use bmux_plugin_sdk::{
            HostConnectionInfo, HostMetadata, HostScope, NativeServiceContext, ProviderId,
            RegisteredService, ServiceKind, ServiceRequest,
        };
        let plugin_id = if route.interface_id.starts_with("pane-runtime") {
            "bmux.pane_runtime"
        } else {
            "bmux.permissions"
        };
        NativeServiceContext {
            plugin_id: plugin_id.into(),
            request: ServiceRequest {
                caller_plugin_id: "fixture".into(),
                service: RegisteredService {
                    capability: HostScope::new(&route.capability).unwrap(),
                    kind: if route.kind == InvokeServiceKind::Command {
                        ServiceKind::Command
                    } else {
                        ServiceKind::Query
                    },
                    interface_id: route.interface_id.clone(),
                    provider: ProviderId::Plugin(plugin_id.into()),
                },
                operation: route.operation.clone(),
                payload,
            },
            required_capabilities: vec!["bmux.sessions.policy".into()],
            provided_capabilities: vec![],
            services: vec![RegisteredService {
                capability: HostScope::new("bmux.sessions.policy").unwrap(),
                kind: ServiceKind::Query,
                interface_id: "session-policy-state".into(),
                provider: ProviderId::Plugin("bmux.permissions".into()),
            }],
            available_capabilities: vec![],
            enabled_plugins: vec!["bmux.permissions".into()],
            plugin_search_roots: vec![],
            host: HostMetadata {
                product_name: "fixture".into(),
                product_version: "0".into(),
                plugin_api_version: bmux_plugin_sdk::CURRENT_PLUGIN_API_VERSION,
                plugin_abi_version: bmux_plugin_sdk::CURRENT_PLUGIN_ABI_VERSION,
            },
            connection: HostConnectionInfo {
                config_dir: paths.config_dir.to_string_lossy().into(),
                config_dir_candidates: vec![],
                runtime_dir: paths.runtime_dir.to_string_lossy().into(),
                data_dir: paths.data_dir.to_string_lossy().into(),
                state_dir: paths.state_dir.to_string_lossy().into(),
            },
            settings: None,
            plugin_settings_map: std::collections::BTreeMap::new(),
            caller_client_id: Some(host.client_id().0),
            cancellation: bmux_plugin_sdk::CancellationToken::default(),
            host_kernel_bridge: Some(fixture_bridge(host)),
        }
    }

    fn install_resource_fixture(
        server: &bmux_server::BmuxServer,
        paths: &bmux_config::ConfigPaths,
        pairing: &Arc<PairingAuthority>,
    ) {
        use bmux_plugin_sdk::{PluginContract, RustPlugin};
        let storage = Arc::new(std::sync::Mutex::new(std::collections::BTreeMap::<
            String,
            Vec<u8>,
        >::new()));
        for (interface, operation, capability, kind, contract) in [
            (
                "permissions-commands",
                "grant-resource",
                "bmux.permissions.write",
                InvokeServiceKind::Command,
                bmux_permissions_plugin_api::permissions_commands::SERVICE_CONTRACT,
            ),
            (
                "permissions-commands",
                "revoke-resource",
                "bmux.permissions.write",
                InvokeServiceKind::Command,
                bmux_permissions_plugin_api::permissions_commands::SERVICE_CONTRACT,
            ),
            (
                "session-policy-state",
                "check",
                "bmux.sessions.policy",
                InvokeServiceKind::Query,
                bmux_permissions_plugin_api::session_policy_state::SERVICE_CONTRACT,
            ),
            (
                "pane-runtime-spawn-v1",
                "home-directory",
                "bmux.pane_runtime.spawn.v1",
                InvokeServiceKind::Query,
                bmux_pane_runtime_plugin_api::Contract::service_contract_for(
                    "pane-runtime-spawn-v1",
                )
                .unwrap(),
            ),
            (
                "pane-runtime-execution-v4",
                "new-program-session",
                "bmux.pane_runtime.execution.v4",
                InvokeServiceKind::Command,
                bmux_pane_runtime_plugin_api::pane_runtime_execution_v4::SERVICE_CONTRACT,
            ),
            (
                "pane-runtime-execution-v4",
                "send",
                "bmux.pane_runtime.execution.v4",
                InvokeServiceKind::Command,
                bmux_pane_runtime_plugin_api::pane_runtime_execution_v4::SERVICE_CONTRACT,
            ),
            (
                "pane-runtime-execution-v4",
                "terminate",
                "bmux.pane_runtime.execution.v4",
                InvokeServiceKind::Command,
                bmux_pane_runtime_plugin_api::pane_runtime_execution_v4::SERVICE_CONTRACT,
            ),
        ] {
            let paths = paths.clone();
            let pairing = Arc::clone(pairing);
            let storage = Arc::clone(&storage);
            let route = bmux_server::ServiceRoute {
                capability: capability.into(),
                kind,
                interface_id: interface.into(),
                operation: operation.into(),
            };
            if operation == "check" || interface.starts_with("pane-runtime") {
                server.approve_admitted_route(route).unwrap();
            }
            server.register_checked_service_handler_with_metadata(capability, kind, interface, operation, move |route, host, payload| {
                let paths = paths.clone(); let pairing = Arc::clone(&pairing); let storage = Arc::clone(&storage);
                async move {
                    tokio::task::spawn_blocking(move || {
                        let payload = bmux_plugin_sdk::service_contract::decode_for(interface, contract, &payload)?.to_vec();
                        let context = native_fixture_context(&paths, &route, &host, payload);
                        let _router = bmux_plugin::test_support::install_test_service_router(Arc::new(
                            // TestServiceRouter fixes the SDK error ABI; it cannot be boxed here.
                            #[allow(clippy::result_large_err)]
                            move |caller, _, capability, kind, interface, operation, payload| {
                            let result = fixture_nested_call(&host, &pairing, &storage, caller, (capability, kind, interface, operation), &payload);
                            result.map_err(|_| bmux_plugin_sdk::PluginError::ServiceProtocol { details: "fixture nested dispatch failed".into() })
                        }));
                        let response = if interface.starts_with("pane-runtime") { bmux_pane_runtime_plugin::PaneRuntimePlugin.invoke_service(context) } else { bmux_permissions_plugin::PermissionsPlugin.invoke_service(context) };
                        anyhow::ensure!(response.error.is_none(), "real handler rejected: {:?}", response.error);
                        Ok::<_, anyhow::Error>(bmux_plugin_sdk::service_contract::encode_response(&response.payload).into())
                    }).await?
                }
            }).unwrap();
        }
    }

    fn fixture_nested_call(
        host: &bmux_server::ServiceInvokeContext,
        pairing: &PairingAuthority,
        storage: &std::sync::Mutex<std::collections::BTreeMap<String, Vec<u8>>>,
        caller: &str,
        route: (&str, bmux_plugin_sdk::ServiceKind, &str, &str),
        payload: &[u8],
    ) -> anyhow::Result<Vec<u8>> {
        use bmux_plugin_sdk::{decode_service_message, encode_service_message};
        let (capability, kind, interface, operation) = route;
        match (interface, operation) {
            ("storage-query/v1", "get") => {
                let request: bmux_plugin_sdk::StorageGetRequest = decode_service_message(payload)?;
                Ok(encode_service_message(
                    &bmux_plugin_sdk::StorageGetResponse {
                        value: storage
                            .lock()
                            .unwrap()
                            .get(&format!("{caller}:{}", request.key.as_str()))
                            .cloned(),
                    },
                )?)
            }
            ("storage-command/v1", "set") => {
                let request: bmux_plugin_sdk::StorageSetRequest = decode_service_message(payload)?;
                storage
                    .lock()
                    .unwrap()
                    .insert(format!("{caller}:{}", request.key.as_str()), request.value);
                Ok(encode_service_message(&())?)
            }
            ("pairing-state", "check-device-access") => {
                let request: bmux_connections_plugin_api::pairing_state::client::CheckDeviceAccessRequest = decode_service_message(bmux_plugin_sdk::service_contract::decode_for(
                    interface,
                    bmux_connections_plugin_api::pairing_state::SERVICE_CONTRACT,
                    payload,
                )?)?;
                Ok(bmux_plugin_sdk::service_contract::encode_response(
                    &encode_service_message(&pairing.check_device_access(request.subject_id)?)?,
                ))
            }
            _ => {
                let contract = <bmux_permissions_plugin_api::Contract as bmux_plugin_sdk::PluginContract>::service_contract_for(interface).ok_or_else(|| anyhow::anyhow!("missing fixture service contract"))?;
                bmux_plugin_sdk::service_contract::decode_for(interface, contract, payload)?;
                let request = Request::InvokeService {
                    capability: capability.into(),
                    kind: if kind == bmux_plugin_sdk::ServiceKind::Command {
                        InvokeServiceKind::Command
                    } else {
                        InvokeServiceKind::Query
                    },
                    interface_id: interface.into(),
                    operation: operation.into(),
                    payload: payload.to_vec(),
                };
                let bytes = tokio::task::block_in_place(|| {
                    tokio::runtime::Handle::current()
                        .block_on(host.execute_raw(bmux_ipc::encode(&request)?))
                })?;
                let Response::Ok(ResponsePayload::ServiceInvoked { payload }) =
                    bmux_ipc::decode(&bytes)?
                else {
                    anyhow::bail!("nested service denied")
                };
                Ok(payload)
            }
        }
    }

    struct CheckedLocal<'a>(&'a mut bmux_client::BmuxClient);

    impl TypedDispatchClient for CheckedLocal<'_> {
        fn requires_service_contracts(&self) -> bool {
            true
        }
        async fn invoke_service_raw(
            &mut self,
            capability: &str,
            kind: InvokeServiceKind,
            interface: &str,
            operation: &str,
            payload: Vec<u8>,
        ) -> Result<Vec<u8>, TypedDispatchClientError> {
            self.0
                .invoke_service_raw(capability, kind, interface, operation, payload)
                .await
                .map_err(|_| {
                    TypedDispatchClientError::transport(
                        interface,
                        operation,
                        "local fixture call failed",
                    )
                })
        }
    }

    async fn exercise_resource_policy(
        client: &mut bmux_client::BmuxClient,
        paths: &bmux_config::ConfigPaths,
        device_id: &str,
    ) {
        use bmux_permissions_plugin_api::{
            permissions_commands::client as commands, session_policy_state::client as policy,
        };
        let session = uuid::Uuid::new_v4();
        let subject = device_id.parse().unwrap();
        let forged = uuid::Uuid::new_v4();
        let check = |principal| policy::CheckRequest {
            session_id: session,
            context_id: None,
            client_id: uuid::Uuid::new_v4(),
            principal_id: principal,
            action: "read".into(),
            plugin_id: None,
            capability: None,
            execution_class: None,
        };
        let invoke_check = |request: policy::CheckRequest| {
            bmux_plugin_sdk::service_contract::encode(
                "session-policy-state",
                bmux_permissions_plugin_api::session_policy_state::SERVICE_CONTRACT,
                &bmux_plugin_sdk::encode_service_message(&request).unwrap(),
            )
            .unwrap()
        };
        let bytes = client
            .invoke_service_raw(
                "bmux.sessions.policy",
                InvokeServiceKind::Query,
                "session-policy-state",
                "check",
                invoke_check(check(forged)),
            )
            .await
            .unwrap();
        let denied: bmux_permissions_plugin_api::session_policy_state::SessionPolicyCheckResponse =
            bmux_plugin_sdk::decode_service_message(
                bmux_plugin_sdk::service_contract::decode_response(&bytes).unwrap(),
            )
            .unwrap();
        assert!(!denied.allowed);
        let mut local = bmux_client::BmuxClient::connect_with_paths(paths, "resource-local-owner")
            .await
            .unwrap();
        let mut local = CheckedLocal(&mut local);
        commands::grant_resource(&mut local, subject, Some(session), "owner".into())
            .await
            .unwrap();
        let bytes = client
            .invoke_service_raw(
                "bmux.sessions.policy",
                InvokeServiceKind::Query,
                "session-policy-state",
                "check",
                invoke_check(check(forged)),
            )
            .await
            .unwrap();
        let allowed: bmux_permissions_plugin_api::session_policy_state::SessionPolicyCheckResponse =
            bmux_plugin_sdk::decode_service_message(
                bmux_plugin_sdk::service_contract::decode_response(&bytes).unwrap(),
            )
            .unwrap();
        assert!(
            allowed.allowed,
            "trusted device subject, not forged wire principal, owns grant"
        );
        let home = bmux_pane_runtime_plugin_api::pane_runtime_spawn_v1::client::home_directory(
            &mut CheckedLocal(client),
            session,
        )
        .await
        .unwrap();
        assert!(
            home.is_ok(),
            "real pane handler must consume real permissions grant"
        );
        commands::revoke_resource(&mut local, subject, Some(session))
            .await
            .unwrap();
        let denied = bmux_pane_runtime_plugin_api::pane_runtime_spawn_v1::client::home_directory(
            &mut CheckedLocal(client),
            session,
        )
        .await
        .unwrap();
        assert!(denied.is_err());
        exercise_execution_mutation(client, &mut local, subject).await;
        local.0.ping().await.unwrap();
    }

    async fn exercise_execution_mutation(
        client: &mut bmux_client::BmuxClient,
        local: &mut CheckedLocal<'_>,
        subject: uuid::Uuid,
    ) {
        use bmux_pane_runtime_plugin_api::pane_runtime_execution_v4::{
            TerminalProgramOptions, client as execution,
        };
        use bmux_permissions_plugin_api::permissions_commands::client as commands;
        let program = std::env::split_paths(&std::env::var_os("PATH").expect("test PATH"))
            .map(|directory| directory.join("cat"))
            .find(|path| path.is_file())
            .expect("cat from the Nix test environment");
        let options = || TerminalProgramOptions {
            program: program.to_string_lossy().into_owned(),
            args: vec![],
            cwd: None,
            env: std::collections::BTreeMap::new(),
            cols: 80,
            rows: 24,
        };
        let denied = execution::new_program_session(&mut CheckedLocal(client), None, options())
            .await
            .unwrap();
        assert!(denied.is_err());
        commands::grant_resource(local, subject, None, "writer".into())
            .await
            .unwrap();
        let execution = execution::new_program_session(&mut CheckedLocal(client), None, options())
            .await
            .unwrap()
            .expect("explicit writer grant permits real PTY creation");
        assert!(
            execution::send(
                &mut CheckedLocal(client),
                execution.clone(),
                b"admitted\n".to_vec()
            )
            .await
            .unwrap()
            .is_ok()
        );
        commands::revoke_resource(local, subject, None)
            .await
            .unwrap();
        assert!(
            execution::send(
                &mut CheckedLocal(client),
                execution.clone(),
                b"denied\n".to_vec()
            )
            .await
            .unwrap()
            .is_err()
        );
        commands::grant_resource(local, subject, None, "writer".into())
            .await
            .unwrap();
        let terminated = execution::terminate(&mut CheckedLocal(client), execution, 0)
            .await
            .unwrap();
        assert!(terminated.is_ok(), "real PTY terminate: {terminated:?}");
        commands::revoke_resource(local, subject, None)
            .await
            .unwrap();
    }

    fn provider_route(
        interface: &str,
        operation: &str,
        kind: InvokeServiceKind,
    ) -> bmux_server::ServiceRoute {
        bmux_server::ServiceRoute {
            capability: "bmux.connections.pairing".into(),
            kind,
            interface_id: interface.into(),
            operation: operation.into(),
        }
    }

    fn install_provider_fixture(
        server: &bmux_server::BmuxServer,
        authority: &Arc<AdmissionAuthority>,
    ) {
        use bmux_connections_plugin_api::{
            admission_provider_commands as commands, admission_provider_state as state_api,
            pairing_types::AdmissionBinding,
        };
        use bmux_plugin::stream_admission::{StreamAdmissionAuthority, StreamAdmissionBinding};
        use bmux_plugin_sdk::{decode_service_message, encode_service_message, service_contract};
        use bmux_server::StreamAdmissionProviderRoutes;
        for (interface, operation, kind, contract) in [
            (
                "admission-provider-state",
                "probe",
                InvokeServiceKind::Query,
                state_api::SERVICE_CONTRACT,
            ),
            (
                "admission-provider-state",
                "validate",
                InvokeServiceKind::Query,
                state_api::SERVICE_CONTRACT,
            ),
            (
                "admission-provider-commands",
                "consume",
                InvokeServiceKind::Command,
                commands::SERVICE_CONTRACT,
            ),
        ] {
            let authority = Arc::clone(authority);
            server
                .register_checked_service_handler_with_metadata(
                    "bmux.connections.pairing",
                    kind,
                    interface,
                    operation,
                    move |_, _, payload| {
                        let authority = Arc::clone(&authority);
                        async move {
                            let bytes =
                                service_contract::decode_for(interface, contract, &payload)?;
                            let encoded = match operation {
                                "probe" => encode_service_message(&authority.instance())?,
                                "consume" => {
                                    let request: commands::client::ConsumeRequest =
                                        decode_service_message(bytes)?;
                                    let binding = authority.consume(&request.ticket)?;
                                    encode_service_message(&AdmissionBinding {
                                        subject_id: binding.subject_id(),
                                        lease_id: binding.lease_id(),
                                    })?
                                }
                                _ => {
                                    let request: state_api::client::ValidateRequest =
                                        decode_service_message(bytes)?;
                                    encode_service_message(
                                        &authority
                                            .validate(&StreamAdmissionBinding::new(
                                                request.subject_id,
                                                request.lease_id,
                                            ))
                                            .is_ok(),
                                    )?
                                }
                            };
                            Ok(service_contract::encode_response(&encoded).into())
                        }
                    },
                )
                .unwrap();
        }
        assert_eq!(
            server
                .install_stream_admission_provider(
                    StreamAdmissionProviderRoutes {
                        probe: provider_route(
                            "admission-provider-state",
                            "probe",
                            InvokeServiceKind::Query
                        ),
                        consume: provider_route(
                            "admission-provider-commands",
                            "consume",
                            InvokeServiceKind::Command
                        ),
                        validate: provider_route(
                            "admission-provider-state",
                            "validate",
                            InvokeServiceKind::Query
                        ),
                    },
                    Arc::new(crate::admission::AdmissionProviderCodec),
                    Duration::from_secs(2),
                    8
                )
                .unwrap(),
            authority.instance()
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn real_transport_admits_only_paired_device_and_backend_subject() {
        use bmux_server::BmuxServer;
        let root = tempfile::tempdir().unwrap();
        let paths = bmux_config::ConfigPaths::new(
            root.path().join("config"),
            root.path().join("runtime"),
            root.path().join("data"),
            root.path().join("state"),
        );
        std::fs::create_dir_all(&paths.state_dir).unwrap();
        std::fs::create_dir_all(&paths.runtime_dir).unwrap();
        let host = RuntimeIdentity::load_or_create(&paths.state_dir, &paths.runtime_dir).unwrap();
        let identity =
            DeviceIdentity::load_or_create(&paths.state_dir, &paths.runtime_dir).unwrap();
        let pairing = Arc::new(
            PairingAuthority::open(&paths.state_dir, &paths.runtime_dir, host.endpoint_id())
                .unwrap(),
        );
        let owner = LocalOwner::from_trusted_local_owner();
        let invitation = pairing
            .create_invitation(
                &owner,
                &identity.public_key(),
                60,
                DeviceAccess::FullLocalRuntime,
            )
            .unwrap();
        let authority = Arc::new(AdmissionAuthority::new(Arc::clone(&pairing)));
        let backend = bmux_ipc::IpcEndpoint::unix_socket(paths.server_socket());
        let server = Arc::new(BmuxServer::new(backend.clone()));
        install_provider_fixture(&server, &authority);
        install_resource_fixture(&server, &paths, &pairing);
        activate_execution_fixture(&paths);
        bmux_plugin::global_service_locations().mark_remote("bmux.permissions");
        let running = Arc::clone(&server);
        let task = tokio::spawn(async move { running.run().await });
        let mut listener = Listener::start(
            direct(host.endpoint_builder()),
            Arc::clone(&pairing),
            authority,
            backend,
        )
        .await
        .unwrap();
        let address = listener.status().ip_addrs().next().copied().unwrap();
        let target = format!("device://{}?addr={address}", host.endpoint_id());
        let config = bmux_config::BmuxConfig::default();
        let target = crate::target::resolve_internal(&config, &target).unwrap();
        assert!(
            crate::transport::connect(&config, &paths, &target)
                .await
                .is_err()
        );
        let crate::target::ResolvedTarget::Iroh(ref device_target) = target else {
            panic!("device target")
        };
        assert_eq!(
            crate::transport::pair_endpoint(&paths, device_target, &invitation)
                .await
                .unwrap(),
            device_target.reference
        );
        // Re-open the private durable map and device key on every connection.
        let device = pairing.list_devices(&owner).unwrap().remove(0);
        let mut client = crate::transport::connect(&config, &paths, &target)
            .await
            .expect("generated proof admission and trusted-subject Hello");
        assert_eq!(client.principal_id().to_string(), device.device_id);
        exercise_resource_policy(&mut client, &paths, &device.device_id).await;
        // Unmigrated kernel requests deliberately close admitted streams.
        assert!(client.whoami_principal().await.is_err());
        drop(client);
        let spoof = crate::target::resolve_internal(
            &config,
            &format!(
                "iroh://{}?addr={address}&device-id={}",
                host.endpoint_id(),
                uuid::Uuid::new_v4()
            ),
        )
        .unwrap();
        assert!(
            crate::transport::connect(&config, &paths, &spoof)
                .await
                .is_err()
        );
        exercise_idle_lease_revoke(&identity, &listener, &pairing, &owner, &device.device_id).await;
        assert!(
            crate::transport::connect(&config, &paths, &target)
                .await
                .is_err()
        );
        listener.stop().await;
        server.request_shutdown();
        task.await.unwrap().unwrap();
    }

    async fn exercise_idle_lease_revoke(
        identity: &DeviceIdentity,
        listener: &Listener,
        pairing: &PairingAuthority,
        owner: &LocalOwner,
        device_id: &str,
    ) {
        let idle_endpoint = direct(identity.endpoint_builder())
            .alpns(vec![alpn().to_vec()])
            .bind()
            .await
            .unwrap();
        let idle_connection = tokio::time::timeout(
            Duration::from_secs(5),
            idle_endpoint.connect(listener.status(), alpn()),
        )
        .await
        .unwrap()
        .unwrap();
        let (mut idle_stream, idle_subject) = admit(
            &idle_endpoint,
            &idle_connection,
            identity,
            device_id,
            None,
            Duration::from_secs(5),
        )
        .await
        .unwrap();
        assert_eq!(idle_subject.to_string(), device_id);
        // A successful Hello proves backend consumption before lease revocation.
        idle_stream
            .send_envelope(&Envelope::new(
                100,
                EnvelopeKind::Request,
                bmux_ipc::encode(&Request::Hello {
                    contract: bmux_ipc::ProtocolContract::current(
                        bmux_ipc::default_supported_capabilities(),
                    ),
                    client_name: "idle-lease-regression".into(),
                    principal_id: idle_subject,
                })
                .unwrap(),
            ))
            .await
            .unwrap();
        let hello = tokio::time::timeout(Duration::from_secs(5), idle_stream.recv_envelope())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(
            bmux_ipc::decode::<Response>(&hello.payload).unwrap(),
            Response::Ok(ResponsePayload::HelloNegotiated { .. })
        ));
        pairing.revoke(owner, device_id).unwrap();
        assert!(
            tokio::time::timeout(Duration::from_secs(5), idle_stream.recv_envelope())
                .await
                .expect("revocation closes idle admitted stream without another request")
                .is_err()
        );
    }

    #[tokio::test]
    async fn mismatched_peer_cannot_consume_invitation() {
        let root = tempfile::tempdir().unwrap();
        let state = root.path().join("state");
        let runtime = root.path().join("runtime");
        std::fs::create_dir_all(&state).unwrap();
        std::fs::create_dir_all(&runtime).unwrap();
        let host = RuntimeIdentity::load_or_create(&state, &runtime).unwrap();
        let device = DeviceIdentity::load_or_create(&state, &runtime).unwrap();
        let pairing =
            Arc::new(PairingAuthority::open(&state, &runtime, host.endpoint_id()).unwrap());
        let invitation = pairing
            .create_invitation(
                &LocalOwner::from_trusted_local_owner(),
                &device.public_key(),
                60,
                DeviceAccess::FullLocalRuntime,
            )
            .unwrap();
        let admission = Arc::new(AdmissionAuthority::new(Arc::clone(&pairing)));
        let mut listener = Listener::start(
            direct(host.endpoint_builder()),
            Arc::clone(&pairing),
            admission,
            bmux_ipc::IpcEndpoint::unix_socket(runtime.join("absent.sock")),
        )
        .await
        .unwrap();
        let endpoint = direct(iroh::Endpoint::builder(iroh::endpoint::presets::N0))
            .alpns(vec![alpn().to_vec()])
            .bind()
            .await
            .unwrap();
        let connection = endpoint.connect(listener.status(), alpn()).await.unwrap();
        let (send, receive) = connection.open_bi().await.unwrap();
        let mut client = PreAdmission {
            stream: tokio::io::join(receive, send),
            request_id: 0,
        };
        let request = requests::RedeemInvitationRequest {
            invitation_id: invitation.invitation_id.clone(),
            secret: invitation.secret.clone(),
            signature: device
                .sign(&invitation.possession_message)
                .to_bytes()
                .to_vec(),
        };
        let rejected = tokio::time::timeout(
            Duration::from_secs(2),
            requests::redeem_invitation(
                &mut client,
                request.invitation_id.clone(),
                request.secret.clone(),
                request.signature.clone(),
            ),
        )
        .await
        .unwrap();
        assert!(rejected.is_err());
        assert!(
            pairing
                .list_devices(&LocalOwner::from_trusted_local_owner())
                .unwrap()
                .is_empty()
        );
        // The valid peer can still redeem that same invitation after rejection.
        let valid = pairing
            .redeem(&request.invitation_id, &request.secret, &request.signature)
            .unwrap();
        assert_eq!(valid.device_key, device.public_key());
        listener.stop().await;
        endpoint.close().await;
    }
}
