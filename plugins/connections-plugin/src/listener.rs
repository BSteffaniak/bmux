//! Plugin-owned admitted ingress. No generic IPC is forwarded before admission.
use crate::{admission::AdmissionAuthority, pairing::PairingAuthority};
use anyhow::{Context, Result, bail, ensure};
use bmux_connections_plugin_api::pairing_commands::{self, client as requests};
use bmux_ipc::{Envelope, EnvelopeKind, IpcEndpoint, Request, Response, ResponsePayload};
use bmux_plugin_sdk::{decode_service_message, encode_service_message, service_contract};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::task::{JoinHandle, JoinSet};

pub const ALPN: &[u8] = b"bmux/connections/admission/1";
const PREAUTH_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_PREAUTH_FRAME: usize = 64 * 1024;

pub struct Listener {
    endpoint: iroh::Endpoint,
    task: JoinHandle<()>,
}

impl Listener {
    /// Caller must establish local-owner policy and register this exact admission
    /// authority with the backend first. Construction does not grant authority.
    pub async fn start(
        builder: iroh::endpoint::Builder,
        pairing: Arc<PairingAuthority>,
        admission: Arc<AdmissionAuthority>,
        backend: IpcEndpoint,
    ) -> Result<Self> {
        let endpoint = builder.alpns(vec![ALPN.to_vec()]).bind().await?;
        ensure!(
            endpoint.id().to_string() == pairing.endpoint(),
            "pairing listener identity mismatch"
        );
        let accepting = endpoint.clone();
        let task = tokio::spawn(async move {
            let mut connections = JoinSet::new();
            loop {
                tokio::select! {
                    incoming = accepting.accept(), if connections.len() < 64 => {
                        let Some(incoming) = incoming else { break };
                        let pairing = Arc::clone(&pairing);
                        let admission = Arc::clone(&admission);
                        let backend = backend.clone();
                        connections.spawn(async move {
                            let Ok(Ok(connection)) = tokio::time::timeout(PREAUTH_TIMEOUT, incoming).await else { return };
                            let mut streams = JoinSet::new();
                            loop {
                                tokio::select! {
                                    stream = tokio::time::timeout(Duration::from_secs(15), connection.accept_bi()), if streams.len() < 16 => {
                                        let (send, receive) = match stream {
                                            Ok(Ok(stream)) => stream,
                                            Err(_) if !streams.is_empty() => continue,
                                            _ => { connection.close(0u32.into(), b"idle pairing connection"); break; }
                                        };
                                        let pairing = Arc::clone(&pairing);
                                        let admission = Arc::clone(&admission);
                                        let backend = backend.clone();
                                        let peer = connection.remote_id().to_string();
                                        streams.spawn(async move {
                                            // Never log credential-bearing wire values or decoder errors.
                                            let _ = serve_stream(send, receive, &peer, &pairing, &admission, &backend).await;
                                        });
                                    }
                                    _ = streams.join_next(), if !streams.is_empty() => {}
                                }
                            }
                        });
                    }
                    _ = connections.join_next(), if !connections.is_empty() => {}
                }
            }
        });
        Ok(Self { endpoint, task })
    }

    pub fn targets(&self) -> Vec<String> {
        let address = self.status();
        let mut targets: Vec<_> = address
            .ip_addrs()
            .map(|addr| format!("device://{}?addr={addr}", address.id))
            .collect();
        targets.extend(
            address
                .relay_urls()
                .map(|url| format!("device://{}?relay={url}", address.id)),
        );
        targets.sort();
        targets.dedup();
        targets
    }

    pub fn status(&self) -> iroh::EndpointAddr {
        self.endpoint.addr()
    }

    pub async fn stop(&mut self) {
        self.task.abort();
        let _ = (&mut self.task).await;
        self.endpoint.close().await;
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn serve_stream(
    mut send: iroh::endpoint::SendStream,
    mut receive: iroh::endpoint::RecvStream,
    peer: &str,
    pairing: &PairingAuthority,
    admission: &AdmissionAuthority,
    backend: &IpcEndpoint,
) -> Result<()> {
    // Proofs and tickets are scoped to this authenticated transport stream.
    let preamble = tokio::time::timeout(PREAUTH_TIMEOUT, async {
        let mut challenge = None;
        let mut ticket = None;
        for _ in 0..8 {
            let envelope = read_envelope(&mut receive).await?;
            ensure!(envelope.kind == EnvelopeKind::Request, "expected request");
            let request: Request = bmux_ipc::decode(&envelope.payload)?;
            if let Request::InvokeService {
                capability,
                kind: bmux_ipc::InvokeServiceKind::Command,
                interface_id,
                operation,
                payload,
            } = &request
                && capability == "bmux.connections.pairing"
                && interface_id
                    == bmux_connections_plugin_api::pairing_peer_commands::INTERFACE_ID.as_str()
            {
                let response = pairing_response(pairing, peer, interface_id, operation, payload)?;
                write_response(&mut send, envelope.request_id, &response).await?;
                // One short operation per stream. Approval polling cannot hold
                // a host command or the admission handshake open.
                return Ok(None);
            }
            if let Request::AdmitStream {
                admission_version: 1,
                ticket: presented,
            } = request
            {
                return admitted_preamble(ticket.as_deref(), &presented, envelope);
            }
            let Request::InvokeService {
                capability,
                kind: bmux_ipc::InvokeServiceKind::Command,
                interface_id,
                operation,
                payload,
            } = request
            else {
                bail!("pre-admission operation denied");
            };
            ensure!(
                capability == "bmux.connections.pairing"
                    && interface_id == pairing_commands::INTERFACE_ID.as_str(),
                "pre-admission service denied"
            );
            let payload = service_contract::decode_for(
                &interface_id,
                pairing_commands::SERVICE_CONTRACT,
                &payload,
            )?;
            let result = match operation.as_str() {
                "redeem-invitation" => {
                    let request: requests::RedeemInvitationRequest =
                        decode_service_message(payload)?;
                    let device = pairing.redeem_for_peer(
                        &request.invitation_id,
                        &request.secret,
                        &request.signature,
                        peer,
                    )?;
                    ensure!(device.device_key == peer, "device transport mismatch");
                    encode_service_message(&device)?
                }
                "begin-admission" => {
                    let request: requests::BeginAdmissionRequest = decode_service_message(payload)?;
                    let device = pairing.authorized_device(request.device_id.parse()?)?;
                    ensure!(device.device_key == peer, "device transport mismatch");
                    let proof = admission.begin(&request.device_id)?;
                    challenge = Some(proof.challenge.clone());
                    encode_service_message(&proof)?
                }
                "finish-admission" => {
                    let request: requests::FinishAdmissionRequest =
                        decode_service_message(payload)?;
                    ensure!(
                        challenge.take().as_ref() == Some(&request.challenge),
                        "challenge not issued on this stream"
                    );
                    let issued = admission.finish(&request.challenge, &request.signature)?;
                    ticket = Some(issued.clone());
                    encode_service_message(&issued)?
                }
                _ => bail!("pre-admission operation denied"),
            };
            let response = Response::Ok(ResponsePayload::ServiceInvoked {
                payload: service_contract::encode_response(&result),
            });
            write_response(&mut send, envelope.request_id, &response).await?;
        }
        bail!("pre-admission operation limit exceeded")
    })
    .await
    .context("pre-admission deadline")??;
    if let Some(preamble) = preamble {
        bridge_admitted(send, receive, backend, &preamble).await?;
    }
    Ok(())
}

fn admitted_preamble(
    ticket: Option<&[u8]>,
    presented: &[u8],
    envelope: Envelope,
) -> Result<Option<Envelope>> {
    ensure!(
        ticket == Some(presented),
        "ticket not issued on this stream"
    );
    Ok(Some(envelope))
}

async fn write_response(
    send: &mut iroh::endpoint::SendStream,
    request_id: u64,
    response: &Response,
) -> Result<()> {
    write_envelope(
        send,
        &Envelope::new(
            request_id,
            EnvelopeKind::Response,
            bmux_ipc::encode(response)?,
        ),
    )
    .await
}

fn pairing_response(
    pairing: &PairingAuthority,
    peer: &str,
    interface_id: &str,
    operation: &str,
    payload: &[u8],
) -> Result<Response> {
    use bmux_connections_plugin_api::pairing_peer_commands::{
        self as api, client as peer_requests,
    };
    let payload = service_contract::decode_for(interface_id, api::SERVICE_CONTRACT, payload)?;
    let result = match operation {
        "submit-pairing" => {
            let request: peer_requests::SubmitPairingRequest = decode_service_message(payload)?;
            encode_service_message(&pairing.sessions.submit_named(
                &request.session_id,
                peer,
                &request.nonce,
                &request.signature,
                request.device_name.as_deref(),
            )?)?
        }
        "poll-pairing" => {
            let request: peer_requests::PollPairingRequest = decode_service_message(payload)?;
            encode_service_message(&pairing.sessions.poll(
                &request.session_id,
                peer,
                &request.nonce,
            )?)?
        }
        "pairing-result" => {
            let request: peer_requests::PairingResultRequest = decode_service_message(payload)?;
            encode_service_message(&pairing.sessions.result(
                pairing,
                &request.session_id,
                peer,
                &request.nonce,
            )?)?
        }
        _ => bail!("pre-admission operation denied"),
    };
    Ok(Response::Ok(ResponsePayload::ServiceInvoked {
        payload: service_contract::encode_response(&result),
    }))
}

async fn bridge_admitted(
    mut send: iroh::endpoint::SendStream,
    mut receive: iroh::endpoint::RecvStream,
    backend: &IpcEndpoint,
    preamble: &Envelope,
) -> Result<()> {
    // The authoritative backend consumes the opaque single-use ticket and
    // enforces Hello subject equality plus revocation/resource checks. Never
    // synthesize Hello, principal IDs, or a fallback unadmitted connection.
    let mut local = bmux_ipc::transport::LocalIpcStream::connect(backend).await?;
    local.send_envelope(preamble).await?;
    let (mut read, mut write) = tokio::io::split(local);
    tokio::try_join!(
        async {
            tokio::io::copy(&mut receive, &mut write).await?;
            write.shutdown().await
        },
        async {
            tokio::io::copy(&mut read, &mut send).await?;
            send.shutdown().await
        },
    )?;
    Ok(())
}

/// Strictly bounded uncompressed framing, shared by this one private workflow.
pub async fn read_envelope(reader: &mut (impl AsyncRead + Unpin)) -> Result<Envelope> {
    let length = reader.read_u32_le().await?;
    let length = usize::try_from(length)?;
    ensure!(length <= MAX_PREAUTH_FRAME, "pre-admission frame too large");
    let mut frame = Vec::with_capacity(length + 4);
    frame.extend_from_slice(&u32::try_from(length)?.to_le_bytes());
    frame.resize(length + 4, 0);
    reader.read_exact(&mut frame[4..]).await?;
    Ok(bmux_ipc::frame::decode_frame_exact(&frame)?)
}

pub async fn write_envelope(
    writer: &mut (impl AsyncWrite + Unpin),
    envelope: &Envelope,
) -> Result<()> {
    let frame = bmux_ipc::frame::encode_frame(envelope)?;
    ensure!(
        frame.len() <= MAX_PREAUTH_FRAME + 4,
        "pre-admission frame too large"
    );
    writer.write_all(&frame).await?;
    writer.flush().await?;
    Ok(())
}
