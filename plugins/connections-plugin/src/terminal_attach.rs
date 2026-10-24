//! Single-terminal admitted attach adapter using only authorized execution services.
use anyhow::{Context, Result};
use bmux_attach_layout_protocol::{
    AttachFocusTarget, AttachLayer, AttachRect, AttachScene, AttachSurface, AttachSurfaceKind,
};
use bmux_client::{
    AttachDeltaSequence, AttachDetachOutcome, AttachInputPayload, AttachProviderAck,
    AttachProviderAction, AttachProviderChange, AttachProviderDelta, AttachProviderEvent,
    AttachProviderInput, AttachProviderSnapshot, AttachProviderViewport, AttachResumeState,
    AttachSession, AttachSessionError, AttachSessionFuture, AttachStreamCursor, AttachStreamId,
    AttachStreamSnapshot, AttachViewRevision, BmuxClient,
};
use bmux_config::ConfigPaths;
use bmux_pane_runtime_plugin_api::pane_runtime_execution_v4::{
    TerminalExecutionRef, TerminalProgramOptions, client as execution,
};
use std::{collections::BTreeMap, fmt, time::Duration};

type PendingScreenRead = std::pin::Pin<Box<dyn std::future::Future<Output = Result<(BmuxClient, bmux_pane_runtime_plugin_api::pane_runtime_execution_v4::TerminalExecutionScreen)>> + Send>>;

pub struct RemoteTerminal {
    client: BmuxClient,
    paths: ConfigPaths,
    target: String,
    execution: TerminalExecutionRef,
    grid: bmux_terminal_grid::TerminalGridStream,
    sequence: u64,
    revision: u64,
    scene_dirty: bool,
    pending_read: Option<PendingScreenRead>,
    last_grid: Vec<u8>,
    read_client: Option<BmuxClient>,
    detached: bool,
    controls: bmux_client::AttachControlValidator,
    continuity: bmux_client::AttachContinuityValidator,
}
impl fmt::Debug for RemoteTerminal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("RemoteTerminal { credentials: [redacted] }")
    }
}
fn failed(error: impl fmt::Display) -> AttachSessionError {
    AttachSessionError::Provider {
        reason: error.to_string(),
    }
}
fn operation<T, E: fmt::Debug>(result: std::result::Result<T, E>) -> Result<T> {
    result.map_err(|e| anyhow::anyhow!("remote operation denied or failed: {e:?}"))
}

impl RemoteTerminal {
    /// Open or resume one named terminal. Execution credentials stay in private local storage.
    ///
    /// # Errors
    /// Fails on missing pairing, resource grants, unavailable execution or incompatible service.
    pub async fn open(paths: ConfigPaths, target: String, name: String) -> Result<Self> {
        crate::remote_preferences::name_valid(&name)?;
        let mut client = crate::connect_paired(&paths, &target).await?;
        let key = format!("{target}/{name}");
        let saved =
            crate::remote_preferences::update(&paths, |p| Ok(p.terminals.get(&key).cloned()))?;
        let execution = if let Some(value) = saved {
            serde_json::from_str(&value)?
        } else {
            let reference = operation(
                execution::new_program_session(
                    &mut client,
                    Some(name),
                    TerminalProgramOptions {
                        program: "/bin/sh".into(),
                        args: vec![],
                        cwd: None,
                        env: BTreeMap::new(),
                        cols: 80,
                        rows: 24,
                    },
                )
                .await?,
            )?;
            crate::remote_preferences::update(&paths, |p| {
                p.terminals
                    .insert(key.clone(), serde_json::to_string(&reference)?);
                Ok(())
            })?;
            reference
        };
        let screen = operation(execution::screen(&mut client, execution.clone()).await?)?;
        let snapshot = serde_json::from_slice(&screen.encoded_grid)?;
        let grid = bmux_terminal_grid::TerminalGridStream::from_snapshot(
            &snapshot,
            bmux_terminal_grid::GridLimits::default(),
        )?;
        Ok(Self {
            client,
            paths,
            target,
            execution,
            grid,
            sequence: 0,
            revision: 1,
            scene_dirty: false,
            pending_read: None,
            last_grid: screen.encoded_grid,
            read_client: None,
            detached: false,
            controls: bmux_client::AttachControlValidator::default(),
            continuity: bmux_client::AttachContinuityValidator::default(),
        })
    }
    fn cursor(&self) -> Result<AttachStreamCursor, AttachSessionError> {
        Ok(AttachStreamCursor {
            stream_id: AttachStreamId::new(self.execution.pane_id.to_string())?,
            surface_id: self.execution.pane_id,
            generation: 1,
            offset: self.sequence,
        })
    }
    fn scene(&self) -> AttachScene {
        let snapshot = self.grid.grid().snapshot(0, self.grid.grid().height());
        let rect = AttachRect {
            x: 0,
            y: 0,
            w: snapshot.width,
            h: snapshot.height,
        };
        AttachScene {
            session_id: self.execution.session_id,
            focus: AttachFocusTarget::Pane {
                pane_id: self.execution.pane_id,
            },
            surfaces: vec![AttachSurface {
                id: self.execution.pane_id,
                kind: AttachSurfaceKind::Pane,
                layer: AttachLayer::Pane,
                z: 0,
                rect,
                content_rect: rect,
                interactive_regions: vec![],
                opaque: true,
                visible: true,
                accepts_input: true,
                cursor_owner: true,
                pane_id: Some(self.execution.pane_id),
            }],
        }
    }
    fn resume(&self) -> Result<AttachResumeState, AttachSessionError> {
        Ok(AttachResumeState {
            view_revision: AttachViewRevision(self.revision),
            event_sequence: AttachDeltaSequence(self.sequence),
            streams: vec![self.cursor()?],
            provider_token: vec![],
        })
    }
    const fn ack() -> AttachProviderAck {
        AttachProviderAck {
            command_id: None,
            accepted: true,
            message: None,
        }
    }
    async fn refresh(&mut self) -> Result<(), AttachSessionError> {
        let screen = operation(
            execution::screen(&mut self.client, self.execution.clone())
                .await
                .map_err(failed)?,
        )
        .map_err(failed)?;
        let snapshot = serde_json::from_slice(&screen.encoded_grid).map_err(failed)?;
        self.grid = bmux_terminal_grid::TerminalGridStream::from_snapshot(
            &snapshot,
            bmux_terminal_grid::GridLimits::default(),
        )
        .map_err(failed)?;
        Ok(())
    }
}
impl AttachSession for RemoteTerminal {
    fn initial_snapshot(&mut self) -> AttachSessionFuture<'_, AttachProviderSnapshot> {
        Box::pin(async move {
            self.refresh().await?;
            let snapshot = AttachProviderSnapshot {
                view_revision: AttachViewRevision(self.revision),
                event_sequence: AttachDeltaSequence(self.sequence),
                scene: self.scene(),
                streams: vec![AttachStreamSnapshot {
                    cursor: self.cursor()?,
                    snapshot: bmux_terminal_grid::full_screen_repaint_bytes(self.grid.grid()),
                }],
                resume: self.resume()?,
            };
            self.continuity.apply_snapshot(&snapshot)?;
            Ok(snapshot)
        })
    }
    fn next_event(&mut self) -> AttachSessionFuture<'_, AttachProviderEvent> {
        Box::pin(async move {
            if self.detached {
                return Ok(AttachProviderEvent::Detached);
            }
            loop {
                if self.pending_read.is_none() {
                    let read_client = self.read_client.take();
                    let paths = self.paths.clone();
                    let target = self.target.clone();
                    let reference = self.execution.clone();
                    self.pending_read = Some(Box::pin(async move {
                        tokio::time::sleep(Duration::from_millis(100)).await;
                        let mut client = if let Some(client) = read_client {
                            client
                        } else {
                            crate::connect_paired(&paths, &target).await?
                        };
                        let screen = operation(execution::screen(&mut client, reference).await?)?;
                        Ok((client, screen))
                    }));
                }
                // Keep the whole owned request alive across select cancellation. A read
                // cannot leave an unconsumed response on the input/control connection.
                let screen = self.pending_read.as_mut().expect("pending read").await;
                self.pending_read = None;
                let (client, screen) = screen.map_err(failed)?;
                self.read_client = Some(client);
                if screen.encoded_grid == self.last_grid && !self.scene_dirty {
                    continue;
                }
                let snapshot = serde_json::from_slice(&screen.encoded_grid).map_err(failed)?;
                let grid = bmux_terminal_grid::TerminalGridStream::from_snapshot(
                    &snapshot,
                    bmux_terminal_grid::GridLimits::default(),
                )
                .map_err(failed)?;
                if grid.grid().width() != self.grid.grid().width()
                    || grid.grid().height() != self.grid.grid().height()
                {
                    self.scene_dirty = true;
                }
                self.grid = grid;
                self.last_grid = screen.encoded_grid;
                break;
            }
            let base_revision = self.revision;
            let mut changes = Vec::new();
            if self.scene_dirty {
                self.revision += 1;
                self.scene_dirty = false;
                changes.push(AttachProviderChange::Scene(self.scene()));
            }
            self.sequence += 1;
            changes.push(AttachProviderChange::StreamRepair(AttachStreamSnapshot {
                cursor: self.cursor()?,
                snapshot: bmux_terminal_grid::full_screen_repaint_bytes(self.grid.grid()),
            }));
            let delta = AttachProviderDelta {
                sequence: AttachDeltaSequence(self.sequence),
                base_view_revision: AttachViewRevision(base_revision),
                view_revision: AttachViewRevision(self.revision),
                changes,
                resume: self.resume()?,
            };
            self.continuity.apply_delta(&delta)?;
            Ok(AttachProviderEvent::Delta(delta))
        })
    }
    fn send_input(
        &mut self,
        input: AttachProviderInput,
    ) -> AttachSessionFuture<'_, AttachProviderAck> {
        Box::pin(async move {
            self.controls.validate_input(&input, &self.continuity)?;
            let protocol = self.grid.grid().protocol_state();
            let bytes = match input.payload {
                AttachInputPayload::Bytes(bytes) => bytes,
                AttachInputPayload::Paste(bytes) => {
                    if protocol.bracketed_paste {
                        [b"\x1b[200~".as_slice(), &bytes, b"\x1b[201~"].concat()
                    } else {
                        bytes
                    }
                }
                AttachInputPayload::Key { stroke, enhanced } => {
                    bmux_keyboard::encode::encode_key_with_modes(
                        &stroke,
                        enhanced,
                        bmux_keyboard::encode::KeyEncodingModes {
                            application_cursor: protocol.application_cursor,
                            application_keypad: protocol.application_keypad,
                        },
                    )
                    .context("unsupported key")
                    .map_err(failed)?
                }
                AttachInputPayload::Mouse(_) => {
                    return Ok(AttachProviderAck {
                        accepted: false,
                        message: Some(
                            "mouse reporting is not yet supported for this remote view".into(),
                        ),
                        command_id: None,
                    });
                }
            };
            operation(
                execution::send(&mut self.client, self.execution.clone(), bytes)
                    .await
                    .map_err(failed)?,
            )
            .map_err(failed)?;
            Ok(Self::ack())
        })
    }
    fn update_viewport(
        &mut self,
        viewport: AttachProviderViewport,
    ) -> AttachSessionFuture<'_, AttachProviderAck> {
        Box::pin(async move {
            self.controls.validate_viewport(&viewport)?;
            operation(
                execution::resize(
                    &mut self.client,
                    self.execution.clone(),
                    viewport.columns.max(1),
                    viewport.rows.max(1),
                )
                .await
                .map_err(failed)?,
            )
            .map_err(failed)?;
            self.pending_read = None;
            self.scene_dirty = true;
            Ok(Self::ack())
        })
    }
    fn execute_action(
        &mut self,
        action: AttachProviderAction,
    ) -> AttachSessionFuture<'_, AttachProviderAck> {
        Box::pin(async move {
            self.controls.validate_action(&action)?;
            Ok(AttachProviderAck { command_id: Some(action.command_id), accepted: false, message: Some("single-terminal remote view supports input, resize and detach; local layout actions are unavailable".into()) })
        })
    }
    fn detach(&mut self) -> AttachSessionFuture<'_, AttachDetachOutcome> {
        Box::pin(async move {
            self.detached = true;
            Ok(self.controls.detach())
        })
    }
}
