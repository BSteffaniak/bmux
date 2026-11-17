//! Attach-process mirror. Polling is asynchronous and never runs in rendering.
use bmux_broadcast_plugin_api::broadcast_presentation_v1 as presentation;
use bmux_config::ConfigPaths;
use std::time::Duration;
/// Owned by the invoking attach runtime; dropping removes the local indicator.
pub struct BroadcastPresentation {
    task: tokio::task::JoinHandle<()>,
    indicator: std::sync::Arc<crate::indicator::FactIndicator>,
}
impl BroadcastPresentation {
    /// Start with the explicitly selected runtime paths (never default
    /// discovery), publishing the indicator into the fact host of `states`,
    /// the invoking attach runtime's registry.
    ///
    /// # Errors
    /// Returns an error when no asynchronous runtime is available.
    pub fn install(
        paths: ConfigPaths,
        states: std::sync::Arc<bmux_plugin::PluginStateRegistry>,
    ) -> Result<Self, String> {
        let runtime = tokio::runtime::Handle::try_current()
            .map_err(|_| "broadcast mirror needs runtime".to_owned())?;
        let indicator = std::sync::Arc::new(crate::indicator::FactIndicator::bound(
            crate::indicator::fact_host(&states),
        ));
        let task_indicator = std::sync::Arc::clone(&indicator);
        let task = runtime.spawn(async move {
            let indicator = task_indicator;
            let mut client = None;
            let mut mirror = crate::mirror::Mirror::default();
            let mut tick = tokio::time::interval(Duration::from_millis(250));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tick.tick().await;
                if client.is_none() {
                    match tokio::time::timeout(
                        Duration::from_secs(1),
                        bmux_client::BmuxClient::connect_with_paths(&paths, "broadcast-indicator"),
                    )
                    .await
                    {
                        Ok(Ok(next)) => {
                            client = Some(next);
                            mirror.reconnect();
                        }
                        _ => {
                            if mirror.established {
                                indicator.update("unavailable", "unknown");
                            } else {
                                indicator.remove();
                            }
                            continue;
                        }
                    }
                }
                let Some(connection) = client.as_mut() else {
                    continue;
                };
                match tokio::time::timeout(
                    Duration::from_secs(1),
                    presentation::client::status(connection),
                )
                .await
                {
                    Ok(Ok(status)) => {
                        if !mirror.accept(&status.state, status.revision) {
                            if !["live", "paused", "offline", "unavailable"]
                                .contains(&status.state.as_str())
                            {
                                indicator.update("unavailable", "unknown");
                                client = None;
                            }
                            continue;
                        }
                        match status.state.as_str() {
                            "live" => indicator.update("running", "live"),
                            "paused" => indicator.update("paused", "paused"),
                            "offline" => indicator.update("stopped", "offline"),
                            "unavailable" => indicator.update("unavailable", "unknown"),
                            _ => {
                                indicator.update("unavailable", "unknown");
                                client = None;
                            }
                        }
                    }
                    _ => {
                        if mirror.established {
                            indicator.update("unavailable", "unknown");
                        } else {
                            indicator.remove();
                        }
                        client = None;
                    }
                }
            }
        });
        Ok(Self { task, indicator })
    }
}
impl Drop for BroadcastPresentation {
    fn drop(&mut self) {
        self.task.abort();
        self.indicator.remove();
    }
}
