use crate::connection::{TerminalMouseButton, TerminalMouseEvent, TerminalMouseEventKind};
use crate::error::{MobileCoreError, Result};
use crate::pane_runtime_client::{
    PaneRuntimeClientExt, attach_pane_frames_streaming, attach_pane_grid_snapshot_state_streaming,
    reset_attach_pane_frames_streaming,
};
use crate::paired_host::{PairedHostStore, runtime};
use bmux_attach_layout_protocol::AttachRect;
use bmux_attach_pipeline::mouse as attach_mouse;
use bmux_attach_pipeline::render::visible_scene_pane_ids;
use bmux_attach_pipeline::scene_pipeline::{AttachPaneFrameState, AttachPaneGridSnapshotState};
use bmux_attach_pipeline::{AttachScenePipeline, AttachViewport};
use bmux_attach_view_protocol::AttachViewComponent;
use bmux_client::{BmuxClient, ClientError, ServerEvent, StreamingBmuxClient};
use bmux_ipc::ErrorCode;
use bmux_pane_runtime_plugin_api::pane_runtime_events;
use bmux_session_models::SessionSelector;
use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};
use uuid::Uuid;

const SESSION_COMMAND_BUFFER: usize = 64;
const OUTPUT_QUEUE_MAX_BYTES: usize = 4 * 1024 * 1024;
/// Raw output bytes a snapshot asks for: none. Panes are hydrated from grid snapshots and kept by
/// frames, which carry their input modes too.
const EVENT_TRIGGER_FETCH_MAX_BYTES: usize = 0;
const GRID_SNAPSHOT_MAX_ROWS_PER_PANE: usize = 100_000;
/// History lines a pane's first frame carries at most; older lines stay on the server.
const FRAME_MAX_HISTORY_LINES: usize = 2_000;
const SESSION_KEEPALIVE_INTERVAL_MS: u64 = 10_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BackendSessionHandle {
    pub id: Uuid,
    pub session_id: Uuid,
    pub can_write: bool,
}

pub trait TerminalBackend: Send + Sync {
    /// Checks that saved host `host` answers and accepts this device.
    ///
    /// # Errors
    ///
    /// Returns an error if the host is unknown, unreachable, or refuses this
    /// device.
    fn probe(&self, host: &str) -> Result<()>;

    /// Attaches a terminal session on saved host `host`: `session` when set,
    /// else `remembered_session` when it still exists, else the session with
    /// the most clients (creating one when there is none).
    ///
    /// # Errors
    ///
    /// Returns an error if the host is unknown, the connection fails, or the
    /// attach flow cannot be completed.
    fn open(
        &self,
        host: &str,
        session: Option<String>,
        remembered_session: Option<String>,
        rows: u16,
        cols: u16,
    ) -> Result<BackendSessionHandle>;

    /// Reads terminal output bytes for an open session.
    ///
    /// # Errors
    ///
    /// Returns an error if the session handle is unknown or the remote output
    /// call fails.
    fn poll_output(&self, handle_id: Uuid, max_bytes: usize) -> Result<Vec<u8>>;

    /// Return complete native pointer metadata after queued bytes are drained.
    ///
    /// # Errors
    /// Returns unsupported-version or backend errors.
    fn pointer_output(
        &self,
        _handle_id: Uuid,
        version: u32,
    ) -> Result<Option<crate::pointer_output::TerminalPointerOutput>> {
        crate::pointer_output::validate_pointer_version(version)?;
        Ok(None)
    }

    /// # Errors
    /// Returns backend lookup or input errors.
    fn write_input(&self, handle_id: Uuid, bytes: &[u8]) -> Result<()>;

    /// Sends a terminal mouse event for an open session.
    ///
    /// # Errors
    ///
    /// Returns an error if the session handle is unknown or the remote mouse
    /// routing call fails.
    fn mouse_event(&self, handle_id: Uuid, event: &TerminalMouseEvent) -> Result<()>;

    /// Updates the terminal viewport size for an open session.
    ///
    /// # Errors
    ///
    /// Returns an error if the session handle is unknown or the remote resize
    /// call fails.
    fn resize(&self, handle_id: Uuid, rows: u16, cols: u16) -> Result<()>;

    /// Closes an open session and detaches from the remote terminal.
    ///
    /// # Errors
    ///
    /// Returns an error if the session handle is unknown.
    fn close(&self, handle_id: Uuid) -> Result<()>;
}

struct RemoteTerminalSession {
    command_tx: mpsc::Sender<SessionCommand>,
    actor_state: Arc<Mutex<SessionActorState>>,
}

#[derive(Debug, Default)]
struct SessionActorState {
    terminated_reason: Option<String>,
}

enum SessionCommand {
    PollOutput {
        max_bytes: usize,
        response: oneshot::Sender<Result<Vec<u8>>>,
    },
    PointerOutput {
        version: u32,
        response: oneshot::Sender<Result<Option<crate::pointer_output::TerminalPointerOutput>>>,
    },
    WriteInput {
        bytes: Vec<u8>,
        response: oneshot::Sender<Result<()>>,
    },
    MouseEvent {
        event: TerminalMouseEvent,
        response: oneshot::Sender<Result<()>>,
    },
    Resize {
        rows: u16,
        cols: u16,
        response: oneshot::Sender<Result<()>>,
    },
    Close {
        response: oneshot::Sender<Result<()>>,
    },
}

/// Terminal sessions on saved paired hosts, each attached over a client the
/// connections plugin's saved-host connector opens.
pub struct RemoteTerminalBackend {
    hosts: Arc<PairedHostStore>,
    sessions: Mutex<BTreeMap<Uuid, RemoteTerminalSession>>,
}

impl RemoteTerminalBackend {
    #[must_use]
    pub const fn new(hosts: Arc<PairedHostStore>) -> Self {
        Self {
            hosts,
            sessions: Mutex::new(BTreeMap::new()),
        }
    }

    fn lock_sessions(&self) -> Result<MutexGuard<'_, BTreeMap<Uuid, RemoteTerminalSession>>> {
        self.sessions.lock().map_err(|_| {
            MobileCoreError::TerminalBackendFailure(
                "terminal backend sessions lock poisoned".to_string(),
            )
        })
    }

    fn session_command_tx(&self, handle_id: Uuid) -> Result<mpsc::Sender<SessionCommand>> {
        self.lock_sessions()?
            .get(&handle_id)
            .map(|session| session.command_tx.clone())
            .ok_or_else(|| MobileCoreError::TerminalSessionNotFound(handle_id.to_string()))
    }

    fn session_actor_state(&self, handle_id: Uuid) -> Result<Arc<Mutex<SessionActorState>>> {
        self.lock_sessions()?
            .get(&handle_id)
            .map(|session| Arc::clone(&session.actor_state))
            .ok_or_else(|| MobileCoreError::TerminalSessionNotFound(handle_id.to_string()))
    }

    fn remove_session_if_present(&self, handle_id: Uuid) {
        if let Ok(mut sessions) = self.lock_sessions() {
            sessions.remove(&handle_id);
        }
    }

    fn call_session<T>(
        &self,
        handle_id: Uuid,
        command_builder: impl FnOnce(oneshot::Sender<Result<T>>) -> SessionCommand,
    ) -> Result<T> {
        let command_tx = self.session_command_tx(handle_id)?;
        let actor_state = self.session_actor_state(handle_id)?;
        if let Some(reason) = session_terminated_reason(&actor_state) {
            self.remove_session_if_present(handle_id);
            return Err(MobileCoreError::TerminalBackendFailure(reason));
        }

        let response = runtime().block_on(async {
            let (response_tx, response_rx) = oneshot::channel();
            command_tx
                .send(command_builder(response_tx))
                .await
                .map_err(|_| {
                    MobileCoreError::TerminalBackendFailure(
                        session_terminated_reason(&actor_state).unwrap_or_else(|| {
                            "terminal backend session actor unavailable".to_string()
                        }),
                    )
                })?;
            response_rx.await.map_err(|_| {
                MobileCoreError::TerminalBackendFailure(
                    session_terminated_reason(&actor_state).unwrap_or_else(|| {
                        "terminal backend session actor dropped response".to_string()
                    }),
                )
            })
        });

        match response {
            Ok(result) => result,
            Err(error) => {
                self.remove_session_if_present(handle_id);
                Err(error)
            }
        }
    }

    fn session_selector_from_value(value: &str) -> SessionSelector {
        Uuid::parse_str(value).map_or_else(
            |_| SessionSelector::ByName(value.to_string()),
            SessionSelector::ById,
        )
    }

    fn normalize_session_value(value: Option<&str>) -> Option<String> {
        value
            .map(str::trim)
            .filter(|item| !item.is_empty())
            .map(ToString::to_string)
    }

    const fn is_session_not_found(error: &ClientError) -> bool {
        matches!(
            error,
            ClientError::ServerError {
                code: ErrorCode::NotFound,
                ..
            }
        )
    }

    async fn attach_with_selector(
        client: &mut BmuxClient,
        selector: SessionSelector,
    ) -> std::result::Result<(Uuid, bool), ClientError> {
        let grant = client.attach_grant(selector).await?;
        let info = client.open_attach_stream_info(&grant).await?;
        Ok((info.session_id, info.can_write))
    }

    async fn attach_auto_session(client: &mut BmuxClient) -> Result<(Uuid, bool)> {
        // Typed dispatch: `sessions-state:list-sessions` then
        // `sessions-commands:new-session` if the list is empty.
        let list_payload = bmux_codec::to_positional_vec(&())
            .map_err(|error| MobileCoreError::TerminalBackendFailure(error.to_string()))?;
        let list_bytes = client
            .invoke_service_raw(
                bmux_sessions_plugin_api::capabilities::SESSIONS_READ.as_str(),
                bmux_ipc::InvokeServiceKind::Query,
                bmux_sessions_plugin_api::sessions_state::INTERFACE_ID.as_str(),
                "list-sessions",
                list_payload,
            )
            .await
            .map_err(|error| MobileCoreError::TerminalBackendFailure(error.to_string()))?;
        let sessions: Vec<bmux_sessions_plugin_api::sessions_state::SessionSummary> =
            bmux_codec::from_positional_bytes(&list_bytes)
                .map_err(|error| MobileCoreError::TerminalBackendFailure(error.to_string()))?;

        let selector =
            if let Some(entry) = sessions.into_iter().max_by_key(|entry| entry.client_count) {
                SessionSelector::ById(entry.id)
            } else {
                #[derive(serde::Serialize)]
                struct NewArgs {
                    name: Option<String>,
                }
                let new_payload = bmux_codec::to_positional_vec(&NewArgs { name: None })
                    .map_err(|error| MobileCoreError::TerminalBackendFailure(error.to_string()))?;
                let new_bytes = client
                    .invoke_service_raw(
                        bmux_sessions_plugin_api::capabilities::SESSIONS_WRITE.as_str(),
                        bmux_ipc::InvokeServiceKind::Command,
                        bmux_sessions_plugin_api::sessions_commands::INTERFACE_ID.as_str(),
                        "new-session",
                        new_payload,
                    )
                    .await
                    .map_err(|error| MobileCoreError::TerminalBackendFailure(error.to_string()))?;
                let outcome: std::result::Result<
                    bmux_sessions_plugin_api::sessions_commands::SessionAck,
                    bmux_sessions_plugin_api::sessions_commands::NewSessionError,
                > = bmux_codec::from_positional_bytes(&new_bytes)
                    .map_err(|error| MobileCoreError::TerminalBackendFailure(error.to_string()))?;
                let ack = outcome.map_err(|err| {
                    MobileCoreError::TerminalBackendFailure(format!("new-session failed: {err:?}"))
                })?;
                SessionSelector::ById(ack.id)
            };

        Self::attach_with_selector(client, selector)
            .await
            .map_err(|error| MobileCoreError::TerminalBackendFailure(error.to_string()))
    }
}

impl TerminalBackend for RemoteTerminalBackend {
    fn probe(&self, host: &str) -> Result<()> {
        runtime().block_on(self.hosts.open_client(host)).map(drop)
    }

    #[allow(clippy::too_many_lines)]
    fn open(
        &self,
        host: &str,
        session: Option<String>,
        remembered_session: Option<String>,
        rows: u16,
        cols: u16,
    ) -> Result<BackendSessionHandle> {
        let explicit_session = Self::normalize_session_value(session.as_deref());
        let preferred_session = explicit_session
            .clone()
            .or_else(|| Self::normalize_session_value(remembered_session.as_deref()));

        let (handle_id, session_id, can_write, initial_snapshot, streaming_client) =
            runtime().block_on(async {
                let mut client = self.hosts.open_client(host).await?;
                let attach_info = if let Some(preferred) = preferred_session {
                    let selector = Self::session_selector_from_value(&preferred);
                    match Self::attach_with_selector(&mut client, selector).await {
                        Ok(info) => Ok(info),
                        Err(error)
                            if explicit_session.is_none() && Self::is_session_not_found(&error) =>
                        {
                            Self::attach_auto_session(&mut client).await
                        }
                        Err(error) if Self::is_session_not_found(&error) => {
                            Err(MobileCoreError::TerminalBackendFailure(format!(
                                "{SESSION_NOT_FOUND}: {error}"
                            )))
                        }
                        Err(error) => {
                            Err(MobileCoreError::TerminalBackendFailure(error.to_string()))
                        }
                    }
                } else {
                    Self::attach_auto_session(&mut client).await
                }?;

                let _ = client
                    .attach_set_viewport(attach_info.0, cols, rows)
                    .await
                    .map_err(|error| MobileCoreError::TerminalBackendFailure(error.to_string()))?;

                let mut streaming_client = StreamingBmuxClient::from_client(client)
                    .map_err(|error| MobileCoreError::TerminalBackendFailure(error.to_string()))?;
                streaming_client
                    .enable_event_push()
                    .await
                    .map_err(|error| MobileCoreError::TerminalBackendFailure(error.to_string()))?;

                let initial_snapshot = if let Ok(snapshot) = streaming_client
                    .attach_snapshot(attach_info.0, EVENT_TRIGGER_FETCH_MAX_BYTES)
                    .await
                {
                    Some(snapshot)
                } else {
                    streaming_client
                        .attach_layout(attach_info.0)
                        .await
                        .ok()
                        .map(|layout| bmux_client::AttachSnapshotState {
                            context_id: layout.context_id,
                            session_id: layout.session_id,
                            focused_pane_id: layout.focused_pane_id,
                            panes: layout.panes,
                            layout_root: layout.layout_root,
                            scene: layout.scene,
                            chunks: Vec::new(),
                            pane_mouse_protocols: Vec::new(),
                            pane_input_modes: Vec::new(),
                            zoomed: layout.zoomed,
                        })
                };

                Ok::<
                    (
                        Uuid,
                        Uuid,
                        bool,
                        Option<bmux_client::AttachSnapshotState>,
                        StreamingBmuxClient,
                    ),
                    MobileCoreError,
                >((
                    Uuid::new_v4(),
                    attach_info.0,
                    attach_info.1,
                    initial_snapshot,
                    streaming_client,
                ))
            })?;

        let (command_tx, command_rx) = mpsc::channel(SESSION_COMMAND_BUFFER);
        let actor_state = Arc::new(Mutex::new(SessionActorState::default()));
        drop(runtime().spawn(run_remote_terminal_session(
            streaming_client,
            session_id,
            initial_snapshot,
            AttachViewport {
                cols,
                rows,
                top_inset: 0,
                right_inset: 0,
                bottom_inset: 0,
                left_inset: 0,
            },
            command_rx,
            Arc::clone(&actor_state),
        )));
        self.lock_sessions()?.insert(
            handle_id,
            RemoteTerminalSession {
                command_tx,
                actor_state,
            },
        );

        Ok(BackendSessionHandle {
            id: handle_id,
            session_id,
            can_write,
        })
    }

    fn poll_output(&self, handle_id: Uuid, max_bytes: usize) -> Result<Vec<u8>> {
        self.call_session(handle_id, |response| SessionCommand::PollOutput {
            max_bytes,
            response,
        })
    }

    fn pointer_output(
        &self,
        handle_id: Uuid,
        version: u32,
    ) -> Result<Option<crate::pointer_output::TerminalPointerOutput>> {
        crate::pointer_output::validate_pointer_version(version)?;
        self.call_session(handle_id, |response| SessionCommand::PointerOutput {
            version,
            response,
        })
    }

    fn write_input(&self, handle_id: Uuid, bytes: &[u8]) -> Result<()> {
        self.call_session(handle_id, |response| SessionCommand::WriteInput {
            bytes: bytes.to_vec(),
            response,
        })
    }

    fn mouse_event(&self, handle_id: Uuid, event: &TerminalMouseEvent) -> Result<()> {
        self.call_session(handle_id, |response| SessionCommand::MouseEvent {
            event: *event,
            response,
        })
    }

    fn resize(&self, handle_id: Uuid, rows: u16, cols: u16) -> Result<()> {
        self.call_session(handle_id, |response| SessionCommand::Resize {
            rows,
            cols,
            response,
        })
    }

    fn close(&self, handle_id: Uuid) -> Result<()> {
        let session = self
            .lock_sessions()?
            .remove(&handle_id)
            .ok_or_else(|| MobileCoreError::TerminalSessionNotFound(handle_id.to_string()))?;

        runtime().block_on(async {
            let (response_tx, response_rx) = oneshot::channel();
            if session
                .command_tx
                .send(SessionCommand::Close {
                    response: response_tx,
                })
                .await
                .is_err()
            {
                return Ok(());
            }
            response_rx.await.unwrap_or(Ok(()))
        })
    }
}

/// How an open names a requested session the server does not have.
pub(crate) const SESSION_NOT_FOUND: &str = "requested session not found";

/// Whether `error` says the requested session does not exist (as opposed
/// to the server being unreachable).
pub(crate) fn is_session_gone_error(error: &MobileCoreError) -> bool {
    matches!(error, MobileCoreError::TerminalBackendFailure(message) if message.starts_with(SESSION_NOT_FOUND))
}

fn set_session_terminated_reason(actor_state: &Arc<Mutex<SessionActorState>>, reason: String) {
    if let Ok(mut state) = actor_state.lock()
        && state.terminated_reason.is_none()
    {
        state.terminated_reason = Some(reason);
    }
}

fn session_terminated_reason(actor_state: &Arc<Mutex<SessionActorState>>) -> Option<String> {
    actor_state.lock().ok().and_then(|state| {
        state
            .terminated_reason
            .as_ref()
            .map(|reason| format!("terminal session terminated: {reason}"))
    })
}

async fn run_remote_terminal_session(
    mut client: StreamingBmuxClient,
    session_id: Uuid,
    initial_snapshot: Option<bmux_client::AttachSnapshotState>,
    viewport: AttachViewport,
    mut command_rx: mpsc::Receiver<SessionCommand>,
    actor_state: Arc<Mutex<SessionActorState>>,
) {
    let mut state = StreamOutputState::new(viewport);
    if let Some(snapshot) = initial_snapshot {
        state.hydrate_snapshot(snapshot);
        if let Err(error) =
            hydrate_visible_grid_snapshots(&mut client, &mut state, session_id).await
        {
            set_session_terminated_reason(&actor_state, error.to_string());
        }
        if let Err(error) = state.render_if_dirty() {
            set_session_terminated_reason(&actor_state, error.to_string());
        }
    }

    let mut event_stream_open = true;
    let mut keepalive = tokio::time::interval(Duration::from_millis(SESSION_KEEPALIVE_INTERVAL_MS));
    keepalive.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let _ = keepalive.tick().await;

    loop {
        tokio::select! {
            maybe_command = command_rx.recv() => {
                let Some(command) = maybe_command else {
                    break;
                };
                if !handle_session_command(
                    &mut client,
                    &mut state,
                    session_id,
                    command,
                    &actor_state,
                ).await {
                    return;
                }
            }
            maybe_event = client.event_receiver().recv(), if event_stream_open => {
                let Some(event) = maybe_event else {
                    event_stream_open = false;
                    let reason = client.disconnect_reason().unwrap_or_else(|| {
                        "terminal session stream closed by remote peer".to_string()
                    });
                    set_session_terminated_reason(
                        &actor_state,
                        reason,
                    );
                    continue;
                };
                if let Err(error) = handle_session_event(&mut client, &mut state, session_id, event).await {
                    set_session_terminated_reason(&actor_state, error.to_string());
                }
            }
            _ = keepalive.tick(), if event_stream_open => {
                if let Err(error) = client.ping().await {
                    event_stream_open = false;
                    set_session_terminated_reason(
                        &actor_state,
                        format!("keepalive ping failed: {error}"),
                    );
                }
            }
        }
    }

    let _ = client.detach().await;
}

/// Reject work after terminal failure while still allowing the attachment to close.
fn reject_terminated_session_command(command: SessionCommand, reason: String) -> bool {
    match command {
        SessionCommand::PollOutput { response, .. } => {
            let _ = response.send(Err(MobileCoreError::TerminalBackendFailure(reason)));
        }
        SessionCommand::PointerOutput { response, .. } => {
            let _ = response.send(Err(MobileCoreError::TerminalBackendFailure(reason)));
        }
        SessionCommand::WriteInput { response, .. } => {
            let _ = response.send(Err(MobileCoreError::TerminalBackendFailure(format!(
                "{reason}; input rejected"
            ))));
        }
        SessionCommand::MouseEvent { response, .. } => {
            let _ = response.send(Err(MobileCoreError::TerminalBackendFailure(format!(
                "{reason}; mouse event rejected"
            ))));
        }
        SessionCommand::Resize { response, .. } => {
            let _ = response.send(Err(MobileCoreError::TerminalBackendFailure(format!(
                "{reason}; resize rejected"
            ))));
        }
        SessionCommand::Close { response } => {
            let _ = response.send(Ok(()));
            return false;
        }
    }
    true
}

async fn handle_session_command(
    client: &mut StreamingBmuxClient,
    state: &mut StreamOutputState,
    session_id: Uuid,
    command: SessionCommand,
    actor_state: &Arc<Mutex<SessionActorState>>,
) -> bool {
    if let Some(reason) = session_terminated_reason(actor_state) {
        return reject_terminated_session_command(command, reason);
    }

    match command {
        SessionCommand::PollOutput {
            max_bytes,
            response,
        } => {
            let render_result = state.render_if_dirty();
            if let Err(error) = &render_result {
                set_session_terminated_reason(actor_state, error.to_string());
            }
            let _ = match render_result {
                Ok(()) => response.send(Ok(state.drain_output(max_bytes))),
                Err(error) => response.send(Err(error)),
            };
            true
        }
        SessionCommand::PointerOutput { version, response } => {
            let _ = response.send(state.take_pointer_output(version));
            true
        }
        SessionCommand::WriteInput { bytes, response } => {
            let result = client
                .attach_input(session_id, bytes)
                .await
                .map(|_bytes| ())
                .map_err(|error| MobileCoreError::TerminalBackendFailure(error.to_string()));
            if let Err(error) = &result {
                set_session_terminated_reason(actor_state, error.to_string());
            }
            let _ = response.send(result);
            true
        }
        SessionCommand::MouseEvent { event, response } => {
            let result = handle_session_mouse_event(client, state, session_id, event).await;
            if let Err(error) = &result {
                set_session_terminated_reason(actor_state, error.to_string());
            }
            let _ = response.send(result);
            true
        }
        SessionCommand::Resize {
            rows,
            cols,
            response,
        } => {
            let result = client
                .attach_set_viewport_with_insets(session_id, cols, rows, 0, 0, 0, 0)
                .await
                .map(|(_cols, _rows)| {
                    state.set_viewport(AttachViewport {
                        cols,
                        rows,
                        top_inset: 0,
                        right_inset: 0,
                        bottom_inset: 0,
                        left_inset: 0,
                    });
                })
                .map_err(|error| MobileCoreError::TerminalBackendFailure(error.to_string()));
            if let Err(error) = &result {
                set_session_terminated_reason(actor_state, error.to_string());
            }
            let _ = response.send(result);
            true
        }
        SessionCommand::Close { response } => {
            let result = client
                .detach()
                .await
                .map_err(|error| MobileCoreError::TerminalBackendFailure(error.to_string()));
            let _ = response.send(result);
            false
        }
    }
}

async fn handle_session_mouse_event(
    client: &mut StreamingBmuxClient,
    state: &mut StreamOutputState,
    session_id: Uuid,
    event: TerminalMouseEvent,
) -> Result<()> {
    state.pointer_position = if event.kind == TerminalMouseEventKind::Leave {
        None
    } else {
        Some((event.col, event.row))
    };
    if matches!(event.kind, TerminalMouseEventKind::Leave) {
        return Ok(());
    }
    let Some((target_pane, pane_content_rect)) =
        state.target_pane_and_rect_at(event.col, event.row)
    else {
        return Ok(());
    };

    let in_focused_pane = state
        .focused_pane_id()
        .is_some_and(|focused_pane| focused_pane == target_pane);
    let focus_before_forward = matches!(
        event.kind,
        TerminalMouseEventKind::Down | TerminalMouseEventKind::Up | TerminalMouseEventKind::Drag
    ) && matches!(event.button, Some(TerminalMouseButton::Left));

    if focus_before_forward && !in_focused_pane {
        #[derive(serde::Serialize)]
        struct FocusPaneBySelectorArgs {
            session: bmux_tabs_plugin_api::tabs_commands::Selector,
            target: bmux_tabs_plugin_api::tabs_commands::Selector,
        }
        let args = FocusPaneBySelectorArgs {
            session: bmux_tabs_plugin_api::tabs_commands::Selector {
                id: Some(session_id),
                name: None,
                index: None,
            },
            target: bmux_tabs_plugin_api::tabs_commands::Selector {
                id: Some(target_pane),
                name: None,
                index: None,
            },
        };
        let encoded = bmux_plugin_sdk::encode_service_message(&args).map_err(|error| {
            MobileCoreError::TerminalBackendFailure(format!(
                "encoding focus-pane-by-selector args: {error}"
            ))
        })?;
        let _response_bytes = client
            .invoke_service_raw(
                bmux_tabs_plugin_api::capabilities::TABS_WRITE.as_str(),
                bmux_ipc::InvokeServiceKind::Command,
                bmux_tabs_plugin_api::tabs_commands::INTERFACE_ID.as_str(),
                "focus-pane-by-selector",
                encoded,
            )
            .await
            .map_err(|error| MobileCoreError::TerminalBackendFailure(error.to_string()))?;
        if let Ok(layout) = client.attach_layout(session_id).await {
            let _ = state.apply_layout_state(layout);
        }
    } else if !in_focused_pane {
        return Ok(());
    }

    let Some(protocol) = state.pane_protocol(target_pane) else {
        return Ok(());
    };
    let Some(mouse_event) = terminal_mouse_event_to_attach_mouse_event(event) else {
        return Ok(());
    };
    // Programs inside the pane expect pane-local coordinates. See the
    // matching translation in `attach_mouse_forward_bytes_for_target` for
    // the attach CLI path.
    let Some(local_event) =
        attach_mouse::translate_event_to_pane_local(mouse_event, pane_content_rect)
    else {
        return Ok(());
    };
    let Some(bytes) = attach_mouse::encode_for_protocol(local_event, protocol) else {
        return Ok(());
    };

    client
        .attach_input(session_id, bytes)
        .await
        .map(|_bytes| ())
        .map_err(|error| MobileCoreError::TerminalBackendFailure(error.to_string()))
}

const fn terminal_mouse_button_to_attach(button: TerminalMouseButton) -> attach_mouse::Button {
    match button {
        TerminalMouseButton::Left => attach_mouse::Button::Left,
        TerminalMouseButton::Middle => attach_mouse::Button::Middle,
        TerminalMouseButton::Right => attach_mouse::Button::Right,
    }
}

fn terminal_mouse_event_to_attach_mouse_event(
    event: TerminalMouseEvent,
) -> Option<attach_mouse::Event> {
    let kind = match event.kind {
        TerminalMouseEventKind::Down => {
            attach_mouse::EventKind::Down(terminal_mouse_button_to_attach(event.button?))
        }
        TerminalMouseEventKind::Up => {
            attach_mouse::EventKind::Up(terminal_mouse_button_to_attach(event.button?))
        }
        TerminalMouseEventKind::Drag => {
            attach_mouse::EventKind::Drag(terminal_mouse_button_to_attach(event.button?))
        }
        TerminalMouseEventKind::Leave => return None,
        TerminalMouseEventKind::Move => attach_mouse::EventKind::Moved,
        TerminalMouseEventKind::ScrollUp => attach_mouse::EventKind::ScrollUp,
        TerminalMouseEventKind::ScrollDown => attach_mouse::EventKind::ScrollDown,
        TerminalMouseEventKind::ScrollLeft => attach_mouse::EventKind::ScrollLeft,
        TerminalMouseEventKind::ScrollRight => attach_mouse::EventKind::ScrollRight,
    };

    Some(attach_mouse::Event {
        kind,
        column: event.col,
        row: event.row,
        modifiers: attach_mouse::Modifiers {
            shift: event.shift,
            alt: event.alt,
            control: event.control,
        },
    })
}

async fn handle_session_event(
    client: &mut StreamingBmuxClient,
    state: &mut StreamOutputState,
    session_id: Uuid,
    event: ServerEvent,
) -> Result<()> {
    match event {
        ServerEvent::PluginBusEvent { kind, payload }
            if kind == pane_runtime_events::EVENT_KIND.as_str() =>
        {
            if let Ok(event) = serde_json::from_slice::<pane_runtime_events::PaneEvent>(&payload) {
                handle_pane_runtime_event(client, state, session_id, event).await?;
            }
        }
        _ => {}
    }
    Ok(())
}

async fn handle_pane_runtime_event(
    client: &mut StreamingBmuxClient,
    state: &mut StreamOutputState,
    session_id: Uuid,
    event: pane_runtime_events::PaneEvent,
) -> Result<()> {
    match event {
        pane_runtime_events::PaneEvent::OutputAvailable {
            session_id: event_session_id,
            ..
        } if event_session_id == session_id => {
            let pane_ids = state.visible_pane_ids();
            if !pane_ids.is_empty() {
                apply_visible_frames(client, state, session_id, pane_ids).await?;
            }
        }
        pane_runtime_events::PaneEvent::AttachViewChanged {
            session_id: event_session_id,
            components,
            ..
        } if event_session_id == session_id => {
            let components = components
                .iter()
                .copied()
                .map(plugin_attach_view_component)
                .collect::<Vec<_>>();
            let component_hydration_requested = state.apply_view_change_components(&components);
            if let Ok(layout) = client.attach_layout(session_id).await {
                let layout_hydration_requested = state.apply_layout_state(layout);
                if component_hydration_requested || layout_hydration_requested {
                    hydrate_full_scene_snapshot(client, state, session_id).await?;
                }
            } else if component_hydration_requested {
                hydrate_full_scene_snapshot(client, state, session_id).await?;
            }
        }
        _ => {}
    }
    Ok(())
}

const fn plugin_attach_view_component(
    component: pane_runtime_events::AttachViewComponent,
) -> AttachViewComponent {
    match component {
        pane_runtime_events::AttachViewComponent::Scene => AttachViewComponent::Scene,
        pane_runtime_events::AttachViewComponent::SurfaceContent => {
            AttachViewComponent::SurfaceContent
        }
        pane_runtime_events::AttachViewComponent::Layout => AttachViewComponent::Layout,
    }
}

async fn hydrate_visible_grid_snapshots(
    client: &mut StreamingBmuxClient,
    state: &mut StreamOutputState,
    session_id: Uuid,
) -> Result<()> {
    let pane_ids = state.visible_pane_ids();
    if pane_ids.is_empty() {
        return Ok(());
    }
    let snapshots = attach_pane_grid_snapshot_state_streaming(
        client,
        session_id,
        pane_ids,
        GRID_SNAPSHOT_MAX_ROWS_PER_PANE,
    )
    .await
    .map_err(|error| MobileCoreError::TerminalBackendFailure(error.to_string()))?;
    let decoded = snapshots
        .into_iter()
        .map(|snapshot| {
            let grid_snapshot =
                serde_json::from_slice::<bmux_terminal_grid::GridSnapshot>(&snapshot.encoded)
                    .map_err(|error| MobileCoreError::TerminalBackendFailure(error.to_string()))?;
            Ok::<AttachPaneGridSnapshotState, MobileCoreError>(AttachPaneGridSnapshotState {
                pane_id: snapshot.pane_id,
                stream_end: snapshot.stream_end,
                snapshot: grid_snapshot,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    state.hydrate_grid_snapshots(decoded)
}

/// Bring the replicas of `pane_ids` level with their grids through frames. A pane whose frame
/// does not apply starts over from a reset frame.
async fn apply_visible_frames(
    client: &mut StreamingBmuxClient,
    state: &mut StreamOutputState,
    session_id: Uuid,
    pane_ids: Vec<Uuid>,
) -> Result<()> {
    let backend = |error: ClientError| MobileCoreError::TerminalBackendFailure(error.to_string());
    loop {
        let (frames, still_pending) = attach_pane_frames_streaming(
            client,
            session_id,
            pane_ids.clone(),
            FRAME_MAX_HISTORY_LINES,
        )
        .await
        .map_err(backend)?;
        let failed = state.apply_frames(
            frames
                .into_iter()
                .map(|frame| AttachPaneFrameState {
                    pane_id: frame.pane_id,
                    sync_update_active: frame.sync_update_active,
                    frame: frame.encoded,
                })
                .collect(),
        );
        if !failed.is_empty() {
            reset_attach_pane_frames_streaming(client, session_id, failed)
                .await
                .map_err(backend)?;
            continue;
        }
        if !still_pending {
            return Ok(());
        }
    }
}

async fn hydrate_full_scene_snapshot(
    client: &mut StreamingBmuxClient,
    state: &mut StreamOutputState,
    session_id: Uuid,
) -> Result<()> {
    let snapshot = client
        .attach_snapshot(session_id, EVENT_TRIGGER_FETCH_MAX_BYTES)
        .await
        .map_err(|error| MobileCoreError::TerminalBackendFailure(error.to_string()))?;
    state.hydrate_snapshot(snapshot);
    hydrate_visible_grid_snapshots(client, state, session_id).await?;
    Ok(())
}

struct StreamOutputState {
    output_queue: OutputQueue,
    pipeline: AttachScenePipeline,
    pointer_position: Option<(u16, u16)>,
    viewport: AttachViewport,
    pointer_generation: Uuid,
    pointer_revision: u64,
    delivered_pointer_revision: u64,
    pointer_output: Option<crate::pointer_output::TerminalPointerOutput>,
}

impl StreamOutputState {
    fn new(viewport: AttachViewport) -> Self {
        Self {
            output_queue: OutputQueue::new(OUTPUT_QUEUE_MAX_BYTES),
            pipeline: AttachScenePipeline::new(viewport),
            pointer_position: None,
            viewport,
            pointer_generation: Uuid::new_v4(),
            pointer_revision: 0,
            delivered_pointer_revision: 0,
            pointer_output: None,
        }
    }

    fn set_viewport(&mut self, viewport: AttachViewport) {
        self.viewport = viewport;
        self.pipeline.set_viewport(viewport);
    }

    fn hydrate_snapshot(&mut self, snapshot: bmux_client::AttachSnapshotState) {
        self.pipeline.hydrate_snapshot(snapshot);
    }

    fn hydrate_grid_snapshots(
        &mut self,
        snapshots: Vec<AttachPaneGridSnapshotState>,
    ) -> Result<()> {
        self.pipeline
            .hydrate_pane_grid_snapshots(snapshots)
            .map_err(|error| MobileCoreError::TerminalBackendFailure(error.to_string()))
    }

    fn apply_frames(&mut self, frames: Vec<AttachPaneFrameState>) -> Vec<Uuid> {
        self.pipeline.apply_pane_frames(frames)
    }

    fn apply_layout_state(&mut self, layout_state: bmux_client::AttachLayoutState) -> bool {
        self.pipeline.apply_layout_state(layout_state)
    }

    fn apply_view_change_components(&mut self, components: &[AttachViewComponent]) -> bool {
        self.pipeline.apply_view_change_components(components)
    }

    fn visible_pane_ids(&self) -> Vec<Uuid> {
        self.pipeline
            .layout_state
            .as_ref()
            .map_or_else(Vec::new, |layout_state| {
                visible_scene_pane_ids(&layout_state.scene)
            })
    }

    fn focused_pane_id(&self) -> Option<Uuid> {
        self.pipeline
            .layout_state
            .as_ref()
            .map(|layout_state| layout_state.focused_pane_id)
    }

    fn target_pane_and_rect_at(&self, column: u16, row: u16) -> Option<(Uuid, AttachRect)> {
        let layout_state = self.pipeline.layout_state.as_ref()?;
        attach_mouse::pane_and_rect_at(&layout_state.scene, column, row)
    }

    fn pane_protocol(&self, pane_id: Uuid) -> Option<attach_mouse::PaneProtocol> {
        attach_mouse::pane_protocol(
            &self.pipeline.pane_buffers,
            self.pipeline.pane_mouse_protocol_hints(),
            pane_id,
        )
    }

    fn render_if_dirty(&mut self) -> Result<()> {
        if let Some(frame) = self
            .pipeline
            .render_frame()
            .map_err(|error| MobileCoreError::TerminalBackendFailure(error.to_string()))?
        {
            self.output_queue.push_bytes(frame)?;
        }
        self.resolve_pointer();
        Ok(())
    }

    fn resolve_pointer(&mut self) {
        use crate::pointer_output::{
            NATIVE_POINTER_VERSION, TerminalPointerOutput, TerminalPointerShape,
        };
        use bmux_attach_pipeline::compositor::retained_surfaces_from_attach_scene;
        use bmux_attach_pipeline::pointer_shape::{
            PointerShapeInput, ResolvedPointerShape, resolve_pointer_shape,
        };
        use bmux_attach_pipeline::render::{DamageCoalescingPolicy, DamageRect};
        use bmux_attach_pipeline::{RetainedCompositor, RetainedPointerRouter};
        use bmux_terminal_protocol::pointer_shape::PointerShape;

        let mut compositor = RetainedCompositor::new();
        if let Some(layout) = &self.pipeline.layout_state {
            compositor.replace_surfaces(
                retained_surfaces_from_attach_scene(&layout.scene),
                DamageRect::new(0, 0, self.viewport.cols, self.viewport.rows),
                DamageCoalescingPolicy::default(),
            );
        }
        let router = RetainedPointerRouter::default();
        let resolved = resolve_pointer_shape(
            PointerShapeInput {
                compositor: &compositor,
                router: &router,
                position: self.pointer_position,
                // Existing mobile surfaces have no resize/floating gesture ownership.
                gesture: None,
                fallback: PointerShape::Default,
            },
            |content_id| {
                let buffer = self.pipeline.pane_buffers.get(&content_id)?;
                let grid = buffer.terminal_grid.grid();
                grid.protocol_state()
                    .pointer_shapes
                    .current(grid.mode() == bmux_terminal_grid::GridMode::Alternate)
            },
        );
        let shape = match resolved {
            ResolvedPointerShape::Hidden => TerminalPointerShape::Hidden,
            ResolvedPointerShape::Shape(shape) => TerminalPointerShape::Shape(shape),
        };
        if self
            .pointer_output
            .as_ref()
            .is_none_or(|previous| previous.shape != shape)
        {
            self.pointer_revision = self.pointer_revision.saturating_add(1);
            self.pointer_output = Some(TerminalPointerOutput {
                version: NATIVE_POINTER_VERSION,
                generation: self.pointer_generation,
                revision: self.pointer_revision,
                shape,
            });
        }
    }

    fn take_pointer_output(
        &mut self,
        version: u32,
    ) -> Result<Option<crate::pointer_output::TerminalPointerOutput>> {
        crate::pointer_output::validate_pointer_version(version)?;
        if !self.output_queue.bytes.is_empty()
            || self.pointer_revision <= self.delivered_pointer_revision
        {
            return Ok(None);
        }
        self.delivered_pointer_revision = self.pointer_revision;
        Ok(self.pointer_output.clone())
    }

    fn drain_output(&mut self, max_bytes: usize) -> Vec<u8> {
        self.output_queue.drain(max_bytes)
    }
}

#[derive(Debug)]
struct OutputQueue {
    bytes: VecDeque<u8>,
    max_bytes: usize,
}

impl OutputQueue {
    const fn new(max_bytes: usize) -> Self {
        Self {
            bytes: VecDeque::new(),
            max_bytes,
        }
    }

    fn push_bytes(&mut self, data: Vec<u8>) -> Result<()> {
        if data.len() > self.max_bytes.saturating_sub(self.bytes.len()) {
            return Err(MobileCoreError::TerminalBackendFailure(
                "terminal output queue overflow; reconnect for complete presentation repair"
                    .to_string(),
            ));
        }
        self.bytes.extend(data);
        Ok(())
    }

    fn drain(&mut self, max_bytes: usize) -> Vec<u8> {
        let to_read = self.bytes.len().min(max_bytes.max(1));
        self.bytes.drain(..to_read).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminated_session_rejects_pointer_output_but_allows_close() {
        let (response, mut received) = oneshot::channel();
        assert!(reject_terminated_session_command(
            SessionCommand::PointerOutput {
                version: 1,
                response
            },
            "disconnected".to_string(),
        ));
        assert!(
            received
                .try_recv()
                .unwrap()
                .unwrap_err()
                .to_string()
                .contains("disconnected")
        );

        let (response, mut received) = oneshot::channel();
        assert!(!reject_terminated_session_command(
            SessionCommand::Close { response },
            "disconnected".to_string(),
        ));
        assert!(received.try_recv().unwrap().is_ok());
    }

    fn pointer_test_state(pane_id: Uuid) -> StreamOutputState {
        use bmux_attach_layout_protocol::{
            AttachFocusTarget, AttachLayer, AttachScene, AttachSurface, AttachSurfaceKind,
            PaneLayoutNode,
        };
        let viewport = AttachViewport {
            cols: 10,
            rows: 2,
            top_inset: 0,
            right_inset: 0,
            bottom_inset: 0,
            left_inset: 0,
        };
        let mut state = StreamOutputState::new(viewport);
        let rect = AttachRect {
            x: 0,
            y: 0,
            w: 10,
            h: 2,
        };
        state.pipeline.layout_state = Some(bmux_client::AttachLayoutState {
            context_id: None,
            session_id: Uuid::nil(),
            focused_pane_id: pane_id,
            panes: Vec::new(),
            layout_root: PaneLayoutNode::Leaf { pane_id },
            scene: AttachScene {
                session_id: Uuid::nil(),
                focus: AttachFocusTarget::Pane { pane_id },
                surfaces: vec![AttachSurface {
                    id: pane_id,
                    kind: AttachSurfaceKind::Pane,
                    layer: AttachLayer::Pane,
                    z: 0,
                    rect,
                    content_rect: rect,
                    interactive_regions: Vec::new(),
                    opaque: true,
                    visible: true,
                    accepts_input: true,
                    cursor_owner: true,
                    pane_id: Some(pane_id),
                }],
            },
            zoomed: false,
        });
        state.pointer_position = Some((3, 1));
        state
    }

    #[test]
    fn native_pointer_snapshot_frame_and_repair_share_retained_resolution() {
        use crate::pointer_output::TerminalPointerShape;
        use bmux_terminal_grid::{GridLimits, TerminalGridStream};
        use bmux_terminal_protocol::pointer_shape::PointerShape;
        let pane_id = Uuid::new_v4();
        let mut state = pointer_test_state(pane_id);
        let mut producer = TerminalGridStream::new(10, 2, GridLimits::default()).unwrap();
        producer.process(b"\x1b]22;pointer\x07");
        let hydrate = |state: &mut StreamOutputState, producer: &TerminalGridStream| {
            state
                .hydrate_grid_snapshots(vec![AttachPaneGridSnapshotState {
                    pane_id,
                    stream_end: 100,
                    snapshot: producer.snapshot(0, 2),
                }])
                .unwrap();
            state.resolve_pointer();
        };
        hydrate(&mut state, &producer);
        assert_eq!(
            state.take_pointer_output(1).unwrap().unwrap().shape,
            TerminalPointerShape::Shape(PointerShape::Pointer)
        );
        producer.process(b"\x1b]22;crosshair\x07");
        let mut frame = Vec::new();
        producer.grid_mut().write_frame_since(
            bmux_terminal_grid::ReplicaCursor::default(),
            usize::MAX,
            &mut frame,
        );
        assert_eq!(
            state.apply_frames(vec![AttachPaneFrameState {
                pane_id,
                sync_update_active: false,
                frame,
            }]),
            Vec::<Uuid>::new()
        );
        state.resolve_pointer(); // stationary pointer, protocol-only change
        assert_eq!(
            state.take_pointer_output(1).unwrap().unwrap().shape,
            TerminalPointerShape::Shape(PointerShape::Crosshair)
        );
        let mut repaired = pointer_test_state(pane_id);
        hydrate(&mut repaired, &producer);
        assert_eq!(
            state.pointer_output.as_ref().unwrap().shape,
            repaired.pointer_output.as_ref().unwrap().shape
        );
        state.pointer_position = None;
        state.resolve_pointer();
        assert_eq!(
            state.take_pointer_output(1).unwrap().unwrap().shape,
            TerminalPointerShape::Shape(PointerShape::Default)
        );
    }

    #[test]
    fn native_pointer_waits_for_all_bytes_and_unknown_version_does_not_consume() {
        let mut state = pointer_test_state(Uuid::new_v4());
        state.resolve_pointer();
        state.output_queue.push_bytes(vec![1, 2, 3]).unwrap();
        assert!(state.take_pointer_output(1).unwrap().is_none());
        assert_eq!(state.drain_output(2), vec![1, 2]);
        assert!(state.take_pointer_output(1).unwrap().is_none());
        assert_eq!(state.drain_output(2), vec![3]);
        assert!(state.take_pointer_output(2).is_err());
        assert!(state.take_pointer_output(1).unwrap().is_some());
        assert!(state.take_pointer_output(1).unwrap().is_none());
    }

    #[test]
    fn native_queue_overflow_is_explicit_and_preserves_queued_bytes() {
        let mut queue = OutputQueue::new(4);
        queue.push_bytes(vec![1, 2, 3]).unwrap();
        assert!(queue.push_bytes(vec![4, 5]).is_err());
        assert_eq!(queue.drain(10), vec![1, 2, 3]);
    }

    #[test]
    fn native_leave_is_not_application_mouse_input() {
        assert!(
            terminal_mouse_event_to_attach_mouse_event(TerminalMouseEvent {
                kind: TerminalMouseEventKind::Leave,
                button: None,
                row: 0,
                col: 0,
                shift: false,
                alt: false,
                control: false,
            })
            .is_none()
        );
    }

    #[test]
    fn native_pointer_lifetimes_do_not_reuse_generations() {
        let viewport = AttachViewport {
            cols: 10,
            rows: 2,
            top_inset: 0,
            right_inset: 0,
            bottom_inset: 0,
            left_inset: 0,
        };
        let mut first = StreamOutputState::new(viewport);
        let second = StreamOutputState::new(viewport);
        assert_ne!(first.pointer_generation, second.pointer_generation);
        first.resolve_pointer();
        assert_eq!(first.pointer_revision, 1);
        first.resolve_pointer();
        assert_eq!(
            first.pointer_revision, 1,
            "unchanged hover must not spin output polling"
        );
        assert_eq!(
            first.pointer_output.unwrap().shape,
            crate::pointer_output::TerminalPointerShape::Shape(
                bmux_terminal_protocol::pointer_shape::PointerShape::Default
            )
        );
    }
}
