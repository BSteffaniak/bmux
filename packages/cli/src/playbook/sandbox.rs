//! CLI acquisition adapter for reusable execution lifecycle.
pub use bmux_playbook::sandbox::SandboxStartOptions;
pub struct SandboxServer(bmux_playbook::sandbox::SandboxServer);
impl SandboxServer {
    /// Acquire an isolated sandbox using CLI process-selection adapters.
    /// # Errors
    /// Returns configuration, process startup or connection errors.
    pub async fn start(options: SandboxStartOptions<'_>) -> anyhow::Result<Self> {
        Ok(Self(
            bmux_playbook::sandbox::SandboxServer::start(options, super::engine::host()).await?,
        ))
    }
    /// Stop the owned sandbox and apply the requested retention policy.
    /// # Errors
    /// Returns an error when process termination or cleanup is unacknowledged.
    pub async fn shutdown(self, retain: bool) -> anyhow::Result<()> {
        self.0.shutdown(retain).await
    }
}
impl std::ops::Deref for SandboxServer {
    type Target = bmux_playbook::sandbox::SandboxServer;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
