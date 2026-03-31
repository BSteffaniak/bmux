use super::*;

pub(super) async fn run_session_detach() -> Result<u8> {
    let mut client = connect(ConnectionPolicyScope::Normal, "bmux-cli-detach").await?;
    client.detach().await.map_err(map_cli_client_error)?;
    println!("detached");
    Ok(0)
}

pub(super) async fn run_follow(target_client_id: &str, global: bool) -> Result<u8> {
    let target_client_id = parse_uuid_value(target_client_id, "target client id")?;
    let mut client = connect(ConnectionPolicyScope::Normal, "bmux-cli-follow").await?;
    client
        .follow_client(target_client_id, global)
        .await
        .map_err(map_cli_client_error)?;
    println!(
        "following client: {}{}",
        target_client_id,
        if global { " (global)" } else { "" }
    );
    Ok(0)
}

pub(super) async fn run_unfollow() -> Result<u8> {
    let mut client = connect(ConnectionPolicyScope::Normal, "bmux-cli-unfollow").await?;
    client.unfollow().await.map_err(map_cli_client_error)?;
    println!("follow stopped");
    Ok(0)
}

pub(super) fn parse_session_selector(target: &str) -> SessionSelector {
    match Uuid::parse_str(target) {
        Ok(id) => SessionSelector::ById(id),
        Err(_) => SessionSelector::ByName(target.to_string()),
    }
}

pub(super) fn parse_uuid_value(value: &str, label: &str) -> Result<Uuid> {
    Uuid::parse_str(value).with_context(|| format!("{label} must be a UUID, got '{value}'"))
}
