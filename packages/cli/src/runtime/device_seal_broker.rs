use anyhow::{Context, Result};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

use super::{ConnectionContext, ConnectionPolicyScope, connect_with_context};

pub(super) async fn run_device_seal_broker(
    connection_context: ConnectionContext<'_>,
) -> Result<u8> {
    let mut input = Vec::new();
    tokio::io::stdin()
        .read_to_end(&mut input)
        .await
        .context("failed to read device-seal broker request from stdin")?;
    let mut client = connect_with_context(
        ConnectionPolicyScope::Normal,
        "bmux-device-seal-broker",
        connection_context,
    )
    .await?;
    let output = client.device_seal_broker(input).await?;
    tokio::io::stdout()
        .write_all(&output)
        .await
        .context("failed to write device-seal broker response")?;
    Ok(0)
}
