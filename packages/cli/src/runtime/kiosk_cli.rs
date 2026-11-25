use super::attach::runtime::{
    ClientAttachOptions, ReconnectPrepare, run_session_attach_with_client_options,
};
use super::{ConnectionContext, connect_with_context, map_cli_client_error};
use crate::connection::ConnectionPolicyScope;
use crate::pane_runtime_client::BmuxPaneRuntimeClientExt;
use anyhow::Result;
use bmux_client::BmuxClient;
use std::sync::Arc;

/// Attach with an authorized kiosk grant. A connector-backed target resumes
/// after a dropped connection; every replacement client gets the kiosk attach
/// policy before it attaches, and a client whose policy cannot be applied is
/// never attached.
pub(super) async fn attach_authorized(
    grant: bmux_connections_admin::kiosk::AuthorizedAttach,
) -> Result<u8> {
    let allow_detach = grant.allow_detach;
    let client = connect_with_context(
        ConnectionPolicyScope::Normal,
        "bmux-cli-kiosk-attach",
        ConnectionContext::new(grant.target.as_deref()),
    )
    .await?;
    let client = apply_kiosk_policy(client, allow_detach).await?;
    let prepare: ReconnectPrepare =
        Arc::new(move |client| Box::pin(apply_kiosk_policy(client, allow_detach)));
    Box::pin(run_session_attach_with_client_options(
        client,
        grant.session.as_deref(),
        None,
        false,
        ClientAttachOptions {
            prepare: Some(prepare),
            ..ClientAttachOptions::default()
        },
    ))
    .await
    .map(|outcome| outcome.status_code)
}

async fn apply_kiosk_policy(mut client: BmuxClient, allow_detach: bool) -> Result<BmuxClient> {
    client
        .set_attach_policy(allow_detach)
        .await
        .map_err(map_cli_client_error)?;
    Ok(client)
}
