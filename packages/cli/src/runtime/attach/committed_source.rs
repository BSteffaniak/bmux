//! Publication of this attach's successfully committed view, on its own connection.
use super::{observer::AttachSnapshot, state::AttachViewState};
use bmux_broadcast_plugin_api::broadcast_output_v1::client;
use bmux_client::StreamingBmuxClient;

pub(super) struct CommittedSource {
    id: String,
    sent: Option<(u64, uuid::Uuid, u64)>,
}

impl CommittedSource {
    #[cfg(not(feature = "bundled-plugin-broadcast"))]
    pub(super) async fn register(
        _client: &mut StreamingBmuxClient,
        _runtime_dir: &std::path::Path,
    ) -> Option<Self> {
        None
    }

    #[cfg(feature = "bundled-plugin-broadcast")]
    pub(super) async fn register(
        client: &mut StreamingBmuxClient,
        runtime_dir: &std::path::Path,
    ) -> Option<Self> {
        #[cfg(feature = "bundled-plugin-broadcast")]
        bmux_broadcast_plugin::authorize_committed_output(client, runtime_dir)
            .await
            .ok()?;
        let id = uuid::Uuid::new_v4().to_string();
        match client::register(client, id.clone()).await {
            Ok(_) => Some(Self { id, sent: None }),
            Err(error) => {
                // Missing or unauthorized broadcast must not affect local attach.
                tracing::debug!(%error, "committed view publication unavailable");
                None
            }
        }
    }

    pub(super) async fn poll(
        &mut self,
        connection: &mut StreamingBmuxClient,
        snapshot: Option<&AttachSnapshot>,
    ) {
        let state = match client::poll(connection, self.id.clone()).await {
            Ok(state) => state,
            Err(error) => {
                self.sent = None;
                tracing::debug!(%error, "committed view publication lease unavailable");
                return;
            }
        };
        if !state.selected {
            self.sent = None;
            return;
        }
        let Some(snapshot) = snapshot else { return };
        let identity = (state.refresh, snapshot.epoch, snapshot.revision);
        if self.sent == Some(identity) {
            return;
        }
        let frame = match encode_snapshot(snapshot) {
            Ok(frame) => frame,
            Err(error) => {
                tracing::debug!(%error, "committed view exceeds publication representation");
                return;
            }
        };
        match client::publish(connection, self.id.clone(), state.refresh, frame).await {
            Ok(true) => self.sent = Some(identity),
            Ok(false) => self.sent = None,
            Err(error) => {
                self.sent = None;
                tracing::debug!(%error, "committed view publication rejected");
            }
        }
    }

    pub(super) fn id(&self) -> &str {
        &self.id
    }

    pub(super) async fn unregister(&self, connection: &mut StreamingBmuxClient) {
        if let Err(error) = client::unregister(connection, self.id.clone()).await {
            tracing::debug!(%error, "unregistering committed view publication");
        }
    }
}

fn encode_snapshot(snapshot: &AttachSnapshot) -> anyhow::Result<Vec<u8>> {
    let frame = bmux_web_renderer::BrowserFrame::from_prepared_in_generation(
        &snapshot.epoch.to_string(),
        snapshot.revision,
        &snapshot.prepared,
    )?;
    let bytes = bmux_web_renderer::encode_binary(&frame)?;
    anyhow::ensure!(bytes.len() <= 8 * 1024 * 1024, "publication exceeds 8 MiB");
    Ok(bytes)
}

pub(super) fn latest(view: &AttachViewState) -> Option<std::sync::Arc<AttachSnapshot>> {
    view.observer.as_ref()?.latest_committed()
}

pub(super) async fn control(
    connection: &mut StreamingBmuxClient,
    view: &mut AttachViewState,
    command: &str,
    args: &[String],
) -> String {
    use bmux_broadcast_plugin_api::{
        broadcast_control_v1::client as controls, broadcast_turnkey_v1::client as turnkey,
    };
    let normalized = command.strip_prefix("broadcast-").unwrap_or(command);
    let (operation, args) = if normalized == "broadcast" {
        (
            args.first().map_or("status", String::as_str),
            args.get(1..).unwrap_or_default(),
        )
    } else {
        (normalized, args)
    };
    if view.committed_source.is_none() {
        if operation != "start" {
            return "broadcast: start this committed view first".into();
        }
        let readiness = match bmux_pane_runtime_plugin_api::pane_runtime_readiness_v1::client::runtime_readiness(connection).await {
            Ok(readiness) => readiness,
            Err(error) => return format!("broadcast: selected runtime unavailable: {error}"),
        };
        let Some(source) =
            CommittedSource::register(connection, std::path::Path::new(&readiness.runtime_dir))
                .await
        else {
            return "broadcast: local authorization or source registration rejected".into();
        };
        view.committed_source = Some(source);
        if view.observer.is_none() {
            let (publisher, _) = super::observer::attach_observer_channel();
            view.set_observer(publisher);
            view.dirty
                .mark_full_frame(super::state::AttachDirtySource::ManualRedraw);
        }
    }
    let Some(source) = view.committed_source.as_ref() else {
        unreachable!()
    };
    let result = match operation {
        "start" if args.is_empty() => {
            return match turnkey::start_profile(connection, None, source.id().to_string()).await {
                Ok(_) => format!("broadcast: publishing committed view {}", source.id()),
                Err(error) => format!("broadcast: {error}"),
            };
        }
        "start" if args.len() == 2 && args[0] == "--profile" => {
            return match turnkey::start_profile(connection, Some(args[1].clone()), source.id().to_string()).await {
                Ok(_) => format!("broadcast: publishing committed view {}", source.id()),
                Err(error) => format!("broadcast: {error}"),
            };
        }
        "start" if args.len() == 3 => controls::start(connection, args[0].clone(), args[1].clone(), args[2].clone(), source.id().to_string()).await,
        "pause" if args.is_empty() => controls::pause(connection).await,
        "resume" if args.is_empty() => controls::resume(connection).await,
        "stop" if args.is_empty() => controls::stop(connection).await,
        "status" if args.is_empty() => controls::status(connection).await,
        "source" if args.is_empty() => controls::source(connection, source.id().to_string()).await,
        "source" if args.len() == 1 && args[0] == source.id() => controls::source(connection, source.id().to_string()).await,
        _ => return "broadcast: use start [--profile NAME | ENDPOINT CHANNEL CREDENTIAL_FILE], pause, resume, stop, status, or source; terminal/foreign-source selection is not a committed-view mirror".into(),
    };
    match result {
        Ok(status) => format!(
            "broadcast: {} / {} (source {})",
            status.local,
            status.remote,
            source.id()
        ),
        Err(error) => format!("broadcast: {error}"),
    }
}

#[cfg(test)]
#[path = "committed_source_tests.rs"]
mod tests;
