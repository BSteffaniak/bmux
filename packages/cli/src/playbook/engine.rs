//! Playbook execution engine.
//!
//! Orchestrates the full lifecycle: parse → sandbox → execute steps → report.

use std::io::{IsTerminal, Write};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use bmux_client::BmuxClient;
use bmux_ipc::{InvokeServiceKind, PaneFocusDirection, PaneSplitDirection, SessionSelector};
use bmux_keyboard::{KeyCode as BmuxKeyCode, KeyStroke};
use bmux_plugin_sdk::{PluginCliCommandRequest, PluginCliCommandResponse};
use crossterm::cursor::{Hide, MoveTo, Show};
use crossterm::event::{
    Event as CrosstermEvent, KeyCode as CrosstermKeyCode, KeyEvent, KeyEventKind, KeyEventState,
    KeyModifiers,
};
use crossterm::style::Print;
use crossterm::terminal::{
    self, Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode,
    enable_raw_mode,
};
use crossterm::{execute, queue};
use tracing::{debug, info, warn};
use uuid::Uuid;

use super::RunOptions;
use super::parse_dsl::parse_action_line;
use super::sandbox::SandboxServer;
use super::screen::ScreenInspector;
use super::subst::RuntimeVars;
use super::types::{
    Action, Playbook, PlaybookResult, ServiceKind, SnapshotCapture, SplitDirection, Step,
    StepFailure, StepResult, StepStatus,
};

/// Default timeout for waiting for the sandbox server to start.
const SERVER_STARTUP_TIMEOUT: Duration = Duration::from_secs(15);
const SANDBOX_PHASE_MARKERS: [&str; 4] = [
    "[bmux-attach-phase-json]",
    "[bmux-service-phase-json]",
    "[bmux-ipc-phase-json]",
    "[bmux-storage-phase-json]",
];

/// Max bytes to read from attach output per drain cycle.
const ATTACH_OUTPUT_MAX_BYTES: usize = 256 * 1024;

const VISUAL_RENDER_INTERVAL: Duration = Duration::from_millis(50);
const VISUAL_REFRESH_INTERVAL: Duration = Duration::from_millis(60);
const VISUAL_PAUSE_POLL_INTERVAL: Duration = Duration::from_millis(30);
const ATTACH_PHASE_MARKER: &str = "[bmux-attach-phase-json]";

fn emit_attach_phase_timing(payload: &serde_json::Value) {
    if std::env::var_os("BMUX_ATTACH_PHASE_TIMING").is_none() {
        return;
    }
    eprintln!("{ATTACH_PHASE_MARKER}{payload}");
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PlaybookInteractiveMode {
    Disabled,
    Prompt,
    Visual,
}

const fn resolve_interactive_mode(
    interactive_requested: bool,
    stdin_is_tty: bool,
    stdout_is_tty: bool,
) -> PlaybookInteractiveMode {
    if !interactive_requested {
        return PlaybookInteractiveMode::Disabled;
    }
    if stdin_is_tty && stdout_is_tty {
        PlaybookInteractiveMode::Visual
    } else {
        PlaybookInteractiveMode::Prompt
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VisualCheckpointPhase {
    BeforeStep,
    InStep,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VisualControlAction {
    TogglePause,
    StepOnce,
    ContinueLive,
    Abort,
    Help,
    PromptDsl,
}

#[derive(Debug)]
struct InteractiveAbort;

impl std::fmt::Display for InteractiveAbort {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "interactive run aborted by operator")
    }
}

impl std::error::Error for InteractiveAbort {}

const fn parse_visual_control_action(key: KeyEvent) -> Option<VisualControlAction> {
    if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
        return None;
    }

    if key.modifiers.contains(KeyModifiers::CONTROL)
        && matches!(key.code, CrosstermKeyCode::Char('c' | 'd'))
    {
        return Some(VisualControlAction::Abort);
    }

    match key.code {
        CrosstermKeyCode::Char(' ') => Some(VisualControlAction::TogglePause),
        CrosstermKeyCode::Char('n') => Some(VisualControlAction::StepOnce),
        CrosstermKeyCode::Char('c' | 'l') => Some(VisualControlAction::ContinueLive),
        CrosstermKeyCode::Char('q') | CrosstermKeyCode::Esc => Some(VisualControlAction::Abort),
        CrosstermKeyCode::Char(':') => Some(VisualControlAction::PromptDsl),
        CrosstermKeyCode::Char('?' | 'h') => Some(VisualControlAction::Help),
        _ => None,
    }
}

struct VisualTerminalGuard {
    active: bool,
}

impl VisualTerminalGuard {
    fn enter() -> Result<Self> {
        enable_raw_mode().context("failed enabling raw mode for visual playbook mode")?;
        if let Err(error) = execute!(std::io::stdout(), EnterAlternateScreen, Hide) {
            let _ = disable_raw_mode();
            return Err(anyhow::anyhow!(
                "failed entering alternate screen for visual playbook mode: {error}"
            ));
        }
        Ok(Self { active: true })
    }

    fn suspend_for_line_input(&mut self) -> Result<()> {
        if !self.active {
            return Ok(());
        }
        execute!(std::io::stdout(), Show, LeaveAlternateScreen)
            .context("failed leaving alternate screen for DSL prompt")?;
        disable_raw_mode().context("failed disabling raw mode for DSL prompt")?;
        self.active = false;
        Ok(())
    }

    fn resume_after_line_input(&mut self) -> Result<()> {
        if self.active {
            return Ok(());
        }
        enable_raw_mode().context("failed re-enabling raw mode for visual mode")?;
        execute!(std::io::stdout(), EnterAlternateScreen, Hide)
            .context("failed restoring alternate screen for visual mode")?;
        self.active = true;
        Ok(())
    }
}

impl Drop for VisualTerminalGuard {
    fn drop(&mut self) {
        if self.active {
            let _ = execute!(std::io::stdout(), Show, LeaveAlternateScreen);
            let _ = disable_raw_mode();
        }
    }
}

#[allow(clippy::struct_excessive_bools)]
pub(super) struct VisualInteractiveState {
    terminal: VisualTerminalGuard,
    paused: bool,
    step_once_requested: bool,
    single_step_inflight: bool,
    abort_requested: bool,
    status_line: String,
    current_step_label: String,
    current_step_position: usize,
    total_steps: usize,
    last_step_line: String,
    started_at: Instant,
    last_render_at: Instant,
    last_refresh_at: Instant,
}

impl VisualInteractiveState {
    fn enter(total_steps: usize) -> Result<Self> {
        let now = Instant::now();
        Ok(Self {
            terminal: VisualTerminalGuard::enter()?,
            paused: true,
            step_once_requested: false,
            single_step_inflight: false,
            abort_requested: false,
            status_line: "paused (press n to step, c/l to run live)".to_string(),
            current_step_label: "<waiting>".to_string(),
            current_step_position: 0,
            total_steps,
            last_step_line: String::new(),
            started_at: now,
            last_render_at: now.checked_sub(VISUAL_RENDER_INTERVAL).unwrap_or(now),
            last_refresh_at: now.checked_sub(VISUAL_REFRESH_INTERVAL).unwrap_or(now),
        })
    }

    fn set_current_step(&mut self, step_position: usize, step: &Step) {
        self.current_step_position = step_position;
        self.current_step_label = step.to_dsl();
        self.force_render();
    }

    fn mark_step_result(
        &mut self,
        step_position: usize,
        action_name: &str,
        status: StepStatus,
        elapsed_ms: u128,
        detail: Option<&str>,
    ) {
        let symbol = match status {
            StepStatus::Pass => "+",
            StepStatus::Fail => "-",
            StepStatus::Skip => "~",
        };
        self.last_step_line = if let Some(detail) = detail {
            format!(
                "[{symbol}] step {}/{} {action_name} ({elapsed_ms}ms) {detail}",
                step_position + 1,
                self.total_steps,
            )
        } else {
            format!(
                "[{symbol}] step {}/{} {action_name} ({elapsed_ms}ms)",
                step_position + 1,
                self.total_steps,
            )
        };

        if self.single_step_inflight {
            self.single_step_inflight = false;
            self.paused = true;
            self.step_once_requested = false;
            self.status_line = "paused after single-step".to_string();
        }
        self.force_render();
    }

    fn mark_status(&mut self, status: impl Into<String>) {
        self.status_line = status.into();
        self.force_render();
    }

    fn force_render(&mut self) {
        let now = Instant::now();
        self.last_render_at = now.checked_sub(VISUAL_RENDER_INTERVAL).unwrap_or(now);
    }

    #[allow(clippy::unused_self)]
    fn parse_next_control_action(&self) -> Result<Option<VisualControlAction>> {
        loop {
            if !crossterm::event::poll(Duration::ZERO).context("failed polling visual controls")? {
                return Ok(None);
            }
            if let crossterm::event::Event::Key(key) =
                crossterm::event::read().context("failed reading visual control event")?
                && let Some(action) = parse_visual_control_action(key)
            {
                return Ok(Some(action));
            }
        }
    }

    fn apply_control_action(
        &mut self,
        action: VisualControlAction,
        phase: VisualCheckpointPhase,
    ) -> bool {
        match action {
            VisualControlAction::TogglePause => {
                self.paused = !self.paused;
                self.status_line = if self.paused {
                    "paused".to_string()
                } else {
                    "resumed".to_string()
                };
                self.step_once_requested = false;
                self.force_render();
                false
            }
            VisualControlAction::StepOnce => {
                match phase {
                    VisualCheckpointPhase::BeforeStep => {
                        self.paused = true;
                        self.step_once_requested = true;
                        self.status_line = "single-step armed".to_string();
                    }
                    VisualCheckpointPhase::InStep => {
                        self.paused = false;
                        self.status_line = "resumed current step".to_string();
                    }
                }
                self.force_render();
                false
            }
            VisualControlAction::ContinueLive => {
                self.paused = false;
                self.step_once_requested = false;
                self.status_line = "running live".to_string();
                self.force_render();
                false
            }
            VisualControlAction::Abort => {
                self.abort_requested = true;
                self.status_line = "abort requested".to_string();
                self.force_render();
                false
            }
            VisualControlAction::Help => {
                self.status_line =
                    "space pause/resume | n step | c/l live | : ad-hoc dsl | q quit".to_string();
                self.force_render();
                false
            }
            VisualControlAction::PromptDsl => true,
        }
    }

    fn prompt_for_dsl_command(&mut self) -> Result<Option<String>> {
        self.terminal.suspend_for_line_input()?;

        let read_result = (|| -> Result<Option<String>> {
            eprint!("playbook:dsl> ");
            std::io::stderr()
                .flush()
                .context("failed flushing visual DSL prompt")?;
            let mut line = String::new();
            let read = std::io::stdin()
                .read_line(&mut line)
                .context("failed reading visual DSL command")?;
            if read == 0 {
                return Ok(None);
            }
            let trimmed = line.trim();
            if trimmed.is_empty() {
                Ok(None)
            } else {
                Ok(Some(trimmed.to_string()))
            }
        })();

        let resume_result = self.terminal.resume_after_line_input();
        resume_result?;

        self.force_render();
        read_result
    }

    async fn maybe_refresh_and_render(
        &mut self,
        client: &mut BmuxClient,
        inspector: &mut ScreenInspector,
        session_id: Option<Uuid>,
        attached: bool,
        force: bool,
    ) -> Result<()> {
        let now = Instant::now();
        if force || now.duration_since(self.last_refresh_at) >= VISUAL_REFRESH_INTERVAL {
            if attached
                && let Some(sid) = session_id
                && let Err(error) = inspector.refresh(client, sid).await
            {
                self.status_line = format!("screen refresh failed: {error:#}");
            }
            self.last_refresh_at = now;
        }

        if force || now.duration_since(self.last_render_at) >= VISUAL_RENDER_INTERVAL {
            self.render(inspector, attached, session_id)?;
            self.last_render_at = Instant::now();
        }

        Ok(())
    }

    fn render(
        &self,
        inspector: &ScreenInspector,
        attached: bool,
        session_id: Option<Uuid>,
    ) -> Result<()> {
        let (cols, rows) = terminal::size().unwrap_or((80, 24));
        let cols_usize = usize::from(cols.max(1));
        let rows_usize = usize::from(rows.max(1));

        let mut lines = Vec::new();
        let mode = if self.abort_requested {
            "ABORTING"
        } else if self.paused {
            "PAUSED"
        } else {
            "RUNNING"
        };
        lines.push(truncate_display_line(
            &format!(
                "bmux playbook live tour [{mode}] step {}/{} elapsed {}ms",
                self.current_step_position.saturating_add(1),
                self.total_steps,
                self.started_at.elapsed().as_millis(),
            ),
            cols_usize,
        ));
        lines.push(truncate_display_line(
            "keys: space pause/resume | n step | c/l live | : dsl | q quit | ? help",
            cols_usize,
        ));
        lines.push(truncate_display_line(
            &format!("step: {}", self.current_step_label),
            cols_usize,
        ));
        let last_line = if self.last_step_line.is_empty() {
            "last: <none>".to_string()
        } else {
            format!("last: {}", self.last_step_line)
        };
        lines.push(truncate_display_line(&last_line, cols_usize));
        lines.push(truncate_display_line(
            &format!(
                "status: {} | session: {}",
                self.status_line,
                session_id.map_or_else(|| "none".to_string(), |id| id.to_string())
            ),
            cols_usize,
        ));
        lines.push("-".repeat(cols_usize));

        if !attached {
            lines.push(truncate_display_line(
                "waiting for session attach (run will start with new-session)",
                cols_usize,
            ));
        } else if let Some(panes) = inspector.capture_all_safe() {
            if panes.is_empty() {
                lines.push(truncate_display_line("no panes captured", cols_usize));
            } else {
                for pane in panes {
                    let focus_marker = if pane.focused { "*" } else { " " };
                    lines.push(truncate_display_line(
                        &format!(
                            "[{focus_marker}] pane {} cursor {}:{}",
                            pane.index,
                            pane.cursor_row.saturating_add(1),
                            pane.cursor_col.saturating_add(1)
                        ),
                        cols_usize,
                    ));
                    for pane_line in pane.screen_text.lines() {
                        lines.push(truncate_display_line(pane_line, cols_usize));
                        if lines.len() >= rows_usize {
                            break;
                        }
                    }
                    if lines.len() >= rows_usize {
                        break;
                    }
                    lines.push(String::new());
                }
            }
        } else {
            lines.push(truncate_display_line(
                "waiting for first screen snapshot",
                cols_usize,
            ));
        }

        if lines.len() > rows_usize {
            lines.truncate(rows_usize);
        }

        let mut stdout = std::io::stdout().lock();
        queue!(stdout, MoveTo(0, 0), Clear(ClearType::All))
            .context("failed clearing visual playbook frame")?;
        for (row, line) in lines.iter().enumerate() {
            let row = u16::try_from(row).unwrap_or(u16::MAX);
            queue!(stdout, MoveTo(0, row), Print(line))
                .context("failed writing visual playbook frame line")?;
        }
        stdout
            .flush()
            .context("failed flushing visual playbook frame")?;
        Ok(())
    }
}

fn truncate_display_line(input: &str, max_cols: usize) -> String {
    if max_cols == 0 {
        return String::new();
    }
    let mut out = String::new();
    for ch in input.chars().take(max_cols) {
        out.push(ch);
    }
    out
}

pub(super) struct AttachInputRuntime {
    processor: crate::input::InputProcessor,
    state: AttachInputState,
}

#[derive(Debug, Clone)]
pub(super) struct AttachInputState {
    attached_id: Uuid,
    attached_context_id: Option<Uuid>,
    window_context_ids: Vec<Uuid>,
    scrollback_active: bool,
    scrollback_offset: usize,
}

impl AttachInputRuntime {
    fn new(attach_info: bmux_client::AttachOpenInfo) -> Self {
        let config = bmux_config::BmuxConfig::default();
        let timeout_ms = config
            .keybindings
            .resolve_timeout()
            .map(|timeout| timeout.timeout_ms())
            .unwrap_or(None);
        let modes = config
            .keybindings
            .modes
            .iter()
            .map(|(mode_id, mode)| {
                (
                    mode_id.clone(),
                    crate::input::ModalModeConfig {
                        label: mode.label.clone(),
                        passthrough: mode.passthrough,
                        bindings: mode.bindings.clone(),
                    },
                )
            })
            .collect();
        let keymap = crate::input::Keymap::from_modal_parts_with_scroll(
            timeout_ms,
            &config.keybindings.initial_mode,
            &modes,
            &config.keybindings.global,
            &config.keybindings.scroll,
        )
        .unwrap_or_else(|_| crate::input::Keymap::default_runtime());
        Self {
            processor: crate::input::InputProcessor::new(keymap, false),
            state: AttachInputState {
                attached_id: attach_info.session_id,
                attached_context_id: attach_info.context_id,
                window_context_ids: attach_info.context_id.into_iter().collect(),
                scrollback_active: false,
                scrollback_offset: 0,
            },
        }
    }
}

/// Invoke a `windows-commands` typed operation on a [`BmuxClient`] by
/// routing through `Request::InvokeService`. Mirrors the pattern the
/// attach runtime uses on its `StreamingBmuxClient` handle; separate
/// helpers exist because the two client types share no trait.
async fn invoke_windows_command_bmux<Req, Resp>(
    client: &mut BmuxClient,
    operation: &str,
    args: &Req,
) -> anyhow::Result<Resp>
where
    Req: serde::Serialize + Sync,
    Resp: serde::de::DeserializeOwned,
{
    let payload = bmux_codec::to_vec(args)
        .map_err(|error| anyhow::anyhow!("encoding {operation}: {error}"))?;
    let response_bytes = client
        .invoke_service_raw(
            crate::runtime::typed_windows::WINDOWS_WRITE_CAPABILITY.as_str(),
            bmux_ipc::InvokeServiceKind::Command,
            crate::runtime::typed_windows::WINDOWS_COMMANDS_INTERFACE.as_str(),
            operation,
            payload,
        )
        .await
        .map_err(|e| anyhow::anyhow!("client invoke_service_raw failed: {e}"))?;
    bmux_codec::from_bytes::<Resp>(&response_bytes)
        .map_err(|error| anyhow::anyhow!("decoding {operation} response: {error}"))
}

async fn switch_window_by_id_playbook(client: &mut BmuxClient, id: Uuid) -> anyhow::Result<()> {
    let _ack =
        invoke_windows_command_bmux::<_, bmux_windows_plugin_api::windows_commands::WindowAck>(
            client,
            "switch-window",
            &id.to_string(),
        )
        .await?;
    Ok(())
}

#[derive(Debug, Clone, Copy, Default)]
struct PlaybookWindowCycleTiming {
    known_contexts: bool,
    resolve_us: u128,
    invoke_us: u128,
    fallback_us: u128,
    total_us: u128,
}

async fn cycle_window_playbook(client: &mut BmuxClient, reverse: bool) -> anyhow::Result<()> {
    let contexts = list_contexts_playbook(client).await?;
    if contexts.len() < 2 {
        return Err(anyhow::anyhow!("no alternate window available"));
    }
    let current_context = current_context_playbook(client).await?;
    let current_index = current_context
        .and_then(|current| contexts.iter().position(|context| context.id == current.id))
        .unwrap_or(0);
    let target_index = if reverse {
        (current_index + contexts.len() - 1) % contexts.len()
    } else {
        (current_index + 1) % contexts.len()
    };
    switch_window_by_id_playbook(client, contexts[target_index].id).await
}

async fn cycle_known_window_playbook(
    client: &mut BmuxClient,
    runtime: &AttachInputRuntime,
    reverse: bool,
) -> anyhow::Result<(Uuid, PlaybookWindowCycleTiming)> {
    let total_started = Instant::now();
    let contexts = &runtime.state.window_context_ids;
    if contexts.len() < 2 {
        let fallback_started = Instant::now();
        cycle_window_playbook(client, reverse).await?;
        let context_id = current_context_playbook(client)
            .await?
            .map(|context| context.id)
            .ok_or_else(|| anyhow::anyhow!("current context unavailable after window switch"))?;
        return Ok((
            context_id,
            PlaybookWindowCycleTiming {
                fallback_us: fallback_started.elapsed().as_micros(),
                total_us: total_started.elapsed().as_micros(),
                ..PlaybookWindowCycleTiming::default()
            },
        ));
    }
    let resolve_started = Instant::now();
    let current_index = runtime
        .state
        .attached_context_id
        .and_then(|current| contexts.iter().position(|context| *context == current))
        .unwrap_or(0);
    let target_index = if reverse {
        (current_index + contexts.len() - 1) % contexts.len()
    } else {
        (current_index + 1) % contexts.len()
    };
    let target_id = contexts[target_index];
    let resolve_us = resolve_started.elapsed().as_micros();
    let invoke_started = Instant::now();
    switch_window_by_id_playbook(client, target_id).await?;
    Ok((
        target_id,
        PlaybookWindowCycleTiming {
            known_contexts: true,
            resolve_us,
            invoke_us: invoke_started.elapsed().as_micros(),
            total_us: total_started.elapsed().as_micros(),
            ..PlaybookWindowCycleTiming::default()
        },
    ))
}

async fn goto_known_window_playbook(
    client: &mut BmuxClient,
    runtime: &AttachInputRuntime,
    args: &[String],
) -> anyhow::Result<(Uuid, PlaybookWindowCycleTiming)> {
    let total_started = Instant::now();
    let target_index = args
        .first()
        .ok_or_else(|| anyhow::anyhow!("goto-window requires an index argument"))?
        .parse::<usize>()
        .map_err(|error| anyhow::anyhow!("invalid goto-window index: {error}"))?
        .checked_sub(1)
        .ok_or_else(|| anyhow::anyhow!("goto-window index must be at least 1"))?;
    let contexts = &runtime.state.window_context_ids;
    if contexts.is_empty() {
        return Err(anyhow::anyhow!("known window context list is empty"));
    }
    let resolve_started = Instant::now();
    let Some(target_id) = contexts.get(target_index).copied() else {
        return Err(anyhow::anyhow!(
            "goto-window index {} is out of range for {} known windows",
            target_index + 1,
            contexts.len()
        ));
    };
    let resolve_us = resolve_started.elapsed().as_micros();
    let invoke_started = Instant::now();
    switch_window_by_id_playbook(client, target_id).await?;
    Ok((
        target_id,
        PlaybookWindowCycleTiming {
            known_contexts: true,
            resolve_us,
            invoke_us: invoke_started.elapsed().as_micros(),
            total_us: total_started.elapsed().as_micros(),
            ..PlaybookWindowCycleTiming::default()
        },
    ))
}

async fn run_known_attach_plugin_command_playbook(
    client: &mut BmuxClient,
    plugin_id: &str,
    command_name: &str,
    args: &[String],
) -> anyhow::Result<Option<PluginCliCommandResponse>> {
    if plugin_id != "bmux.windows" {
        return Ok(None);
    }
    match command_name {
        "new-window" => {
            let name = args.first().cloned();
            let _ack = invoke_windows_command_bmux::<
                _,
                bmux_windows_plugin_api::windows_commands::WindowAck,
            >(client, "new-window", &name)
            .await?;
            Ok(Some(PluginCliCommandResponse::new(0)))
        }
        "next-window" => {
            cycle_window_playbook(client, false).await?;
            Ok(Some(PluginCliCommandResponse::new(0)))
        }
        "prev-window" => {
            cycle_window_playbook(client, true).await?;
            Ok(Some(PluginCliCommandResponse::new(0)))
        }
        _ => Ok(None),
    }
}

/// Invoke a `sessions-commands` typed operation on a [`BmuxClient`].
async fn invoke_sessions_command_bmux<Req, Resp>(
    client: &mut BmuxClient,
    operation: &str,
    args: &Req,
) -> anyhow::Result<Resp>
where
    Req: serde::Serialize + Sync,
    Resp: serde::de::DeserializeOwned,
{
    let payload = bmux_codec::to_vec(args)
        .map_err(|error| anyhow::anyhow!("encoding {operation}: {error}"))?;
    let response_bytes = client
        .invoke_service_raw(
            crate::runtime::typed_sessions::SESSIONS_WRITE_CAPABILITY.as_str(),
            bmux_ipc::InvokeServiceKind::Command,
            crate::runtime::typed_sessions::SESSIONS_COMMANDS_INTERFACE.as_str(),
            operation,
            payload,
        )
        .await
        .map_err(|e| anyhow::anyhow!("client invoke_service_raw failed: {e}"))?;
    bmux_codec::from_bytes::<Resp>(&response_bytes)
        .map_err(|error| anyhow::anyhow!("decoding {operation} response: {error}"))
}

async fn current_context_playbook(
    client: &mut BmuxClient,
) -> anyhow::Result<Option<bmux_contexts_plugin_api::contexts_state::ContextSummary>> {
    let payload = bmux_codec::to_vec(&())
        .map_err(|error| anyhow::anyhow!("encoding current-context args: {error}"))?;
    let response_bytes = client
        .invoke_service_raw(
            crate::runtime::typed_contexts::CONTEXTS_READ_CAPABILITY.as_str(),
            crate::runtime::typed_contexts::QUERY_KIND,
            crate::runtime::typed_contexts::CONTEXTS_STATE_INTERFACE.as_str(),
            crate::runtime::typed_contexts::OP_CURRENT_CONTEXT,
            payload,
        )
        .await
        .map_err(|e| anyhow::anyhow!("current-context failed: {e}"))?;
    bmux_codec::from_bytes(&response_bytes)
        .map_err(|error| anyhow::anyhow!("decoding current-context response: {error}"))
}

async fn list_contexts_playbook(
    client: &mut BmuxClient,
) -> anyhow::Result<Vec<bmux_contexts_plugin_api::contexts_state::ContextSummary>> {
    let payload = bmux_codec::to_vec(&())
        .map_err(|error| anyhow::anyhow!("encoding list-contexts args: {error}"))?;
    let response_bytes = client
        .invoke_service_raw(
            crate::runtime::typed_contexts::CONTEXTS_READ_CAPABILITY.as_str(),
            crate::runtime::typed_contexts::QUERY_KIND,
            crate::runtime::typed_contexts::CONTEXTS_STATE_INTERFACE.as_str(),
            crate::runtime::typed_contexts::OP_LIST_CONTEXTS,
            payload,
        )
        .await
        .map_err(|e| anyhow::anyhow!("list-contexts failed: {e}"))?;
    bmux_codec::from_bytes(&response_bytes)
        .map_err(|error| anyhow::anyhow!("decoding list-contexts response: {error}"))
}

async fn retarget_attach_to_current_context_playbook(
    client: &mut BmuxClient,
    inspector: &ScreenInspector,
    runtime: &mut AttachInputRuntime,
    plugin_id: Option<&str>,
    command_name: Option<&str>,
) -> anyhow::Result<()> {
    let current_started = Instant::now();
    let Some(context) = current_context_playbook(client).await? else {
        return Ok(());
    };
    let current_context_us = current_started.elapsed().as_micros();
    retarget_attach_to_context_playbook(
        client,
        inspector,
        runtime,
        context.id,
        current_context_us,
        plugin_id,
        command_name,
    )
    .await
}

async fn retarget_attach_to_context_playbook(
    client: &mut BmuxClient,
    inspector: &ScreenInspector,
    runtime: &mut AttachInputRuntime,
    context_id: Uuid,
    current_context_us: u128,
    plugin_id: Option<&str>,
    command_name: Option<&str>,
) -> anyhow::Result<()> {
    let total_started = Instant::now();
    let from_session_id = runtime.state.attached_id;
    let (cols, rows) = inspector.viewport_size();
    let attach_info = client
        .retarget_attach_context(context_id, cols, rows)
        .await
        .map_err(|e| anyhow::anyhow!("attach context retarget failed: {e}"))?;
    let retarget_service_us = total_started.elapsed().as_micros();
    runtime.state.attached_id = attach_info.session_id;
    runtime.state.attached_context_id = attach_info.context_id;
    emit_attach_phase_timing(&serde_json::json!({
        "phase": "attach.retarget_context",
        "plugin_id": plugin_id,
        "command_name": command_name,
        "from_session_id": from_session_id,
        "to_context_id": context_id,
        "selected_context_id": attach_info.context_id,
        "selected_session_id": attach_info.session_id,
        "current_context_us": current_context_us,
        "retarget_service_us": retarget_service_us,
        "grant_us": 0_u128,
        "open_us": retarget_service_us,
        "viewport_us": 0_u128,
        "total_us": total_started.elapsed().as_micros(),
    }));
    Ok(())
}

async fn run_plugin_command_playbook(
    client: &mut BmuxClient,
    plugin_id: &str,
    command_name: &str,
    args: Vec<String>,
) -> anyhow::Result<PluginCliCommandResponse> {
    if let Some(response) =
        run_known_attach_plugin_command_playbook(client, plugin_id, command_name, &args).await?
    {
        return Ok(response);
    }
    let request =
        PluginCliCommandRequest::new(plugin_id.to_string(), command_name.to_string(), args);
    let payload = bmux_plugin_sdk::encode_service_message(&request)
        .context("failed encoding plugin command request")?;
    let response_payload = client
        .invoke_service_raw(
            "bmux.commands",
            InvokeServiceKind::Command,
            "cli-command/v1",
            "run_plugin",
            payload,
        )
        .await
        .map_err(|e| anyhow::anyhow!("plugin command bridge failed: {e}"))?;
    bmux_plugin_sdk::decode_service_message(&response_payload)
        .context("failed decoding plugin command response")
}

/// Invoke the typed `sessions-commands:new-session` operation. Returns
/// the new session's id on success.
async fn typed_new_session_playbook(
    client: &mut BmuxClient,
    name: Option<String>,
) -> anyhow::Result<Uuid> {
    #[derive(serde::Serialize)]
    struct Args {
        name: Option<String>,
    }
    let outcome = invoke_sessions_command_bmux::<
        _,
        std::result::Result<
            bmux_sessions_plugin_api::sessions_commands::SessionAck,
            bmux_sessions_plugin_api::sessions_commands::NewSessionError,
        >,
    >(client, "new-session", &Args { name })
    .await?;
    outcome
        .map(|ack| ack.id)
        .map_err(|err| anyhow::anyhow!("new-session failed: {err:?}"))
}

/// Invoke the typed `sessions-commands:kill-session` operation.
/// Returns the killed session's id on success.
async fn typed_kill_session_playbook(
    client: &mut BmuxClient,
    selector: SessionSelector,
) -> anyhow::Result<Uuid> {
    let args = crate::runtime::typed_sessions::KillSessionArgs {
        selector: crate::runtime::typed_sessions::from_ipc_selector(selector),
        force_local: false,
    };
    let outcome = invoke_sessions_command_bmux::<
        _,
        std::result::Result<
            bmux_sessions_plugin_api::sessions_commands::SessionAck,
            bmux_sessions_plugin_api::sessions_commands::KillSessionError,
        >,
    >(client, "kill-session", &args)
    .await?;
    outcome
        .map(|ack| ack.id)
        .map_err(|err| anyhow::anyhow!("kill-session failed: {err:?}"))
}

/// Run a playbook to completion, returning the result.
///
/// Handles Ctrl+C gracefully: on signal, the sandbox server is cleaned up
/// via `SandboxServer`'s `Drop` impl.
///
/// # Errors
///
/// Returns an error if the playbook execution fails (server startup, IPC, etc.).
pub async fn run_playbook(
    playbook: Playbook,
    target_server: bool,
    options: RunOptions,
) -> Result<PlaybookResult> {
    tokio::select! {
        result = run_playbook_inner(playbook, target_server, options) => result,
        _ = tokio::signal::ctrl_c() => {
            // The sandbox (if any) will be cleaned up by Drop when the
            // run_playbook_inner future is dropped by select!.
            info!("playbook interrupted by signal");
            Err(anyhow::anyhow!("interrupted by signal"))
        }
    }
}

/// Core playbook execution logic.
#[allow(clippy::too_many_lines)]
async fn run_playbook_inner(
    playbook: Playbook,
    target_server: bool,
    options: RunOptions,
) -> Result<PlaybookResult> {
    let started = Instant::now();
    let playbook_name = playbook.config.name.clone();
    let should_record = playbook.config.record;

    let mut step_results = Vec::new();
    let mut snapshots = Vec::new();
    let mut error_msg: Option<String> = None;
    let mut recording_id: Option<Uuid> = None;

    // Either connect to an existing server or spin up a sandbox.
    let sandbox: Option<SandboxServer>;
    let mut client: BmuxClient;

    if target_server {
        sandbox = None;
        client = BmuxClient::connect_default("bmux-playbook-runner")
            .await
            .map_err(|e| anyhow::anyhow!("failed connecting to live server: {e}"))?;
    } else {
        let sb = SandboxServer::start(
            playbook.config.shell.as_deref(),
            &playbook.config.plugins,
            SERVER_STARTUP_TIMEOUT,
            &playbook.config.env,
            playbook.config.effective_env_mode(),
            playbook.config.binary.as_deref(),
            &playbook.config.bundled_plugin_ids,
        )
        .await
        .context("failed starting sandbox server")?;
        client = sb.connect("bmux-playbook-runner").await?;
        sandbox = Some(sb);
    }

    let mut inspector =
        ScreenInspector::new(playbook.config.viewport.cols, playbook.config.viewport.rows);

    // Runtime variable context for substitution
    let mut runtime_vars = RuntimeVars::new(playbook.config.vars.clone());

    // Session tracking
    let mut session_id: Option<Uuid> = None;
    let mut attached = false;
    let mut events_subscribed = false;
    let mut attach_runtime: Option<AttachInputRuntime> = None;
    let mut display_track: Option<super::display_track::PlaybookDisplayTrackWriter> = None;

    // Start recording before any steps execute so that all events (including
    // NewSession) are captured. Uses session_id: None since no session exists
    // yet — the sandbox is ephemeral so there's no noise from other sessions.
    if should_record {
        match start_recording(&mut client, None).await {
            Ok(rid) => {
                info!("recording started: {rid}");
                recording_id = Some(rid);

                // Create display track writer for GIF export.
                if let Some(ref sb) = sandbox {
                    let rec_dir = sb.paths().recordings_dir().join(rid.to_string());
                    let client_id = bmux_clients_plugin_api::typed_client::whoami(&mut client)
                        .await
                        .unwrap_or_else(|_| Uuid::new_v4());
                    match super::display_track::PlaybookDisplayTrackWriter::new(
                        &rec_dir,
                        client_id,
                        rid,
                        playbook.config.viewport.cols,
                        playbook.config.viewport.rows,
                    ) {
                        Ok(dt) => {
                            display_track = Some(dt);
                        }
                        Err(e) => {
                            warn!("failed to create display track: {e:#}");
                        }
                    }
                }
            }
            Err(e) => {
                warn!("failed to start recording: {e:#}");
                // Non-fatal — continue without recording.
            }
        }
    }

    // Execute each step
    let playbook_start = Instant::now();
    let deadline = playbook_start + playbook.config.timeout;
    let total_steps = playbook.steps.len();
    let interactive_mode = resolve_interactive_mode(
        options.interactive,
        std::io::stdin().is_terminal(),
        std::io::stdout().is_terminal(),
    );
    let mut interactive_prompt_active = matches!(interactive_mode, PlaybookInteractiveMode::Prompt);
    let mut visual_interactive = if matches!(interactive_mode, PlaybookInteractiveMode::Visual) {
        Some(VisualInteractiveState::enter(total_steps)?)
    } else {
        None
    };
    let mut interactive_abort_from_step: Option<usize> = None;

    if matches!(interactive_mode, PlaybookInteractiveMode::Prompt) && options.interactive {
        eprintln!(
            "bmux: --interactive visual live tour requires a TTY; using prompt fallback controls"
        );
        eprintln!(
            "interactive playbook controls: n next | c/l continue | s screen | :<dsl> command | q quit"
        );
    }

    for (step_position, step) in playbook.steps.iter().enumerate() {
        if let Some(ref mut visual_state) = visual_interactive {
            let prompt_decision = visual_wait_for_step_permission(
                visual_state,
                step,
                step_position,
                total_steps,
                &mut client,
                &mut inspector,
                &mut session_id,
                &mut attached,
                &mut events_subscribed,
                &mut attach_runtime,
                &playbook.config.viewport.cols,
                &playbook.config.viewport.rows,
                &mut snapshots,
                deadline,
                &mut display_track,
                &mut runtime_vars,
            )
            .await?;

            match prompt_decision {
                InteractivePromptDecision::RunNextStep => {}
                InteractivePromptDecision::ContinueRemaining => {
                    visual_state.mark_status("running live");
                }
                InteractivePromptDecision::AbortRun => {
                    interactive_abort_from_step = Some(step_position);
                    error_msg = Some(format!(
                        "interactive run aborted before step {} ({})",
                        step.index,
                        step.action.name()
                    ));
                    break;
                }
            }
        } else if interactive_prompt_active {
            let prompt_decision = interactive_step_prompt(
                step,
                step_position,
                total_steps,
                &mut client,
                &mut inspector,
                &mut session_id,
                &mut attached,
                &mut events_subscribed,
                &mut attach_runtime,
                &playbook.config.viewport.cols,
                &playbook.config.viewport.rows,
                &mut snapshots,
                deadline,
                &mut display_track,
                &mut runtime_vars,
                &mut visual_interactive,
            )
            .await?;

            match prompt_decision {
                InteractivePromptDecision::RunNextStep => {}
                InteractivePromptDecision::ContinueRemaining => {
                    interactive_prompt_active = false;
                }
                InteractivePromptDecision::AbortRun => {
                    interactive_abort_from_step = Some(step_position);
                    error_msg = Some(format!(
                        "interactive run aborted before step {} ({})",
                        step.index,
                        step.action.name()
                    ));
                    break;
                }
            }
        }

        if Instant::now() > deadline {
            let elapsed = playbook_start.elapsed().as_millis();
            error_msg = Some(format!(
                "playbook timeout exceeded after {elapsed}ms (at step {}: {})",
                step.index,
                step.action.name()
            ));
            step_results.push(StepResult {
                index: step.index,
                action: step.action.name().to_string(),
                status: StepStatus::Skip,
                elapsed_ms: 0,
                detail: Some("skipped: playbook timeout".to_string()),
                expected: None,
                actual: None,
                failure_captures: None,
                continue_on_error: step.continue_on_error,
            });
        }

        let step_start = Instant::now();
        if playbook.config.verbose {
            eprint!(
                "[{}/{}] {}...",
                step_position + 1,
                total_steps,
                step.action.name()
            );
        }
        let result = execute_step(
            step,
            &mut client,
            &mut inspector,
            &mut session_id,
            &mut attached,
            &mut events_subscribed,
            &mut attach_runtime,
            &playbook.config.viewport.cols,
            &playbook.config.viewport.rows,
            &mut snapshots,
            deadline,
            &mut display_track,
            &mut runtime_vars,
            &mut visual_interactive,
            step_position,
            total_steps,
        )
        .await;

        #[allow(clippy::cast_possible_truncation)]
        let elapsed_ms = step_start.elapsed().as_millis() as u64;

        match result {
            Ok(detail) => {
                info!(
                    "step {}: {} — pass ({}ms)",
                    step.index,
                    step.action.name(),
                    elapsed_ms
                );
                let detail_for_visual = detail.clone();
                step_results.push(StepResult {
                    index: step.index,
                    action: step.action.name().to_string(),
                    status: StepStatus::Pass,
                    elapsed_ms,
                    detail,
                    expected: None,
                    actual: None,
                    failure_captures: None,
                    continue_on_error: step.continue_on_error,
                });
                if let Some(state) = visual_interactive.as_mut() {
                    state.mark_step_result(
                        step_position,
                        step.action.name(),
                        StepStatus::Pass,
                        u128::from(elapsed_ms),
                        detail_for_visual.as_deref(),
                    );
                    state
                        .maybe_refresh_and_render(
                            &mut client,
                            &mut inspector,
                            session_id,
                            attached,
                            true,
                        )
                        .await?;
                }
            }
            Err(err) => {
                if err.downcast_ref::<InteractiveAbort>().is_some() {
                    interactive_abort_from_step = Some(step_position);
                    let msg = format!(
                        "interactive run aborted during step {} ({})",
                        step.index,
                        step.action.name()
                    );
                    error_msg = Some(msg.clone());
                    if let Some(state) = visual_interactive.as_mut() {
                        state.mark_status(msg);
                        state
                            .maybe_refresh_and_render(
                                &mut client,
                                &mut inspector,
                                session_id,
                                attached,
                                true,
                            )
                            .await?;
                    }
                    break;
                }

                if playbook.config.verbose {
                    eprintln!(" FAIL ({elapsed_ms}ms)");
                }
                warn!(
                    "step {}: {} — fail: {err:#} ({}ms)",
                    step.index,
                    step.action.name(),
                    elapsed_ms
                );

                // Try to extract structured failure info if the error is a StepFailure.
                let (msg, expected, actual) = err.downcast_ref::<StepFailure>().map_or_else(
                    || (format!("{err:#}"), None, None),
                    |sf| (sf.message.clone(), sf.expected.clone(), sf.actual.clone()),
                );

                // Auto-capture all pane states at the time of failure.
                let failure_captures = if attached {
                    inspector.capture_all_safe()
                } else {
                    None
                };

                step_results.push(StepResult {
                    index: step.index,
                    action: step.action.name().to_string(),
                    status: StepStatus::Fail,
                    elapsed_ms,
                    detail: Some(msg.clone()),
                    expected,
                    actual,
                    failure_captures,
                    continue_on_error: step.continue_on_error,
                });
                if let Some(state) = visual_interactive.as_mut() {
                    state.mark_step_result(
                        step_position,
                        step.action.name(),
                        StepStatus::Fail,
                        u128::from(elapsed_ms),
                        Some(msg.as_str()),
                    );
                    state
                        .maybe_refresh_and_render(
                            &mut client,
                            &mut inspector,
                            session_id,
                            attached,
                            true,
                        )
                        .await?;
                }
                if step.continue_on_error {
                    // Record the failure but keep going.
                    warn!(
                        "step {} failed but continue_on_error is set, continuing",
                        step.index
                    );
                } else {
                    error_msg = Some(msg);
                    break; // Stop on first failure
                }
            }
        }
    }

    if let Some(abort_start_index) = interactive_abort_from_step {
        for skipped_step in playbook.steps.iter().skip(abort_start_index) {
            step_results.push(StepResult {
                index: skipped_step.index,
                action: skipped_step.action.name().to_string(),
                status: StepStatus::Skip,
                elapsed_ms: 0,
                detail: Some("skipped: aborted by interactive operator".to_string()),
                expected: None,
                actual: None,
                failure_captures: None,
                continue_on_error: skipped_step.continue_on_error,
            });
        }
    }

    // Finish display track before stopping the recording.
    if let Some(ref mut dt) = display_track
        && let Err(e) = dt.finish()
    {
        warn!("failed to finish display track: {e:#}");
    }

    // Copy recording dir to user recordings dir before sandbox shutdown.
    let mut recording_path: Option<std::path::PathBuf> = None;
    if let (Some(rid), Some(sb)) = (recording_id, &sandbox) {
        // Stop recording first so the server finalizes the binary files.
        match bmux_recording_plugin_api::typed_client::recording_stop(&mut client, Some(rid)).await
        {
            Ok(stopped_id) => {
                info!("recording stopped: {stopped_id}");
            }
            Err(e) => {
                warn!("failed to stop recording: {e}");
            }
        }

        // Copy recording dir from sandbox to user recordings dir.
        let src_dir = sb.paths().recordings_dir().join(rid.to_string());
        let user_recordings = bmux_config::ConfigPaths::default().recordings_dir();
        let dest_dir = user_recordings.join(rid.to_string());

        if src_dir.exists() {
            if let Err(e) = copy_dir_recursive(&src_dir, &dest_dir) {
                warn!("failed to copy recording to user dir: {e:#}");
            } else {
                info!("recording copied to {}", dest_dir.display());
                recording_path = Some(dest_dir);
            }
        }
    }

    #[allow(clippy::cast_possible_truncation)]
    let total_elapsed_ms = started.elapsed().as_millis() as u64;
    let pass = error_msg.is_none() && !step_results.iter().any(|s| s.status == StepStatus::Fail);

    // Capture sandbox root before shutdown (for inclusion in failed results).
    let sandbox_root = sandbox
        .as_ref()
        .map(|sb| sb.root_dir().to_string_lossy().to_string());

    if let Some(ref sb) = sandbox {
        forward_sandbox_phase_timing(sb);
    }

    // Shutdown sandbox if we created one.
    if let Some(sb) = sandbox
        && let Err(e) = sb.shutdown(!pass).await
    {
        warn!("sandbox shutdown error: {e:#}");
    }

    Ok(PlaybookResult {
        playbook_name,
        pass,
        steps: step_results,
        snapshots,
        recording_id,
        recording_path: recording_path.map(|p| p.to_string_lossy().to_string()),
        total_elapsed_ms,
        error: error_msg,
        sandbox_root: if pass { None } else { sandbox_root },
    })
}

fn forward_sandbox_phase_timing(sandbox: &SandboxServer) {
    if std::env::var_os("BMUX_PLAYBOOK_FORWARD_SANDBOX_PHASE_TIMING").is_none() {
        return;
    }
    let Ok(contents) = std::fs::read_to_string(sandbox.stderr_log_path()) else {
        return;
    };
    for line in contents.lines() {
        if SANDBOX_PHASE_MARKERS
            .iter()
            .any(|marker| line.contains(marker))
        {
            eprintln!("{line}");
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum InteractivePromptCommand {
    RunNextStep,
    ContinueRemaining,
    AbortRun,
    ShowScreen,
    RunDsl(String),
    Help,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InteractivePromptDecision {
    RunNextStep,
    ContinueRemaining,
    AbortRun,
}

fn parse_interactive_prompt_command(raw: &str) -> Result<InteractivePromptCommand> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || matches!(trimmed, "n" | "next") {
        return Ok(InteractivePromptCommand::RunNextStep);
    }
    if matches!(trimmed, "c" | "continue" | "l" | "live") {
        return Ok(InteractivePromptCommand::ContinueRemaining);
    }
    if matches!(trimmed, "q" | "quit" | "abort") {
        return Ok(InteractivePromptCommand::AbortRun);
    }
    if matches!(trimmed, "s" | "screen") {
        return Ok(InteractivePromptCommand::ShowScreen);
    }
    if matches!(trimmed, "h" | "help" | "?") {
        return Ok(InteractivePromptCommand::Help);
    }
    if let Some(rest) = trimmed.strip_prefix(':') {
        let dsl = rest.trim();
        if dsl.is_empty() {
            bail!("missing DSL command after ':'")
        }
        return Ok(InteractivePromptCommand::RunDsl(dsl.to_string()));
    }

    bail!("unknown interactive command '{trimmed}' (expected n/c/l/s/:<dsl>/q/help)",)
}

fn read_interactive_prompt_line() -> Result<Option<String>> {
    let mut line = String::new();
    let read = std::io::stdin()
        .read_line(&mut line)
        .context("failed reading interactive command from stdin")?;
    if read == 0 {
        return Ok(None);
    }
    Ok(Some(line))
}

fn print_interactive_prompt_help() {
    eprintln!("interactive commands:");
    eprintln!("  n / <enter>   run next playbook step");
    eprintln!("  c / l         continue remaining steps without pausing");
    eprintln!("  s             show current pane screen capture");
    eprintln!("  :<dsl>        run ad-hoc DSL command in this session");
    eprintln!("  q             abort run (remaining steps become skipped)");
}

#[allow(clippy::too_many_arguments)]
async fn run_visual_dsl_command(
    dsl: &str,
    step_index: usize,
    step_position: usize,
    total_steps: usize,
    client: &mut BmuxClient,
    inspector: &mut ScreenInspector,
    session_id: &mut Option<Uuid>,
    attached: &mut bool,
    events_subscribed: &mut bool,
    attach_runtime: &mut Option<AttachInputRuntime>,
    viewport_cols: &u16,
    viewport_rows: &u16,
    snapshots: &mut Vec<SnapshotCapture>,
    deadline: Instant,
    display_track: &mut Option<super::display_track::PlaybookDisplayTrackWriter>,
    runtime_vars: &mut RuntimeVars,
) -> Result<String> {
    let action = match parse_action_line(dsl) {
        Ok(action) => action,
        Err(err) => {
            return Ok(format!("DSL parse failed: {err:#}"));
        }
    };

    let action_name = action.name().to_string();
    let command_step = Step {
        index: step_index,
        action,
        continue_on_error: false,
    };
    let started = Instant::now();
    let mut no_visual = None;

    match execute_step(
        &command_step,
        client,
        inspector,
        session_id,
        attached,
        events_subscribed,
        attach_runtime,
        viewport_cols,
        viewport_rows,
        snapshots,
        deadline,
        display_track,
        runtime_vars,
        &mut no_visual,
        step_position,
        total_steps,
    )
    .await
    {
        Ok(detail) => {
            let elapsed_ms = started.elapsed().as_millis();
            detail.map_or_else(
                || {
                    Ok(format!(
                        "interactive command ok: {action_name} ({elapsed_ms}ms)"
                    ))
                },
                |detail| {
                    Ok(format!(
                        "interactive command ok: {action_name} ({elapsed_ms}ms) - {detail}"
                    ))
                },
            )
        }
        Err(err) => {
            if err.downcast_ref::<InteractiveAbort>().is_some() {
                Ok("interactive command aborted".to_string())
            } else {
                Ok(format!("interactive command failed: {err:#}"))
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn visual_wait_for_step_permission(
    visual_state: &mut VisualInteractiveState,
    step: &Step,
    step_position: usize,
    total_steps: usize,
    client: &mut BmuxClient,
    inspector: &mut ScreenInspector,
    session_id: &mut Option<Uuid>,
    attached: &mut bool,
    events_subscribed: &mut bool,
    attach_runtime: &mut Option<AttachInputRuntime>,
    viewport_cols: &u16,
    viewport_rows: &u16,
    snapshots: &mut Vec<SnapshotCapture>,
    deadline: Instant,
    display_track: &mut Option<super::display_track::PlaybookDisplayTrackWriter>,
    runtime_vars: &mut RuntimeVars,
) -> Result<InteractivePromptDecision> {
    visual_state.set_current_step(step_position, step);

    loop {
        visual_state
            .maybe_refresh_and_render(client, inspector, *session_id, *attached, false)
            .await?;

        while let Some(action) = visual_state.parse_next_control_action()? {
            let needs_dsl_prompt =
                visual_state.apply_control_action(action, VisualCheckpointPhase::BeforeStep);
            if needs_dsl_prompt {
                let Some(dsl) = visual_state.prompt_for_dsl_command()? else {
                    visual_state.mark_status("DSL prompt cancelled");
                    continue;
                };
                let status = run_visual_dsl_command(
                    &dsl,
                    step.index,
                    step_position,
                    total_steps,
                    client,
                    inspector,
                    session_id,
                    attached,
                    events_subscribed,
                    attach_runtime,
                    viewport_cols,
                    viewport_rows,
                    snapshots,
                    deadline,
                    display_track,
                    runtime_vars,
                )
                .await?;
                visual_state.mark_status(status);
            }
        }

        if visual_state.abort_requested {
            return Ok(InteractivePromptDecision::AbortRun);
        }

        if visual_state.step_once_requested {
            visual_state.step_once_requested = false;
            visual_state.single_step_inflight = true;
            visual_state.paused = false;
            visual_state.mark_status("running single-step");
            return Ok(InteractivePromptDecision::RunNextStep);
        }

        if !visual_state.paused {
            return Ok(InteractivePromptDecision::RunNextStep);
        }

        tokio::time::sleep(VISUAL_PAUSE_POLL_INTERVAL).await;
    }
}

async fn visual_checkpoint_during_step(
    visual_interactive: &mut Option<VisualInteractiveState>,
    client: &mut BmuxClient,
    inspector: &mut ScreenInspector,
    session_id: Option<Uuid>,
    attached: bool,
) -> Result<()> {
    let Some(visual_state) = visual_interactive.as_mut() else {
        return Ok(());
    };

    loop {
        visual_state
            .maybe_refresh_and_render(client, inspector, session_id, attached, false)
            .await?;

        while let Some(action) = visual_state.parse_next_control_action()? {
            let needs_dsl_prompt =
                visual_state.apply_control_action(action, VisualCheckpointPhase::InStep);
            if needs_dsl_prompt {
                visual_state.mark_status("pause at step boundary to run ':<dsl>' command");
            }
        }

        if visual_state.abort_requested {
            return Err(InteractiveAbort.into());
        }

        if !visual_state.paused {
            return Ok(());
        }

        tokio::time::sleep(VISUAL_PAUSE_POLL_INTERVAL).await;
    }
}

#[allow(clippy::too_many_arguments)]
async fn interactive_step_prompt(
    step: &Step,
    step_position: usize,
    total_steps: usize,
    client: &mut BmuxClient,
    inspector: &mut ScreenInspector,
    session_id: &mut Option<Uuid>,
    attached: &mut bool,
    events_subscribed: &mut bool,
    attach_runtime: &mut Option<AttachInputRuntime>,
    viewport_cols: &u16,
    viewport_rows: &u16,
    snapshots: &mut Vec<SnapshotCapture>,
    deadline: Instant,
    display_track: &mut Option<super::display_track::PlaybookDisplayTrackWriter>,
    runtime_vars: &mut RuntimeVars,
    visual_interactive: &mut Option<VisualInteractiveState>,
) -> Result<InteractivePromptDecision> {
    loop {
        {
            let mut stderr = std::io::stderr().lock();
            writeln!(
                stderr,
                "[step {}/{}] {}",
                step_position + 1,
                total_steps,
                step.to_dsl()
            )
            .context("failed writing interactive prompt")?;
            write!(stderr, "playbook> ").context("failed writing interactive prompt")?;
            stderr
                .flush()
                .context("failed flushing interactive prompt")?;
        }

        let Some(raw_line) = read_interactive_prompt_line()? else {
            eprintln!("interactive stdin closed; aborting run");
            return Ok(InteractivePromptDecision::AbortRun);
        };

        match parse_interactive_prompt_command(&raw_line) {
            Ok(InteractivePromptCommand::RunNextStep) => {
                return Ok(InteractivePromptDecision::RunNextStep);
            }
            Ok(InteractivePromptCommand::ContinueRemaining) => {
                return Ok(InteractivePromptDecision::ContinueRemaining);
            }
            Ok(InteractivePromptCommand::AbortRun) => {
                return Ok(InteractivePromptDecision::AbortRun);
            }
            Ok(InteractivePromptCommand::ShowScreen) => {
                print_interactive_screen_snapshot(client, inspector, *session_id, *attached)
                    .await?;
            }
            Ok(InteractivePromptCommand::RunDsl(dsl)) => {
                run_interactive_dsl_command(
                    &dsl,
                    step.index,
                    step_position,
                    total_steps,
                    client,
                    inspector,
                    session_id,
                    attached,
                    events_subscribed,
                    attach_runtime,
                    viewport_cols,
                    viewport_rows,
                    snapshots,
                    deadline,
                    display_track,
                    runtime_vars,
                    visual_interactive,
                )
                .await?;
            }
            Ok(InteractivePromptCommand::Help) => {
                print_interactive_prompt_help();
            }
            Err(err) => {
                eprintln!("interactive command error: {err}");
            }
        }
    }
}

async fn print_interactive_screen_snapshot(
    client: &mut BmuxClient,
    inspector: &mut ScreenInspector,
    session_id: Option<Uuid>,
    attached: bool,
) -> Result<()> {
    if !attached {
        eprintln!("screen unavailable: not attached to a session yet");
        return Ok(());
    }
    let Some(sid) = session_id else {
        eprintln!("screen unavailable: no session context");
        return Ok(());
    };

    if let Err(err) = inspector.refresh(client, sid).await {
        eprintln!("failed to refresh screen: {err:#}");
        return Ok(());
    }

    let Some(panes) = inspector.capture_all_safe() else {
        eprintln!("screen unavailable: capture failed");
        return Ok(());
    };

    if panes.is_empty() {
        eprintln!("screen unavailable: no panes captured");
        return Ok(());
    }

    for pane in panes {
        let focus_marker = if pane.focused { " (focused)" } else { "" };
        eprintln!("--- pane {}{} ---", pane.index, focus_marker);
        eprintln!("cursor: row={} col={}", pane.cursor_row, pane.cursor_col);
        eprintln!("{}", pane.screen_text);
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn run_interactive_dsl_command(
    dsl: &str,
    step_index: usize,
    step_position: usize,
    total_steps: usize,
    client: &mut BmuxClient,
    inspector: &mut ScreenInspector,
    session_id: &mut Option<Uuid>,
    attached: &mut bool,
    events_subscribed: &mut bool,
    attach_runtime: &mut Option<AttachInputRuntime>,
    viewport_cols: &u16,
    viewport_rows: &u16,
    snapshots: &mut Vec<SnapshotCapture>,
    deadline: Instant,
    display_track: &mut Option<super::display_track::PlaybookDisplayTrackWriter>,
    runtime_vars: &mut RuntimeVars,
    visual_interactive: &mut Option<VisualInteractiveState>,
) -> Result<()> {
    let action = match parse_action_line(dsl) {
        Ok(action) => action,
        Err(err) => {
            eprintln!("interactive DSL parse failed: {err:#}");
            return Ok(());
        }
    };

    let action_name = action.name().to_string();
    let command_step = Step {
        index: step_index,
        action,
        continue_on_error: false,
    };
    let started = Instant::now();

    match execute_step(
        &command_step,
        client,
        inspector,
        session_id,
        attached,
        events_subscribed,
        attach_runtime,
        viewport_cols,
        viewport_rows,
        snapshots,
        deadline,
        display_track,
        runtime_vars,
        visual_interactive,
        step_position,
        total_steps,
    )
    .await
    {
        Ok(detail) => {
            let elapsed_ms = started.elapsed().as_millis();
            if let Some(detail) = detail {
                eprintln!("interactive command ok: {action_name} ({elapsed_ms}ms) - {detail}");
            } else {
                eprintln!("interactive command ok: {action_name} ({elapsed_ms}ms)");
            }
        }
        Err(err) => {
            eprintln!("interactive command failed: {err:#}");
        }
    }

    Ok(())
}

/// Start a recording on the server, optionally filtered to a specific session.
pub(super) async fn start_recording(
    client: &mut BmuxClient,
    session_id: Option<Uuid>,
) -> Result<Uuid> {
    let summary = bmux_recording_plugin_api::typed_client::recording_start(
        client, session_id, true, // capture_input
        None, // name
        None, // profile: server default (Functional)
        None, // event_kinds: server default
    )
    .await
    .map_err(|e| anyhow::anyhow!("recording start failed: {e}"))?;
    Ok(summary.id)
}

/// Execute a single step.
#[allow(clippy::too_many_arguments)]
#[allow(clippy::too_many_lines, clippy::cast_possible_truncation)]
pub(super) async fn execute_step(
    step: &Step,
    client: &mut BmuxClient,
    inspector: &mut ScreenInspector,
    session_id: &mut Option<Uuid>,
    attached: &mut bool,
    events_subscribed: &mut bool,
    attach_runtime: &mut Option<AttachInputRuntime>,
    viewport_cols: &u16,
    viewport_rows: &u16,
    snapshots: &mut Vec<SnapshotCapture>,
    deadline: Instant,
    display_track: &mut Option<super::display_track::PlaybookDisplayTrackWriter>,
    runtime_vars: &mut RuntimeVars,
    visual_interactive: &mut Option<VisualInteractiveState>,
    _step_position: usize,
    _total_steps: usize,
) -> Result<Option<String>> {
    visual_checkpoint_during_step(
        visual_interactive,
        client,
        inspector,
        *session_id,
        *attached,
    )
    .await?;

    match &step.action {
        Action::NewSession { name } => {
            let resolved_name = name.as_ref().map(|n| runtime_vars.resolve_opt(n));
            let sid = typed_new_session_playbook(client, resolved_name.clone()).await?;
            debug!("created session {sid}");

            // Update runtime vars
            runtime_vars.session_id = Some(sid);
            runtime_vars.session_name = resolved_name;
            runtime_vars.pane_count = 1;
            runtime_vars.focused_pane = 1;

            // Attach to the session
            let grant = client
                .attach_grant(SessionSelector::ById(sid))
                .await
                .map_err(|e| anyhow::anyhow!("attach grant failed: {e}"))?;
            let attach_info = client
                .open_attach_stream_info(&grant)
                .await
                .map_err(|e| anyhow::anyhow!("attach open failed: {e}"))?;
            client
                .attach_set_viewport(sid, *viewport_cols, *viewport_rows)
                .await
                .map_err(|e| anyhow::anyhow!("set viewport failed: {e}"))?;

            *session_id = Some(sid);
            *attached = true;
            *attach_runtime = Some(AttachInputRuntime::new(attach_info));
            let current_context_id = current_context_playbook(client)
                .await
                .ok()
                .flatten()
                .map(|context| context.id);
            if let Some(runtime) = attach_runtime.as_mut() {
                runtime.state.attached_id = sid;
                runtime.state.attached_context_id = current_context_id.or(grant.context_id);
                if let Some(context_id) = runtime.state.attached_context_id
                    && !runtime.state.window_context_ids.contains(&context_id)
                {
                    runtime.state.window_context_ids.push(context_id);
                }
                runtime.state.scrollback_active = false;
                runtime.state.scrollback_offset = 0;
                runtime.processor.set_scroll_mode(false);
            }

            // Drain initial output to let the shell start up
            drain_output_until_idle(
                client,
                inspector,
                sid,
                Duration::from_millis(500),
                display_track,
                visual_interactive,
                *attached,
            )
            .await?;

            Ok(Some(format!("session_id={sid}")))
        }

        Action::KillSession { name } => {
            let selector = SessionSelector::ByName(name.clone());
            let killed_id = typed_kill_session_playbook(client, selector).await?;
            // Only clear state if we killed the session we were attached to.
            if *session_id == Some(killed_id) {
                *session_id = None;
                *attached = false;
                *attach_runtime = None;
            }
            Ok(None)
        }

        Action::SplitPane {
            direction,
            ratio: _,
        } => {
            let sid = require_session(*session_id)?;
            require_attached(*attached)?;
            let ipc_dir = match direction {
                SplitDirection::Vertical => PaneSplitDirection::Vertical,
                SplitDirection::Horizontal => PaneSplitDirection::Horizontal,
            };
            let ack: bmux_windows_plugin_api::windows_commands::PaneAck =
                invoke_windows_command_bmux(
                    client,
                    "split-pane",
                    &crate::runtime::typed_windows::args::SplitPane {
                        session: Some(crate::runtime::typed_windows::ipc_to_typed_selector(
                            SessionSelector::ById(sid),
                        )),
                        target: None,
                        direction: crate::runtime::typed_windows::ipc_split_to_typed_direction(
                            ipc_dir,
                        ),
                        ratio_pct: None,
                    },
                )
                .await
                .map_err(|e| anyhow::anyhow!("split-pane failed: {e}"))?;
            let pane_id = ack
                .pane_id
                .ok_or_else(|| anyhow::anyhow!("split-pane returned no pane id"))?;

            // Let the new pane shell start
            drain_output_until_idle(
                client,
                inspector,
                sid,
                Duration::from_millis(300),
                display_track,
                visual_interactive,
                *attached,
            )
            .await?;

            runtime_vars.pane_count += 1;

            Ok(Some(format!("pane_id={pane_id}")))
        }

        Action::FocusPane { target } => {
            let sid = require_session(*session_id)?;
            require_attached(*attached)?;
            let selector = crate::runtime::typed_windows::ipc_pane_to_typed_selector(
                &bmux_ipc::PaneSelector::ByIndex(*target),
            );
            let _ack: bmux_windows_plugin_api::windows_commands::PaneAck =
                invoke_windows_command_bmux(
                    client,
                    "focus-pane-by-selector",
                    &crate::runtime::typed_windows::args::FocusPaneBySelector {
                        session: Some(crate::runtime::typed_windows::ipc_to_typed_selector(
                            SessionSelector::ById(sid),
                        )),
                        target: selector,
                    },
                )
                .await
                .map_err(|e| anyhow::anyhow!("focus-pane failed: {e}"))?;
            runtime_vars.focused_pane = *target;
            Ok(None)
        }

        Action::ClosePane { target } => {
            let sid = require_session(*session_id)?;
            require_attached(*attached)?;
            let selector = target.as_ref().map_or_else(
                || {
                    crate::runtime::typed_windows::ipc_pane_to_typed_selector(
                        &bmux_ipc::PaneSelector::Active,
                    )
                },
                |idx| {
                    crate::runtime::typed_windows::ipc_pane_to_typed_selector(
                        &bmux_ipc::PaneSelector::ByIndex(*idx),
                    )
                },
            );
            let _ack: bmux_windows_plugin_api::windows_commands::PaneAck =
                invoke_windows_command_bmux(
                    client,
                    "close-pane-by-selector",
                    &crate::runtime::typed_windows::args::ClosePaneBySelector {
                        session: Some(crate::runtime::typed_windows::ipc_to_typed_selector(
                            SessionSelector::ById(sid),
                        )),
                        target: selector,
                    },
                )
                .await
                .map_err(|e| anyhow::anyhow!("close-pane failed: {e}"))?;
            runtime_vars.pane_count = runtime_vars.pane_count.saturating_sub(1);
            Ok(None)
        }

        Action::SendKeys { keys, pane } => {
            let sid = require_session(*session_id)?;
            require_attached(*attached)?;
            if pane.is_none()
                && attach_runtime
                    .as_ref()
                    .is_some_and(|runtime| runtime.state.scrollback_active)
            {
                bail!(
                    "send-keys targets pane input while attach scrollback is active; use send-attach key='<chord>' for UI-mode key handling"
                );
            }
            let resolved_keys = runtime_vars.resolve_bytes(keys);

            if let Some(target_index) = pane {
                // Pane-targeted send: resolve the pane index to a UUID and use
                // PaneDirectInput to write bytes directly without focus changes.
                let layout = client
                    .attach_layout(sid)
                    .await
                    .map_err(|e| anyhow::anyhow!("layout for pane lookup failed: {e}"))?;
                let pane_id = layout
                    .panes
                    .iter()
                    .find(|p| p.index == *target_index)
                    .map(|p| p.id)
                    .ok_or_else(|| anyhow::anyhow!("pane index {target_index} not found"))?;

                client
                    .pane_direct_input(sid, pane_id, resolved_keys.clone())
                    .await
                    .map_err(|e| anyhow::anyhow!("send-keys to pane {target_index} failed: {e}"))?;
            } else {
                client
                    .attach_input(sid, resolved_keys)
                    .await
                    .map_err(|e| anyhow::anyhow!("send-keys failed: {e}"))?;
            }
            if let Some(dt) = display_track.as_mut() {
                let _ = dt.record_activity(bmux_ipc::DisplayActivityKind::Input);
            }
            Ok(None)
        }

        Action::SendBytes { hex } => {
            let sid = require_session(*session_id)?;
            require_attached(*attached)?;
            client
                .attach_input(sid, hex.clone())
                .await
                .map_err(|e| anyhow::anyhow!("send-bytes failed: {e}"))?;
            if let Some(dt) = display_track.as_mut() {
                let _ = dt.record_activity(bmux_ipc::DisplayActivityKind::Input);
            }
            Ok(None)
        }

        Action::Sleep { duration } => {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let sleep_dur = (*duration).min(remaining);
            let sleep_start = Instant::now();
            while sleep_start.elapsed() < sleep_dur {
                visual_checkpoint_during_step(
                    visual_interactive,
                    client,
                    inspector,
                    *session_id,
                    *attached,
                )
                .await?;
                let remaining_chunk = sleep_dur.saturating_sub(sleep_start.elapsed());
                let chunk = remaining_chunk.min(Duration::from_millis(50));
                if chunk.is_zero() {
                    break;
                }
                tokio::time::sleep(chunk).await;
            }
            Ok(None)
        }

        Action::WaitFor {
            pattern,
            pane,
            timeout,
            retry,
        } => {
            let sid = require_session(*session_id)?;
            require_attached(*attached)?;

            // Resolve variables in the pattern before compiling the regex.
            let resolved_pattern = runtime_vars.resolve_opt(pattern);

            // Compile regex once, not on every poll iteration.
            let re = regex::Regex::new(&resolved_pattern)
                .with_context(|| format!("invalid regex: {resolved_pattern}"))?;

            let max_attempts = (*retry).max(1);
            let mut last_err = None;

            for attempt in 0..max_attempts {
                if attempt > 0 {
                    // Brief drain between retry attempts.
                    drain_output_until_idle(
                        client,
                        inspector,
                        sid,
                        Duration::from_millis(200),
                        display_track,
                        visual_interactive,
                        *attached,
                    )
                    .await?;
                }

                let wait_deadline = Instant::now() + (*timeout).min(deadline - Instant::now());
                let mut poll_delay = Duration::from_millis(10);

                let result = loop {
                    visual_checkpoint_during_step(
                        visual_interactive,
                        client,
                        inspector,
                        *session_id,
                        *attached,
                    )
                    .await?;

                    // Drain any pending output (lower threshold for WaitFor's retry loop)
                    drain_output_with_threshold(
                        client,
                        inspector,
                        sid,
                        Duration::from_millis(100),
                        display_track,
                        visual_interactive,
                        *attached,
                        3,
                    )
                    .await?;

                    // Refresh screen state
                    let snapshot = inspector.refresh(client, sid).await?;
                    let pane_idx = inspector.resolve_pane_index(*pane, &snapshot)?;

                    if inspector.pane_matches_compiled(pane_idx, &re) {
                        break Ok(Some(format!("matched pattern '{resolved_pattern}'")));
                    }

                    if Instant::now() >= wait_deadline {
                        let screen_text = inspector
                            .pane_text(pane_idx)
                            .unwrap_or_else(|| "<no text>".to_string());
                        break Err(StepFailure::assertion(
                            format!(
                                "wait-for timed out after {}ms on pane {} waiting for pattern '{resolved_pattern}' (attempt {}/{})",
                                timeout.as_millis(),
                                pane_idx,
                                attempt + 1,
                                max_attempts,
                            ),
                            resolved_pattern.clone(),
                            screen_text,
                        ));
                    }

                    visual_checkpoint_during_step(
                        visual_interactive,
                        client,
                        inspector,
                        *session_id,
                        *attached,
                    )
                    .await?;
                    tokio::time::sleep(poll_delay).await;
                    poll_delay = (poll_delay * 2).min(Duration::from_millis(200));
                };

                match result {
                    Ok(detail) => return Ok(detail),
                    Err(err) => {
                        if attempt + 1 < max_attempts {
                            info!(
                                "wait-for attempt {}/{} failed, retrying",
                                attempt + 1,
                                max_attempts
                            );
                        }
                        last_err = Some(err);
                    }
                }
            }

            // All attempts exhausted.
            Err(last_err.unwrap().into())
        }

        Action::Snapshot { id } => {
            let sid = require_session(*session_id)?;
            require_attached(*attached)?;

            drain_output_until_idle(
                client,
                inspector,
                sid,
                Duration::from_millis(200),
                display_track,
                visual_interactive,
                *attached,
            )
            .await?;
            let _snapshot = inspector.refresh(client, sid).await?;
            let panes = inspector.capture_all();

            snapshots.push(SnapshotCapture {
                id: id.clone(),
                panes,
            });

            Ok(Some(format!("snapshot '{id}' captured")))
        }

        Action::AssertScreen {
            pane,
            contains,
            not_contains,
            matches,
        } => {
            let sid = require_session(*session_id)?;
            require_attached(*attached)?;

            drain_output_until_idle(
                client,
                inspector,
                sid,
                Duration::from_millis(200),
                display_track,
                visual_interactive,
                *attached,
            )
            .await?;
            let snapshot = inspector.refresh(client, sid).await?;
            let pane_idx = inspector.resolve_pane_index(*pane, &snapshot)?;

            if let Some(needle) = contains {
                let resolved = runtime_vars.resolve_opt(needle);
                if !inspector.pane_contains(pane_idx, &resolved) {
                    let text = inspector
                        .pane_text(pane_idx)
                        .unwrap_or_else(|| "<no text>".to_string());
                    return Err(StepFailure::assertion(
                        format!("assert-screen: pane {pane_idx} does not contain '{resolved}'"),
                        resolved,
                        text,
                    )
                    .into());
                }
            }

            if let Some(needle) = not_contains {
                let resolved = runtime_vars.resolve_opt(needle);
                if inspector.pane_contains(pane_idx, &resolved) {
                    let text = inspector
                        .pane_text(pane_idx)
                        .unwrap_or_else(|| "<no text>".to_string());
                    return Err(StepFailure::assertion(
                        format!(
                            "assert-screen: pane {pane_idx} unexpectedly contains '{resolved}'"
                        ),
                        format!("not '{resolved}'"),
                        text,
                    )
                    .into());
                }
            }

            if let Some(pattern) = matches {
                let resolved = runtime_vars.resolve_opt(pattern);
                if !inspector.pane_matches(pane_idx, &resolved)? {
                    let text = inspector
                        .pane_text(pane_idx)
                        .unwrap_or_else(|| "<no text>".to_string());
                    return Err(StepFailure::assertion(
                        format!("assert-screen: pane {pane_idx} does not match '{resolved}'"),
                        resolved,
                        text,
                    )
                    .into());
                }
            }

            Ok(None)
        }

        Action::AssertLayout { pane_count } => {
            let sid = require_session(*session_id)?;
            require_attached(*attached)?;

            let snapshot = inspector.refresh(client, sid).await?;
            #[allow(clippy::cast_possible_truncation)]
            let actual_count = snapshot.panes.len() as u32;

            if actual_count != *pane_count {
                return Err(StepFailure::assertion(
                    format!("assert-layout: expected {pane_count} panes, got {actual_count}"),
                    pane_count.to_string(),
                    actual_count.to_string(),
                )
                .into());
            }

            Ok(None)
        }

        Action::AssertCursor { pane, row, col } => {
            let sid = require_session(*session_id)?;
            require_attached(*attached)?;

            let snapshot = inspector.refresh(client, sid).await?;
            let pane_idx = inspector.resolve_pane_index(*pane, &snapshot)?;

            let (actual_row, actual_col) = inspector
                .pane_cursor(pane_idx)
                .context("pane cursor not available")?;

            if actual_row != *row || actual_col != *col {
                return Err(StepFailure::assertion(
                    format!(
                        "assert-cursor: expected ({row},{col}), got ({actual_row},{actual_col})"
                    ),
                    format!("({row},{col})"),
                    format!("({actual_row},{actual_col})"),
                )
                .into());
            }

            Ok(None)
        }

        Action::ResizeViewport { cols, rows } => {
            let sid = require_session(*session_id)?;
            if *attached {
                client
                    .attach_set_viewport(sid, *cols, *rows)
                    .await
                    .map_err(|e| anyhow::anyhow!("resize-viewport failed: {e}"))?;
            }
            inspector.update_viewport(*cols, *rows);
            if let Some(ref mut dt) = *display_track {
                let _ = dt.record_resize(*cols, *rows);
            }
            Ok(None)
        }

        Action::SendAttach { key } => {
            execute_attach_chord(
                key,
                client,
                inspector,
                session_id,
                attached,
                attach_runtime,
                runtime_vars,
            )
            .await
            .map_err(|e| anyhow::anyhow!("send-attach failed: {e}"))?;
            let detail = attach_runtime.as_ref().map(|runtime| {
                format!(
                    "scrollback_active={} scrollback_offset={}",
                    runtime.state.scrollback_active, runtime.state.scrollback_offset
                )
            });
            Ok(detail)
        }

        Action::PrefixKey { key } => {
            let key = format!("ctrl+a {key}");
            execute_attach_chord(
                &key,
                client,
                inspector,
                session_id,
                attached,
                attach_runtime,
                runtime_vars,
            )
            .await
            .map_err(|e| anyhow::anyhow!("prefix-key failed: {e}"))?;
            let detail = attach_runtime.as_ref().map(|runtime| {
                format!(
                    "scrollback_active={} scrollback_offset={}",
                    runtime.state.scrollback_active, runtime.state.scrollback_offset
                )
            });
            Ok(detail)
        }

        Action::WaitForEvent { event, timeout } => {
            let _sid = require_session(*session_id)?;

            // Subscribe to events on first use.
            if !*events_subscribed {
                client
                    .subscribe_events()
                    .await
                    .map_err(|e| anyhow::anyhow!("event subscription failed: {e}"))?;
                *events_subscribed = true;
            }

            let resolved_event = runtime_vars.resolve_opt(event);
            let wait_deadline = Instant::now() + (*timeout).min(deadline - Instant::now());
            let mut poll_delay = Duration::from_millis(25);

            loop {
                visual_checkpoint_during_step(
                    visual_interactive,
                    client,
                    inspector,
                    *session_id,
                    *attached,
                )
                .await?;

                let events = client
                    .poll_events(32)
                    .await
                    .map_err(|e| anyhow::anyhow!("poll events failed: {e}"))?;

                for evt in &events {
                    if event_matches(evt, &resolved_event) {
                        return Ok(Some(format!("matched event '{resolved_event}'")));
                    }
                }

                if Instant::now() >= wait_deadline {
                    return Err(StepFailure::msg(format!(
                        "wait-for-event timed out after {}ms waiting for '{resolved_event}'",
                        timeout.as_millis()
                    ))
                    .into());
                }

                visual_checkpoint_during_step(
                    visual_interactive,
                    client,
                    inspector,
                    *session_id,
                    *attached,
                )
                .await?;
                tokio::time::sleep(poll_delay).await;
                poll_delay = (poll_delay * 2).min(Duration::from_millis(250));
            }
        }

        Action::InvokeService {
            capability,
            kind,
            interface_id,
            operation,
            payload,
        } => {
            let resolved_payload = runtime_vars.resolve_opt(payload);
            let ipc_kind = match kind {
                ServiceKind::Query => InvokeServiceKind::Query,
                ServiceKind::Command => InvokeServiceKind::Command,
            };
            let response_bytes = client
                .invoke_service_raw(
                    capability.clone(),
                    ipc_kind,
                    interface_id.clone(),
                    operation.clone(),
                    resolved_payload.into_bytes(),
                )
                .await
                .map_err(|e| anyhow::anyhow!("invoke-service failed: {e}"))?;

            let detail = if response_bytes.is_empty() {
                None
            } else {
                Some(
                    String::from_utf8(response_bytes)
                        .unwrap_or_else(|e| format!("<{} bytes binary>", e.into_bytes().len())),
                )
            };
            Ok(detail)
        }

        Action::Screen => {
            let sid = require_session(*session_id)?;
            require_attached(*attached)?;
            drain_output_until_idle(
                client,
                inspector,
                sid,
                Duration::from_millis(200),
                display_track,
                visual_interactive,
                *attached,
            )
            .await?;
            let snapshot = inspector.refresh(client, sid).await?;
            let _ = snapshot; // satisfy the borrow checker
            let captures = inspector.capture_all();
            // Serialize the pane captures as JSON for inclusion in step detail.
            let json = serde_json::to_string(&captures).unwrap_or_else(|_| "[]".to_string());
            Ok(Some(json))
        }

        Action::Status => {
            let sid_detail = session_id.map_or("none".to_string(), |id| id.to_string());
            let detail = format!(
                "session_id={}, pane_count={}, focused_pane={}",
                sid_detail, runtime_vars.pane_count, runtime_vars.focused_pane,
            );
            Ok(Some(detail))
        }
    }
}

#[allow(clippy::too_many_arguments)]
#[allow(clippy::cast_possible_truncation)]
async fn execute_attach_chord(
    chord: &str,
    client: &mut BmuxClient,
    inspector: &mut ScreenInspector,
    session_id: &mut Option<Uuid>,
    attached: &mut bool,
    attach_runtime: &mut Option<AttachInputRuntime>,
    runtime_vars: &mut RuntimeVars,
) -> Result<()> {
    let sid = require_session(*session_id)?;
    require_attached(*attached)?;
    let runtime = attach_runtime
        .as_mut()
        .context("attach input runtime not initialized; create a session first")?;

    runtime.state.attached_id = sid;
    runtime
        .processor
        .set_scroll_mode(runtime.state.scrollback_active);

    let strokes = crate::input::parse_key_chord(chord)
        .map_err(|e| anyhow::anyhow!("invalid attach key chord '{chord}': {e}"))?;
    for stroke in &strokes {
        let event = crossterm_event_from_stroke(*stroke);
        let actions = runtime.processor.process_terminal_event(event);
        apply_attach_runtime_actions(actions, client, sid, inspector, runtime).await?;
    }

    let trailing_actions = runtime.processor.process_stream_bytes(&[]);
    apply_attach_runtime_actions(trailing_actions, client, sid, inspector, runtime).await?;

    *session_id = Some(runtime.state.attached_id);
    *attached = true;

    let snapshot = inspector.refresh(client, runtime.state.attached_id).await?;
    runtime_vars.pane_count = snapshot.panes.len() as u32;
    if let Some(focused) = snapshot.panes.iter().find(|pane| pane.focused) {
        runtime_vars.focused_pane = focused.index;
    }

    Ok(())
}

#[allow(clippy::too_many_lines)]
async fn apply_attach_runtime_actions(
    actions: Vec<crate::input::RuntimeAction>,
    client: &mut BmuxClient,
    sid: Uuid,
    inspector: &ScreenInspector,
    runtime: &mut AttachInputRuntime,
) -> Result<()> {
    for runtime_action in actions {
        match runtime_action {
            crate::input::RuntimeAction::ForwardToPane(bytes) => {
                client
                    .attach_input(sid, bytes)
                    .await
                    .map_err(|e| anyhow::anyhow!("attach input failed: {e}"))?;
            }
            crate::input::RuntimeAction::Detach => {
                bail!("attach input requested detach; unsupported inside playbook step")
            }
            crate::input::RuntimeAction::SplitFocusedVertical => {
                invoke_windows_command_bmux::<_, bmux_windows_plugin_api::windows_commands::PaneAck>(
                    client,
                    "split-pane",
                    &crate::runtime::typed_windows::args::SplitPane {
                        session: Some(crate::runtime::typed_windows::ipc_to_typed_selector(
                            SessionSelector::ById(runtime.state.attached_id),
                        )),
                        target: None,
                        direction: crate::runtime::typed_windows::ipc_split_to_typed_direction(
                            PaneSplitDirection::Vertical,
                        ),
                        ratio_pct: None,
                    },
                )
                .await
                .map_err(|e| anyhow::anyhow!("split focused vertical failed: {e}"))?;
            }
            crate::input::RuntimeAction::SplitFocusedHorizontal => {
                invoke_windows_command_bmux::<_, bmux_windows_plugin_api::windows_commands::PaneAck>(
                    client,
                    "split-pane",
                    &crate::runtime::typed_windows::args::SplitPane {
                        session: Some(crate::runtime::typed_windows::ipc_to_typed_selector(
                            SessionSelector::ById(runtime.state.attached_id),
                        )),
                        target: None,
                        direction: crate::runtime::typed_windows::ipc_split_to_typed_direction(
                            PaneSplitDirection::Horizontal,
                        ),
                        ratio_pct: None,
                    },
                )
                .await
                .map_err(|e| anyhow::anyhow!("split focused horizontal failed: {e}"))?;
            }
            crate::input::RuntimeAction::FocusNext => {
                invoke_windows_command_bmux::<_, bmux_windows_plugin_api::windows_commands::PaneAck>(
                    client,
                    "focus-pane-in-direction",
                    &crate::runtime::typed_windows::args::FocusPaneInDirection {
                        session: Some(crate::runtime::typed_windows::ipc_to_typed_selector(
                            SessionSelector::ById(runtime.state.attached_id),
                        )),
                        direction: crate::runtime::typed_windows::ipc_focus_to_typed_direction(
                            PaneFocusDirection::Next,
                        ),
                    },
                )
                .await
                .map_err(|e| anyhow::anyhow!("focus next failed: {e}"))?;
            }
            crate::input::RuntimeAction::FocusPrev => {
                invoke_windows_command_bmux::<_, bmux_windows_plugin_api::windows_commands::PaneAck>(
                    client,
                    "focus-pane-in-direction",
                    &crate::runtime::typed_windows::args::FocusPaneInDirection {
                        session: Some(crate::runtime::typed_windows::ipc_to_typed_selector(
                            SessionSelector::ById(runtime.state.attached_id),
                        )),
                        direction: crate::runtime::typed_windows::ipc_focus_to_typed_direction(
                            PaneFocusDirection::Prev,
                        ),
                    },
                )
                .await
                .map_err(|e| anyhow::anyhow!("focus prev failed: {e}"))?;
            }
            crate::input::RuntimeAction::FocusLeft => {
                invoke_windows_command_bmux::<_, bmux_windows_plugin_api::windows_commands::PaneAck>(
                    client,
                    "focus-pane-in-direction",
                    &crate::runtime::typed_windows::args::FocusPaneInDirection {
                        session: Some(crate::runtime::typed_windows::ipc_to_typed_selector(
                            SessionSelector::ById(runtime.state.attached_id),
                        )),
                        direction: crate::runtime::typed_windows::ipc_focus_to_typed_direction(
                            PaneFocusDirection::Prev,
                        ),
                    },
                )
                .await
                .map_err(|e| anyhow::anyhow!("focus left failed: {e}"))?;
            }
            crate::input::RuntimeAction::FocusRight => {
                invoke_windows_command_bmux::<_, bmux_windows_plugin_api::windows_commands::PaneAck>(
                    client,
                    "focus-pane-in-direction",
                    &crate::runtime::typed_windows::args::FocusPaneInDirection {
                        session: Some(crate::runtime::typed_windows::ipc_to_typed_selector(
                            SessionSelector::ById(runtime.state.attached_id),
                        )),
                        direction: crate::runtime::typed_windows::ipc_focus_to_typed_direction(
                            PaneFocusDirection::Next,
                        ),
                    },
                )
                .await
                .map_err(|e| anyhow::anyhow!("focus right failed: {e}"))?;
            }
            crate::input::RuntimeAction::FocusUp => {
                invoke_windows_command_bmux::<_, bmux_windows_plugin_api::windows_commands::PaneAck>(
                    client,
                    "focus-pane-in-direction",
                    &crate::runtime::typed_windows::args::FocusPaneInDirection {
                        session: Some(crate::runtime::typed_windows::ipc_to_typed_selector(
                            SessionSelector::ById(runtime.state.attached_id),
                        )),
                        direction: crate::runtime::typed_windows::ipc_focus_to_typed_direction(
                            PaneFocusDirection::Prev,
                        ),
                    },
                )
                .await
                .map_err(|e| anyhow::anyhow!("focus up failed: {e}"))?;
            }
            crate::input::RuntimeAction::FocusDown => {
                invoke_windows_command_bmux::<_, bmux_windows_plugin_api::windows_commands::PaneAck>(
                    client,
                    "focus-pane-in-direction",
                    &crate::runtime::typed_windows::args::FocusPaneInDirection {
                        session: Some(crate::runtime::typed_windows::ipc_to_typed_selector(
                            SessionSelector::ById(runtime.state.attached_id),
                        )),
                        direction: crate::runtime::typed_windows::ipc_focus_to_typed_direction(
                            PaneFocusDirection::Next,
                        ),
                    },
                )
                .await
                .map_err(|e| anyhow::anyhow!("focus down failed: {e}"))?;
            }
            crate::input::RuntimeAction::CloseFocusedPane => {
                invoke_windows_command_bmux::<_, bmux_windows_plugin_api::windows_commands::PaneAck>(
                    client,
                    "close-active-pane",
                    &crate::runtime::typed_windows::args::CloseActivePane {
                        session: Some(crate::runtime::typed_windows::ipc_to_typed_selector(
                            SessionSelector::ById(runtime.state.attached_id),
                        )),
                    },
                )
                .await
                .map_err(|e| anyhow::anyhow!("close focused pane failed: {e}"))?;
            }
            crate::input::RuntimeAction::ZoomPane => {
                invoke_windows_command_bmux::<
                    _,
                    bmux_windows_plugin_api::windows_commands::PaneZoomAck,
                >(
                    client,
                    "zoom-pane",
                    &crate::runtime::typed_windows::args::ZoomPane {
                        session: Some(crate::runtime::typed_windows::ipc_to_typed_selector(
                            SessionSelector::ById(runtime.state.attached_id),
                        )),
                    },
                )
                .await
                .map_err(|e| anyhow::anyhow!("zoom pane failed: {e}"))?;
            }
            crate::input::RuntimeAction::IncreaseSplit
            | crate::input::RuntimeAction::ResizeRight
            | crate::input::RuntimeAction::ResizeDown => {
                invoke_windows_command_bmux::<_, bmux_windows_plugin_api::windows_commands::PaneAck>(
                    client,
                    "resize-pane",
                    &crate::runtime::typed_windows::args::ResizePane {
                        session: Some(crate::runtime::typed_windows::ipc_to_typed_selector(
                            SessionSelector::ById(runtime.state.attached_id),
                        )),
                        target: None,
                        delta: 1,
                    },
                )
                .await
                .map_err(|e| anyhow::anyhow!("resize increase failed: {e}"))?;
            }
            crate::input::RuntimeAction::DecreaseSplit
            | crate::input::RuntimeAction::ResizeLeft
            | crate::input::RuntimeAction::ResizeUp => {
                invoke_windows_command_bmux::<_, bmux_windows_plugin_api::windows_commands::PaneAck>(
                    client,
                    "resize-pane",
                    &crate::runtime::typed_windows::args::ResizePane {
                        session: Some(crate::runtime::typed_windows::ipc_to_typed_selector(
                            SessionSelector::ById(runtime.state.attached_id),
                        )),
                        target: None,
                        delta: -1,
                    },
                )
                .await
                .map_err(|e| anyhow::anyhow!("resize decrease failed: {e}"))?;
            }
            crate::input::RuntimeAction::EnterScrollMode => {
                runtime.state.scrollback_active = true;
            }
            crate::input::RuntimeAction::ExitScrollMode
            | crate::input::RuntimeAction::ConfirmScrollback => {
                runtime.state.scrollback_active = false;
                runtime.state.scrollback_offset = 0;
            }
            crate::input::RuntimeAction::ScrollUpLine => {
                if runtime.state.scrollback_active {
                    runtime.state.scrollback_offset =
                        runtime.state.scrollback_offset.saturating_add(1);
                }
            }
            crate::input::RuntimeAction::ScrollDownLine => {
                if runtime.state.scrollback_active {
                    runtime.state.scrollback_offset =
                        runtime.state.scrollback_offset.saturating_sub(1);
                }
            }
            crate::input::RuntimeAction::ScrollUpPage => {
                if runtime.state.scrollback_active {
                    runtime.state.scrollback_offset =
                        runtime.state.scrollback_offset.saturating_add(20);
                }
            }
            crate::input::RuntimeAction::ScrollDownPage => {
                if runtime.state.scrollback_active {
                    runtime.state.scrollback_offset =
                        runtime.state.scrollback_offset.saturating_sub(20);
                }
            }
            crate::input::RuntimeAction::ScrollTop => {
                if runtime.state.scrollback_active {
                    runtime.state.scrollback_offset = usize::MAX / 2;
                }
            }
            crate::input::RuntimeAction::ScrollBottom => {
                if runtime.state.scrollback_active {
                    runtime.state.scrollback_offset = 0;
                }
            }
            crate::input::RuntimeAction::PluginCommand {
                plugin_id,
                command_name,
                args,
            } => {
                let total_started = Instant::now();
                let before_session_id = runtime.state.attached_id;
                let before_context_started = Instant::now();
                let before_context_id = if plugin_id == "bmux.windows" {
                    if runtime.state.attached_context_id.is_some() {
                        runtime.state.attached_context_id
                    } else {
                        current_context_playbook(client)
                            .await
                            .ok()
                            .flatten()
                            .map(|context| context.id)
                    }
                } else {
                    None
                };
                let before_context_us = before_context_started.elapsed().as_micros();
                if let Some(context_id) = before_context_id {
                    runtime.state.attached_context_id = Some(context_id);
                    if !runtime.state.window_context_ids.contains(&context_id) {
                        runtime.state.window_context_ids.push(context_id);
                    }
                }
                let run_started = Instant::now();
                let mut selected_context_id = None;
                let mut window_cycle_timing = None;
                let response = if plugin_id == "bmux.windows" && command_name == "next-window" {
                    let (context_id, timing) =
                        cycle_known_window_playbook(client, runtime, false).await?;
                    selected_context_id = Some(context_id);
                    window_cycle_timing = Some(timing);
                    PluginCliCommandResponse::new(0)
                } else if plugin_id == "bmux.windows" && command_name == "prev-window" {
                    let (context_id, timing) =
                        cycle_known_window_playbook(client, runtime, true).await?;
                    selected_context_id = Some(context_id);
                    window_cycle_timing = Some(timing);
                    PluginCliCommandResponse::new(0)
                } else if plugin_id == "bmux.windows" && command_name == "goto-window" {
                    let (context_id, timing) =
                        goto_known_window_playbook(client, runtime, &args).await?;
                    selected_context_id = Some(context_id);
                    window_cycle_timing = Some(timing);
                    PluginCliCommandResponse::new(0)
                } else {
                    run_plugin_command_playbook(client, &plugin_id, &command_name, args).await?
                };
                let run_us = run_started.elapsed().as_micros();
                if let Some(timing) = window_cycle_timing {
                    emit_attach_phase_timing(&serde_json::json!({
                        "phase": "attach.window_cycle",
                        "plugin_id": plugin_id,
                        "command_name": command_name,
                        "known_contexts": timing.known_contexts,
                        "before_context_us": before_context_us,
                        "resolve_us": timing.resolve_us,
                        "invoke_us": timing.invoke_us,
                        "fallback_us": timing.fallback_us,
                        "total_us": timing.total_us,
                    }));
                }
                if let Some(error) = response.error {
                    emit_attach_phase_timing(&serde_json::json!({
                        "phase": "attach.plugin_command",
                        "plugin_id": plugin_id,
                        "command_name": command_name,
                        "status": "run_error",
                        "before_session_id": before_session_id,
                        "attached_session_id": runtime.state.attached_id,
                        "before_context_us": before_context_us,
                        "run_us": run_us,
                        "retarget_us": 0_u128,
                        "total_us": total_started.elapsed().as_micros(),
                    }));
                    bail!(
                        "plugin command {plugin_id}:{command_name} failed: {error} (exit_code={})",
                        response.exit_code
                    );
                }
                if response.exit_code != 0 {
                    emit_attach_phase_timing(&serde_json::json!({
                        "phase": "attach.plugin_command",
                        "plugin_id": plugin_id,
                        "command_name": command_name,
                        "status": "nonzero",
                        "before_session_id": before_session_id,
                        "attached_session_id": runtime.state.attached_id,
                        "before_context_us": before_context_us,
                        "run_us": run_us,
                        "retarget_us": 0_u128,
                        "total_us": total_started.elapsed().as_micros(),
                    }));
                    bail!(
                        "plugin command {plugin_id}:{command_name} exited with status {}",
                        response.exit_code
                    );
                }
                let retarget_started = Instant::now();
                if let Some(context_id) = selected_context_id {
                    retarget_attach_to_context_playbook(
                        client,
                        inspector,
                        runtime,
                        context_id,
                        0,
                        Some(&plugin_id),
                        Some(&command_name),
                    )
                    .await?;
                } else {
                    retarget_attach_to_current_context_playbook(
                        client,
                        inspector,
                        runtime,
                        Some(&plugin_id),
                        Some(&command_name),
                    )
                    .await?;
                }
                let retarget_us = retarget_started.elapsed().as_micros();
                if plugin_id == "bmux.windows"
                    && command_name == "new-window"
                    && let Some(context_id) = runtime.state.attached_context_id
                    && !runtime.state.window_context_ids.contains(&context_id)
                {
                    runtime.state.window_context_ids.push(context_id);
                }
                emit_attach_phase_timing(&serde_json::json!({
                    "phase": "attach.plugin_command",
                    "plugin_id": plugin_id,
                    "command_name": command_name,
                    "status": "ok",
                    "before_session_id": before_session_id,
                    "attached_session_id": runtime.state.attached_id,
                    "before_context_us": before_context_us,
                    "run_us": run_us,
                    "retarget_us": retarget_us,
                    "total_us": total_started.elapsed().as_micros(),
                }));
            }
            crate::input::RuntimeAction::Quit
            | crate::input::RuntimeAction::ToggleSplitDirection
            | crate::input::RuntimeAction::RestartFocusedPane
            | crate::input::RuntimeAction::ShowHelp
            | crate::input::RuntimeAction::BeginSelection
            | crate::input::RuntimeAction::MoveCursorLeft
            | crate::input::RuntimeAction::MoveCursorRight
            | crate::input::RuntimeAction::MoveCursorUp
            | crate::input::RuntimeAction::MoveCursorDown
            | crate::input::RuntimeAction::CopyScrollback
            | crate::input::RuntimeAction::EnterWindowMode
            | crate::input::RuntimeAction::ExitMode
            | crate::input::RuntimeAction::WindowPrev
            | crate::input::RuntimeAction::WindowNext
            | crate::input::RuntimeAction::WindowGoto1
            | crate::input::RuntimeAction::WindowGoto2
            | crate::input::RuntimeAction::WindowGoto3
            | crate::input::RuntimeAction::WindowGoto4
            | crate::input::RuntimeAction::WindowGoto5
            | crate::input::RuntimeAction::WindowGoto6
            | crate::input::RuntimeAction::WindowGoto7
            | crate::input::RuntimeAction::WindowGoto8
            | crate::input::RuntimeAction::WindowGoto9
            | crate::input::RuntimeAction::WindowClose
            | crate::input::RuntimeAction::EnterMode(_)
            | crate::input::RuntimeAction::SwitchProfile(_) => {}
        }
        runtime
            .processor
            .set_scroll_mode(runtime.state.scrollback_active);
    }
    Ok(())
}

fn crossterm_event_from_stroke(stroke: KeyStroke) -> CrosstermEvent {
    let key_code = match stroke.key {
        BmuxKeyCode::Char(c) => CrosstermKeyCode::Char(c),
        BmuxKeyCode::Enter => CrosstermKeyCode::Enter,
        BmuxKeyCode::Tab => CrosstermKeyCode::Tab,
        BmuxKeyCode::Backspace => CrosstermKeyCode::Backspace,
        BmuxKeyCode::Delete => CrosstermKeyCode::Delete,
        BmuxKeyCode::Escape => CrosstermKeyCode::Esc,
        BmuxKeyCode::Space => CrosstermKeyCode::Char(' '),
        BmuxKeyCode::Up => CrosstermKeyCode::Up,
        BmuxKeyCode::Down => CrosstermKeyCode::Down,
        BmuxKeyCode::Left => CrosstermKeyCode::Left,
        BmuxKeyCode::Right => CrosstermKeyCode::Right,
        BmuxKeyCode::Home => CrosstermKeyCode::Home,
        BmuxKeyCode::End => CrosstermKeyCode::End,
        BmuxKeyCode::PageUp => CrosstermKeyCode::PageUp,
        BmuxKeyCode::PageDown => CrosstermKeyCode::PageDown,
        BmuxKeyCode::Insert => CrosstermKeyCode::Insert,
        BmuxKeyCode::F(value) => CrosstermKeyCode::F(value),
    };

    let mut modifiers = KeyModifiers::NONE;
    if stroke.modifiers.ctrl {
        modifiers |= KeyModifiers::CONTROL;
    }
    if stroke.modifiers.alt {
        modifiers |= KeyModifiers::ALT;
    }
    if stroke.modifiers.shift {
        modifiers |= KeyModifiers::SHIFT;
    }
    if stroke.modifiers.super_key {
        modifiers |= KeyModifiers::SUPER;
    }

    CrosstermEvent::Key(KeyEvent {
        code: key_code,
        modifiers,
        kind: KeyEventKind::Press,
        state: KeyEventState::NONE,
    })
}

pub(super) fn require_session(session_id: Option<Uuid>) -> Result<Uuid> {
    session_id.context("no session — use new-session first")
}

pub(super) fn require_attached(attached: bool) -> Result<()> {
    if !attached {
        bail!("not attached to a session");
    }
    Ok(())
}

/// Drain output from the attached session until idle.
///
/// "Idle" is defined as `idle_threshold` consecutive empty reads separated by
/// 25ms gaps. The default threshold is 5 consecutive empty reads (125ms of
/// silence). For the `wait-for` polling loop, a lower threshold of 3 is
/// acceptable since the outer loop will re-drain on the next iteration.
///
/// Optionally captures output bytes to a display track writer for GIF export.
pub(super) async fn drain_output_until_idle(
    client: &mut BmuxClient,
    inspector: &mut ScreenInspector,
    session_id: Uuid,
    max_wait: Duration,
    display_track: &mut Option<super::display_track::PlaybookDisplayTrackWriter>,
    visual_interactive: &mut Option<VisualInteractiveState>,
    attached: bool,
) -> Result<()> {
    drain_output_with_threshold(
        client,
        inspector,
        session_id,
        max_wait,
        display_track,
        visual_interactive,
        attached,
        5,
    )
    .await
}

/// Same as `drain_output_until_idle` but with a configurable idle threshold.
#[allow(clippy::too_many_arguments)]
pub(super) async fn drain_output_with_threshold(
    client: &mut BmuxClient,
    inspector: &mut ScreenInspector,
    session_id: Uuid,
    max_wait: Duration,
    display_track: &mut Option<super::display_track::PlaybookDisplayTrackWriter>,
    visual_interactive: &mut Option<VisualInteractiveState>,
    attached: bool,
    idle_threshold: u8,
) -> Result<()> {
    let started = Instant::now();
    let mut idle_polls = 0u8;

    while started.elapsed() < max_wait {
        visual_checkpoint_during_step(
            visual_interactive,
            client,
            inspector,
            Some(session_id),
            attached,
        )
        .await?;

        let drain = inspector
            .drain_incremental_output(client, session_id, ATTACH_OUTPUT_MAX_BYTES)
            .await
            .map_err(|e| anyhow::anyhow!("drain output failed: {e}"))?;

        if !drain.focused_output.is_empty()
            && let Some(ref mut dt) = *display_track
        {
            let _ = dt.record_frame_bytes(&drain.focused_output);
        }

        if drain.had_activity {
            idle_polls = 0;
            tokio::time::sleep(Duration::from_millis(10)).await;
        } else if !drain.output_still_pending && !drain.any_sync_update_active {
            idle_polls += 1;
            if idle_polls >= idle_threshold {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        } else {
            idle_polls = 0;
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    Ok(())
}

/// Recursively copy a directory and its contents.
fn copy_dir_recursive(src: &std::path::Path, dst: &std::path::Path) -> Result<()> {
    std::fs::create_dir_all(dst).with_context(|| format!("failed creating {}", dst.display()))?;
    for entry in
        std::fs::read_dir(src).with_context(|| format!("failed reading {}", src.display()))?
    {
        let entry = entry?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());
        if src_path.is_dir() {
            copy_dir_recursive(&src_path, &dst_path)?;
        } else {
            std::fs::copy(&src_path, &dst_path).with_context(|| {
                format!(
                    "failed copying {} -> {}",
                    src_path.display(),
                    dst_path.display()
                )
            })?;
        }
    }
    Ok(())
}

/// Match a server event against a user-specified event name string.
fn event_matches(event: &bmux_ipc::Event, name: &str) -> bool {
    matches!(
        (event, name),
        (bmux_ipc::Event::ServerStarted, "server_started")
            | (bmux_ipc::Event::ServerStopping, "server_stopping")
            | (bmux_ipc::Event::SessionCreated { .. }, "session_created")
            | (bmux_ipc::Event::SessionRemoved { .. }, "session_removed")
            | (bmux_ipc::Event::ClientAttached { .. }, "client_attached")
            | (bmux_ipc::Event::ClientDetached { .. }, "client_detached")
            | (
                bmux_ipc::Event::AttachViewChanged { .. },
                "attach_view_changed"
            )
            | (
                bmux_ipc::Event::PaneOutputAvailable { .. },
                "pane_output_available"
            )
            | (bmux_ipc::Event::PaneOutput { .. }, "pane_output")
            | (
                bmux_ipc::Event::PaneImageAvailable { .. },
                "pane_image_available"
            )
            | (bmux_ipc::Event::PaneExited { .. }, "pane_exited")
            | (bmux_ipc::Event::PaneRestarted { .. }, "pane_restarted")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_attach_info() -> bmux_client::AttachOpenInfo {
        bmux_client::AttachOpenInfo {
            context_id: Some(Uuid::nil()),
            session_id: Uuid::nil(),
            can_write: true,
        }
    }

    fn key_event(code: CrosstermKeyCode, modifiers: KeyModifiers) -> CrosstermEvent {
        CrosstermEvent::Key(KeyEvent {
            code,
            modifiers,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        })
    }

    #[test]
    fn attach_input_uses_default_modal_window_bindings() {
        let mut runtime = AttachInputRuntime::new(test_attach_info());

        assert_eq!(
            runtime
                .processor
                .process_terminal_event(key_event(CrosstermKeyCode::Char('c'), KeyModifiers::NONE)),
            vec![crate::input::RuntimeAction::PluginCommand {
                plugin_id: "bmux.windows".to_string(),
                command_name: "new-window".to_string(),
                args: Vec::new(),
            }]
        );
        assert_eq!(runtime.state.attached_context_id, Some(Uuid::nil()));
        assert_eq!(runtime.state.window_context_ids, vec![Uuid::nil()]);
    }

    #[test]
    fn attach_input_maps_ctrl_s_to_next_window_plugin_command() {
        let mut runtime = AttachInputRuntime::new(test_attach_info());

        assert_eq!(
            runtime.processor.process_terminal_event(key_event(
                CrosstermKeyCode::Char('s'),
                KeyModifiers::CONTROL
            )),
            vec![crate::input::RuntimeAction::PluginCommand {
                plugin_id: "bmux.windows".to_string(),
                command_name: "next-window".to_string(),
                args: Vec::new(),
            }]
        );
    }

    #[test]
    fn parse_interactive_prompt_command_supports_shortcuts_and_defaults() {
        assert_eq!(
            parse_interactive_prompt_command("").expect("empty means next"),
            InteractivePromptCommand::RunNextStep
        );
        assert_eq!(
            parse_interactive_prompt_command("n").expect("n should parse"),
            InteractivePromptCommand::RunNextStep
        );
        assert_eq!(
            parse_interactive_prompt_command("c").expect("c should parse"),
            InteractivePromptCommand::ContinueRemaining
        );
        assert_eq!(
            parse_interactive_prompt_command("l").expect("l should parse"),
            InteractivePromptCommand::ContinueRemaining
        );
        assert_eq!(
            parse_interactive_prompt_command("s").expect("s should parse"),
            InteractivePromptCommand::ShowScreen
        );
        assert_eq!(
            parse_interactive_prompt_command("q").expect("q should parse"),
            InteractivePromptCommand::AbortRun
        );
    }

    #[test]
    fn parse_interactive_prompt_command_parses_inline_dsl() {
        let command = parse_interactive_prompt_command(": send-keys keys='echo hi\\r'")
            .expect("dsl should parse");
        assert_eq!(
            command,
            InteractivePromptCommand::RunDsl("send-keys keys='echo hi\\r'".to_string())
        );
    }

    #[test]
    fn parse_interactive_prompt_command_rejects_unknown_input() {
        let error = parse_interactive_prompt_command("mystery").expect_err("should fail");
        assert!(
            error
                .to_string()
                .contains("unknown interactive command 'mystery'"),
            "unexpected error: {error:#}"
        );
    }

    #[test]
    fn resolve_interactive_mode_prefers_visual_for_tty() {
        assert_eq!(
            resolve_interactive_mode(true, true, true),
            PlaybookInteractiveMode::Visual
        );
        assert_eq!(
            resolve_interactive_mode(true, true, false),
            PlaybookInteractiveMode::Prompt
        );
        assert_eq!(
            resolve_interactive_mode(false, true, true),
            PlaybookInteractiveMode::Disabled
        );
    }

    #[test]
    fn parse_visual_control_action_maps_live_controls() {
        let make_key = |code, modifiers| KeyEvent {
            code,
            modifiers,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        };

        assert_eq!(
            parse_visual_control_action(make_key(CrosstermKeyCode::Char(' '), KeyModifiers::NONE)),
            Some(VisualControlAction::TogglePause)
        );
        assert_eq!(
            parse_visual_control_action(make_key(CrosstermKeyCode::Char('l'), KeyModifiers::NONE)),
            Some(VisualControlAction::ContinueLive)
        );
        assert_eq!(
            parse_visual_control_action(make_key(CrosstermKeyCode::Char('n'), KeyModifiers::NONE)),
            Some(VisualControlAction::StepOnce)
        );
        assert_eq!(
            parse_visual_control_action(make_key(CrosstermKeyCode::Char(':'), KeyModifiers::NONE)),
            Some(VisualControlAction::PromptDsl)
        );
        assert_eq!(
            parse_visual_control_action(make_key(
                CrosstermKeyCode::Char('c'),
                KeyModifiers::CONTROL
            )),
            Some(VisualControlAction::Abort)
        );
    }
}
