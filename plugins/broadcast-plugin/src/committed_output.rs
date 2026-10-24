//! Authorization for an existing committed-view connection; never opens a client.
use bmux_broadcast_plugin_api::broadcast_control_v1 as control;
use bmux_client::StreamingBmuxClient;
use std::path::Path;

/// Authorize output on the connection that owns the committed view.
///
/// # Errors
/// Returns a public-safe error if the selected runtime proof or service is unavailable.
pub async fn authorize_committed_output(
    client: &mut StreamingBmuxClient,
    runtime_dir: &Path,
) -> Result<(), String> {
    control::client::prepare_local(client)
        .await
        .map_err(|_| "broadcast local authorization unavailable".to_owned())?;
    let proof = crate::authorization::local_proof(runtime_dir).map_err(str::to_owned)?;
    if !control::client::authorize(client, proof)
        .await
        .map_err(|_| "broadcast local authorization rejected".to_owned())?
    {
        return Err("broadcast local authorization rejected".into());
    }
    Ok(())
}
