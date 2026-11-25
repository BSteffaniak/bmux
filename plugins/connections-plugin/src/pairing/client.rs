//! The connecting side of `bmux/pair/1`.
use super::protocol::{self, DeviceMessage, HostMessage};
use crate::link::PairingLink;
use crate::transport::manager::{ConnectionManager, HostRoute, refusal};
use crate::transport::PAIR_ALPN;
use crate::trust::Role;
use anyhow::{Result, bail};
use std::time::Duration;
use uuid::Uuid;

const DIAL_TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Paired {
    pub principal: Uuid,
    pub host_name: String,
}

/// Prove the link secret, show the phrase through `show_phrase`, and wait
/// for the owner's decision (pushed by the host; no polling).
pub async fn pair(
    manager: &ConnectionManager,
    device_key: iroh::PublicKey,
    link: &PairingLink,
    device_name: &str,
    role: Role,
    show_phrase: impl FnOnce(&str) + Send,
) -> Result<Paired> {
    let route = HostRoute {
        host_key: link.host_key,
        addrs: link.addrs.clone(),
        relay: link.relay.clone(),
    };
    let connection = manager.dial(&route, PAIR_ALPN, DIAL_TIMEOUT).await?;
    let transcript = protocol::transcript(&link.host_key, &device_key, &link.secret, role);
    let hello = DeviceMessage::Hello {
        version: protocol::VERSION,
        role,
        device_name: protocol::sanitize_name(device_name),
        proof: protocol::hex(&protocol::proof(&link.secret, &transcript)),
    };
    let expected_phrase = protocol::phrase(&link.secret, &transcript);
    let exchange = async {
        let (mut send, mut recv) = connection.open_bi().await?;
        protocol::write_frame(&mut send, &hello).await?;
        let first: HostMessage = protocol::read_frame(&mut recv).await?;
        Ok::<_, anyhow::Error>((send, recv, first))
    };
    let (send, mut recv, first) = match exchange.await {
        Ok(result) => result,
        Err(error) => {
            if let Some(reason) = refusal(&connection) {
                bail!(reason);
            }
            return Err(error.context("pairing exchange failed"));
        }
    };
    match first {
        HostMessage::Pending { phrase } => {
            if phrase != expected_phrase {
                connection.close(0u32.into(), b"phrase mismatch");
                bail!("the host answered with a different phrase; pairing stopped");
            }
            show_phrase(&phrase);
        }
        HostMessage::Refused { reason } => bail!("pairing refused: {}", reason.describe()),
        HostMessage::Approved { .. } => bail!("unexpected pairing message"),
    }
    let outcome = {
        let _send = send;
        protocol::read_frame::<HostMessage>(&mut recv).await
    };
    connection.close(0u32.into(), b"done");
    match outcome? {
        HostMessage::Approved {
            principal,
            host_name,
        } => Ok(Paired {
            principal,
            host_name,
        }),
        HostMessage::Refused { reason } => bail!("pairing refused: {}", reason.describe()),
        HostMessage::Pending { .. } => bail!("unexpected pairing message"),
    }
}
