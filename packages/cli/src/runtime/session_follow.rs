use anyhow::{Context, Result};
use bmux_ipc::SessionSelector;
use uuid::Uuid;

use super::{ConnectionContext, ConnectionPolicyScope, connect_with_context, map_cli_client_error};

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

pub(super) async fn run_follow(
    target_client_id: &str,
    global: bool,
    connection_context: ConnectionContext<'_>,
) -> Result<u8> {
    let target_client_id = parse_uuid_value(target_client_id, "target client id")?;
    let mut client = connect_with_context(
        ConnectionPolicyScope::Normal,
        "bmux-cli-follow",
        connection_context,
    )
    .await?;
    bmux_clients_plugin_api::typed_client::follow_client(&mut client, target_client_id, global)
        .await?;
    println!(
        "following client: {}{}",
        target_client_id,
        if global { " (global)" } else { "" }
    );
    Ok(0)
}

pub(super) async fn run_unfollow(connection_context: ConnectionContext<'_>) -> Result<u8> {
    let mut client = connect_with_context(
        ConnectionPolicyScope::Normal,
        "bmux-cli-unfollow",
        connection_context,
    )
    .await?;
    bmux_clients_plugin_api::typed_client::unfollow(&mut client).await?;
    println!("follow stopped");
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
