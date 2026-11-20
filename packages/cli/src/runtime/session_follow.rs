use anyhow::{Context, Result};
use bmux_session_models::SessionSelector;
use uuid::Uuid;

#[cfg(feature = "bundled-plugin-sessions")]
use super::{ConnectionContext, ConnectionPolicyScope, connect_with_context, map_cli_client_error};
#[cfg(feature = "bundled-plugin-sessions")]
use crate::pane_runtime_client::BmuxPaneRuntimeClientExt;

#[cfg(feature = "bundled-plugin-sessions")]
pub(super) async fn run_session_detach(connection_context: ConnectionContext<'_>) -> Result<u8> {
    let mut client = connect_with_context(
        ConnectionPolicyScope::Normal,
        "bmux-cli-detach",
        connection_context,
    )
    .await?;
    client.detach().await.map_err(map_cli_client_error)?;
    println!("detached");
    Ok(0)
}

pub(super) fn parse_session_selector(target: &str) -> SessionSelector {
    Uuid::parse_str(target).map_or_else(
        |_| SessionSelector::ByName(target.to_string()),
        SessionSelector::ById,
    )
}

pub(super) fn parse_uuid_value(value: &str, label: &str) -> Result<Uuid> {
    Uuid::parse_str(value).with_context(|| format!("{label} must be a UUID, got '{value}'"))
}
