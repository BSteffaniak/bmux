//! Connection-owned preparation fencing for native window initialization.
#[cfg(test)]
mod tests;
use anyhow::{Context as _, Result};
use bmux_pane_runtime_plugin_api::pane_runtime_readiness_v1::{ManagedPreparationLease, client};

pub struct NativePreparation {
    task: tokio::task::JoinHandle<Result<()>>,
    committed: Option<tokio::sync::oneshot::Sender<()>>,
    abandoned: Option<tokio::sync::oneshot::Sender<()>>,
}

impl NativePreparation {
    pub(super) fn start(
        mut connection: bmux_client::BmuxClient,
        lease: ManagedPreparationLease,
    ) -> Self {
        let (committed, mut receiver) = tokio::sync::oneshot::channel();
        let (abandoned, mut abandonment) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            anyhow::ensure!(
                lease.ttl_seconds >= 3,
                "managed preparation lease TTL is too short"
            );
            let renewal = std::time::Duration::from_secs(lease.ttl_seconds / 3);
            loop {
                tokio::select! {
                    committed = async {
                        tokio::select! {
                            commit = &mut receiver => commit.is_ok(),
                            _ = &mut abandonment => false,
                        }
                    } => {
                        // Cancellation is abandonment, not permission to replay attach.
                        let release = tokio::time::timeout(std::time::Duration::from_secs(5),
                            client::release_managed_preparation(&mut connection, lease.instance, lease.lease_id))
                            .await.context("release native preparation timed out")??;
                        release.map_err(anyhow::Error::msg)?;
                        anyhow::ensure!(committed, "native preparation abandoned");
                        return Ok(());
                    }
                    () = tokio::time::sleep(renewal) => {
                        let renewed = tokio::time::timeout(std::time::Duration::from_secs(5),
                            client::renew_managed_preparation(&mut connection, lease.instance, lease.lease_id))
                            .await.context("renew native preparation timed out")??.map_err(anyhow::Error::msg)?;
                        anyhow::ensure!(renewed.instance == lease.instance && renewed.lease_id == lease.lease_id
                            && renewed.ttl_seconds == lease.ttl_seconds, "native preparation lease changed during renewal");
                    }
                }
            }
        });
        Self {
            task,
            committed: Some(committed),
            abandoned: Some(abandoned),
        }
    }

    pub fn commit_callback(&mut self) -> impl FnOnce() + Send + 'static {
        let sender = self.committed.take();
        move || {
            if let Some(sender) = sender {
                let _ = sender.send(());
            }
        }
    }

    /// Poll attachment once. A lost fence cancels this future, never retries it.
    pub async fn run<F, T>(mut self, attach: F) -> Result<T>
    where
        F: std::future::Future<Output = Result<T>>,
    {
        tokio::pin!(attach);
        tokio::select! {
            biased;
            lease = &mut self.task => {
                lease.context("native preparation task failed")??;
                attach.await
            }
            result = &mut attach => result,
        }
    }
}

impl Drop for NativePreparation {
    fn drop(&mut self) {
        // The task releases on its original connection with a five-second
        // bound, then closes it. TTL is the fallback for lost transport.
        // Dropping JoinHandle detaches this bounded release, never kills work.
        if let Some(abandoned) = self.abandoned.take() {
            let _ = abandoned.send(());
        }
    }
}
