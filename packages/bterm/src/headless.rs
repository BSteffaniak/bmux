//! Explicit headless managed-connect diagnostics using the native launch path.
//!
//! This is not an alternate frontend identity or a test server. It runs inside
//! the verified bundled Bterm executable, admits no terminal execution, and drops
//! its authenticated client after producing a deliberately bounded report.

use std::time::Duration;

use anyhow::{Context as _, Result, ensure};
use serde::Serialize;

use crate::configuration::ResolvedConfiguration;
use crate::runtime_paths::RuntimeRoots;
use crate::runtime_selection::{ManagedTargetIdentity, RuntimeSelection};

/// Safe live observations, never configuration contents, credentials, arbitrary
/// server errors, process environment, or executable discovery candidates.
#[derive(Debug, Serialize)]
pub struct ManagedConnectionReport {
    pub schema_version: u32,
    pub process_id: u32,
    pub instance: String,
    pub target_id: String,
    pub artifact_set_sha256: String,
    pub companion_sha256: String,
    pub configuration_id: String,
    pub scope_id: String,
    pub roots: RuntimeRoots,
    pub idle_managed: bool,
    pub idle_grace_seconds: u64,
    pub idle_blocked: bool,
}

/// Connect exactly as native startup would, but never create a surface or shell.
///
/// The caller resolves configuration and selection through the ordinary captured
/// input path, then serializes the returned report to its diagnostic output.
/// External/slot targets are intentionally rejected: a diagnostic must not turn
/// attach-only authority into managed startup or idle-policy authority.
///
/// # Errors
/// Bundle verification, authenticated startup, exact identity/readiness checks,
/// and generated service calls fail explicitly without choosing another target.
pub async fn run(
    selection: &RuntimeSelection,
    configuration: &ResolvedConfiguration,
) -> Result<ManagedConnectionReport> {
    let expected = selection
        .managed_identity()
        .context("managed-connect requires an explicitly selected managed target")?;
    ensure!(
        expected.configuration_id == configuration.runtime_configuration_id,
        "managed-connect configuration differs from selected target"
    );
    let mut client =
        crate::runtime_launch::connect_selection_configured(selection, configuration).await?;
    let readiness = tokio::time::timeout(
        Duration::from_secs(5),
        bmux_pane_runtime_plugin_api::pane_runtime_readiness_v1::client::runtime_readiness(
            &mut client,
        ),
    )
    .await
    .context("managed-connect live readiness timed out")??;
    let observed: ManagedTargetIdentity = serde_json::from_str(
        readiness
            .managed_target_identity
            .as_deref()
            .context("managed-connect runtime omitted managed identity")?,
    )
    .context("managed-connect runtime reported malformed managed identity")?;
    expected.verify_identity(&observed)?;
    ensure!(
        readiness.executable_sha256.as_deref() == Some(expected.companion_sha256.as_str()),
        "managed-connect executable identity mismatch"
    );
    ensure!(
        readiness.helper_ready && !readiness.fenced,
        "managed-connect runtime is not ready"
    );
    let roots = selection.roots();
    ensure!(
        readiness.runtime_dir == roots.runtime_dir.to_string_lossy()
            && readiness.config_dir == roots.config_dir.to_string_lossy()
            && readiness.data_dir == roots.data_dir.to_string_lossy()
            && readiness.state_dir == roots.state_dir.to_string_lossy(),
        "managed-connect live roots mismatch"
    );
    let idle = tokio::time::timeout(
        Duration::from_secs(5),
        bmux_pane_runtime_plugin_api::pane_runtime_readiness_v1::client::managed_idle_status(
            &mut client,
        ),
    )
    .await
    .context("managed-connect idle observation timed out")??;
    let idle = idle.map_err(|_| anyhow::anyhow!("managed-connect idle observation unavailable"))?;
    ensure!(
        idle.instance == readiness.instance,
        "managed-connect runtime instance changed"
    );
    ensure!(!idle.fenced, "managed-connect runtime is retiring");
    Ok(ManagedConnectionReport {
        schema_version: 1,
        process_id: readiness.process_id,
        instance: readiness.instance.to_string(),
        target_id: expected.target_id()?,
        artifact_set_sha256: expected.artifact_set_sha256.clone(),
        companion_sha256: expected.companion_sha256.clone(),
        configuration_id: expected.configuration_id.clone(),
        scope_id: expected.scope_id.clone(),
        roots: roots.clone(),
        idle_managed: idle.managed,
        idle_grace_seconds: idle.grace_seconds,
        idle_blocked: idle.blocker.is_some(),
    })
}
