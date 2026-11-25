//! `bmux drive`: run a terminal program in an isolated bmux and drive it the
//! way an agent tests a TUI, with feedback after every action.
//!
//! Every action (typing, keys, clicks, resizes) waits until the program has
//! settled instead of sleeping, then reports the screen with its changed rows
//! marked, the styled cells that stand out (selections, errors), the terminal
//! modes the program enabled, what it wrote to draw the change, and any new
//! terminal-protocol problems. Sessions persist between invocations, so each
//! command is a short, independent call; every action is recorded as a
//! replayable script that can be rerun at several sizes.

pub mod cli;
mod keys;
mod lint;
pub mod mcp;
mod observe;
mod png;
mod script;
mod session;
mod settle;

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, bail};
use bmux_client::BmuxClient;
use serde::Serialize;

pub use lint::{Finding, LintReport, Severity};
pub use observe::{Observation, ScreenDetail};
pub use settle::Settled;

use session::SessionRecord;
use settle::SettleOptions;

/// Default quiet window: output and CPU still for this long means settled.
pub const DEFAULT_QUIET: Duration = Duration::from_millis(200);
/// How long an action waits for the program to settle.
pub const DEFAULT_ACTION_TIMEOUT: Duration = Duration::from_secs(5);
/// Quiet window between keys of one `press` (a quick typist's pace).
const KEY_GAP: Duration = Duration::from_millis(40);
/// Longest wait between keys of one `press`.
const KEY_GAP_MAX: Duration = Duration::from_millis(1000);
/// How long `start` waits for the program to draw its first screen.
pub const DEFAULT_START_TIMEOUT: Duration = Duration::from_secs(15);

/// What happened after one command.
#[derive(Debug, Clone, Serialize)]
pub struct Report {
    pub session: String,
    pub action: String,
    /// Whether the command did what was asked (an `expect` or `wait` met).
    pub ok: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub settled: Option<Settled>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output: Option<ActionOutput>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub screen: Option<Observation>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub new_findings: Vec<Finding>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lint: Option<LintReport>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub screenshot: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub script: Option<String>,
    /// Output that scrolled off the screen, as logical lines.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub history: Option<History>,
}

/// The main screen's text including scrolled-off lines.
#[derive(Debug, Clone, Serialize)]
pub struct History {
    /// Logical lines (soft-wrapped rows joined), last `lines.len()` of `total`.
    pub lines: Vec<String>,
    pub total: usize,
    /// The program is on the alternate screen: history is the main screen's,
    /// under it.
    pub alternate_screen: bool,
}

impl Report {
    fn new(session: &str, action: impl Into<String>) -> Self {
        Self {
            session: session.to_owned(),
            action: action.into(),
            ok: true,
            notes: Vec::new(),
            settled: None,
            output: None,
            screen: None,
            new_findings: Vec::new(),
            lint: None,
            screenshot: None,
            script: None,
            history: None,
        }
    }

    /// Render for an agent to read.
    pub fn render(&self, detail: ScreenDetail, all_spans: bool) -> String {
        let mut out = String::new();
        let _ = write!(out, "[{}] {}", self.session, self.action);
        if !self.ok {
            out.push_str(" -> FAILED");
        } else if self.action.starts_with("expect") {
            out.push_str(" -> ok");
        }
        if let Some(settled) = &self.settled {
            let _ = write!(out, " -> {}", settled.describe());
        }
        if let Some(output) = &self.output {
            let _ = write!(out, " | {}", output.describe());
        }
        out.push('\n');
        for note in &self.notes {
            let _ = writeln!(out, "note: {note}");
        }
        if let Some(history) = &self.history {
            let first = history.total - history.lines.len() + 1;
            let _ = writeln!(
                out,
                "history: lines {first}-{} of {}{}",
                history.total,
                history.total,
                if history.alternate_screen {
                    " (main screen, under the program's alternate screen)"
                } else {
                    ""
                }
            );
            let digits = history.total.to_string().len();
            for (index, line) in history.lines.iter().enumerate() {
                let _ = writeln!(out, "{:>digits$}|{line}", first + index);
            }
        }
        if let Some(screen) = &self.screen {
            out.push_str(&observe::render(screen, detail, all_spans));
        }
        if !self.new_findings.is_empty() {
            out.push_str("terminal protocol problems (new):\n");
            for finding in &self.new_findings {
                let _ = writeln!(out, "  {}", finding.render().replace('\n', "\n  "));
            }
        }
        if let Some(lint) = &self.lint {
            out.push_str(&lint.render());
        }
        if let Some(path) = &self.screenshot {
            let _ = writeln!(out, "screenshot: {}", path.display());
        }
        if let Some(script) = &self.script {
            out.push_str(script);
            if !script.ends_with('\n') {
                out.push('\n');
            }
        }
        out
    }
}

/// What the program wrote in response to an action.
#[derive(Debug, Clone, Serialize, Default)]
pub struct ActionOutput {
    pub bytes: u64,
    pub writes: u64,
    pub full_clears: u64,
    pub synchronized_frames: u64,
}

impl ActionOutput {
    fn describe(&self) -> String {
        if self.bytes == 0 {
            return "no output".to_owned();
        }
        let mut text = format!("wrote {} in {} writes", human_bytes(self.bytes), self.writes);
        if self.full_clears > 0 {
            let _ = write!(text, ", {} full clears", self.full_clears);
        }
        if self.synchronized_frames > 0 {
            let _ = write!(text, ", {} synchronized frames", self.synchronized_frames);
        }
        text
    }

    fn from_events(events: &[session::CaptureEvent]) -> Self {
        let mut output = Self::default();
        for event in events {
            if let session::CaptureEvent::Output { data, .. } = event {
                output.bytes += data.len() as u64;
                output.writes += 1;
                output.full_clears += count(data, b"\x1b[2J") + count(data, b"\x1b[3J");
                output.synchronized_frames += count(data, b"\x1b[?2026l");
            }
        }
        output
    }
}

fn count(haystack: &[u8], needle: &[u8]) -> u64 {
    haystack.windows(needle.len()).filter(|window| *window == needle).count() as u64
}

fn human_bytes(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else {
        #[allow(clippy::cast_precision_loss)] // Approximate display size.
        let kib = bytes as f64 / 1024.0;
        format!("{kib:.1} KiB")
    }
}

/// How to start a program.
#[derive(Debug, Clone)]
pub struct StartOptions {
    pub name: Option<String>,
    pub command: Vec<String>,
    pub cwd: Option<PathBuf>,
    pub env: BTreeMap<String, String>,
    pub cols: u16,
    pub rows: u16,
    pub timeout: Duration,
    pub quiet: Duration,
}

/// Input an agent sends.
#[derive(Debug, Clone)]
pub enum Input {
    Type { text: String, enter: bool },
    Press(Vec<String>),
    Paste(String),
    Click {
        row: u16,
        col: u16,
        button: String,
        double: bool,
    },
    Scroll {
        up: bool,
        count: u16,
        row: Option<u16>,
        col: Option<u16>,
    },
    Drag {
        from: (u16, u16),
        to: (u16, u16),
    },
}

/// What `wait` waits for.
#[derive(Debug, Clone)]
pub enum WaitFor {
    Text(String),
    Gone(String),
    Idle,
    Exit,
    Change,
}

/// What `expect` checks right now.
#[derive(Debug, Clone)]
pub enum Expect {
    Text(String),
    NoText(String),
    Cursor { row: u16, col: u16 },
    Running,
    Exited(Option<u32>),
    Style { text: String, style: String },
}

/// The shared engine behind the CLI and the MCP server.
pub struct Driver {
    bmux: PathBuf,
}

impl Driver {
    /// `bmux` is the executable that runs session servers.
    pub const fn new(bmux: PathBuf) -> Self {
        Self { bmux }
    }

    pub async fn start(&self, options: StartOptions) -> Result<Report> {
        let Some((program, args)) = options.command.split_first() else {
            bail!("no program given: `bmux drive start -- <program> [args...]`");
        };
        anyhow::ensure!(
            (2..=1000).contains(&options.cols) && (2..=1000).contains(&options.rows),
            "size must be between 2x2 and 1000x1000"
        );
        let name = match &options.name {
            Some(name) => {
                session::validate_name(name)?;
                if session::session_dir(name).exists() {
                    bail!(
                        "a drive session named '{name}' already exists; stop it with `bmux drive stop -s {name}` or pick another name"
                    );
                }
                name.clone()
            }
            None => session::default_name(program),
        };
        let cwd = match &options.cwd {
            Some(cwd) => std::path::absolute(cwd)?,
            None => std::env::current_dir().context("no current directory")?,
        };
        let resolved = session::resolve_program(program, &cwd)?;
        let dir = session::session_dir(&name);
        if let Err(error) = session::start_server(&dir, &self.bmux).await {
            let _ = std::fs::remove_dir_all(&dir);
            return Err(error);
        }
        let started = async {
            let mut client = session::connect(&dir).await?;
            let execution = session::launch(
                &mut client,
                session::LaunchOptions {
                    program: resolved,
                    args: args.to_vec(),
                    cwd: cwd.clone(),
                    env: options.env.clone(),
                    cols: options.cols,
                    rows: options.rows,
                },
            )
            .await?;
            let record = SessionRecord {
                version: session::state_version(),
                name: name.clone(),
                command: options.command.clone(),
                cwd: cwd.display().to_string(),
                cols: options.cols,
                rows: options.rows,
                start_cols: options.cols,
                start_rows: options.rows,
                session_id: execution.session_id,
                pane_id: execution.pane_id,
                execution_token: execution.execution_token,
                created_at_ms: session::now_ms(),
                last_rows: Vec::new(),
                last_row_styles: Vec::new(),
                capture_offset: 0,
                reported_findings: Vec::new(),
            };
            session::write_record(&dir, &record)?;
            Ok::<_, anyhow::Error>((client, record))
        }
        .await;
        let (mut client, mut record) = match started {
            Ok(started) => started,
            Err(error) => {
                let _ = session::stop_server(&dir).await;
                return Err(error);
            }
        };
        script::append(&dir, &script::start_line(&options, &cwd))?;
        let _lock = session::lock_session(&dir)?;
        let mut report = Report::new(&name, format!("start {}", shell_join(&options.command)));
        let settled = settle::settle(
            &mut client,
            &record,
            SettleOptions::new(options.quiet, options.timeout),
        )
        .await?;
        self.finish_action(&dir, &mut client, &mut record, &mut report, settled)
            .await?;
        if !session::sandbox_paths(&dir).runtime_dir.exists() {
            report.notes.push("session server runtime missing".into());
        }
        Ok(report)
    }

    pub async fn input(
        &self,
        name: Option<&str>,
        input: Input,
        quiet: Duration,
        timeout: Duration,
    ) -> Result<Report> {
        let (dir, mut record) = session::resolve_session(name)?;
        let _lock = session::lock_session(&dir)?;
        let mut client = session::connect(&dir).await?;
        let status = session::status(&mut client, &record).await?;
        if !settle::is_running(&status) {
            let exited = settle::exited_from_status(&status, Duration::ZERO);
            bail!(
                "the program in '{}' is no longer running ({}); look at its last screen with \
                 `bmux drive look`, or `bmux drive stop` it",
                record.name,
                exited.describe()
            );
        }
        let screen = self.observe(&mut client, &record).await?;
        let mut report = Report::new(&record.name, describe_input(&input));
        if let Some(writes) = self.encode_input(&input, &screen, &mut report)? {
            let count = writes.len();
            for (index, bytes) in writes.into_iter().enumerate() {
                session::send(&mut client, &record, bytes).await?;
                if index + 1 < count {
                    // Keys arrive one by one, as typed: let the program react to each.
                    settle::settle(&mut client, &record, SettleOptions::new(KEY_GAP, KEY_GAP_MAX))
                        .await?;
                }
            }
            script::append(&dir, &script::input_line(&input))?;
        }
        let settled =
            settle::settle(&mut client, &record, SettleOptions::new(quiet, timeout)).await?;
        self.finish_action(&dir, &mut client, &mut record, &mut report, settled)
            .await?;
        Ok(report)
    }

    fn encode_input(
        &self,
        input: &Input,
        screen: &Observation,
        report: &mut Report,
    ) -> Result<Option<Vec<Vec<u8>>>> {
        let key_modes = keys::KeyModes {
            application_cursor: screen.modes.application_cursor_keys,
        };
        let mouse = keys::MouseModes {
            reporting: keys::MouseReporting::from_name(&screen.modes.mouse),
            sgr: screen.modes.mouse_encoding == "sgr",
        };
        Ok(Some(match input {
            Input::Type { text, enter } => {
                let mut bytes = keys::encode_text(text);
                if *enter {
                    bytes.push(b'\r');
                }
                vec![bytes]
            }
            Input::Press(names) => keys::parse_keys(names)?
                .into_iter()
                .map(|press| keys::encode_key(press, key_modes))
                .collect(),
            Input::Paste(text) => {
                if !screen.modes.bracketed_paste {
                    report.notes.push(
                        "the program has not enabled bracketed paste; the text arrives as if typed"
                            .into(),
                    );
                }
                vec![keys::encode_paste(text, screen.modes.bracketed_paste)]
            }
            Input::Click {
                row,
                col,
                button,
                double,
            } => {
                check_cell(screen, *row, *col)?;
                let button = parse_button(button)?;
                let mods = keys::Mods::default();
                let press = keys::encode_mouse(
                    keys::MouseAction::Press(button),
                    *row,
                    *col,
                    mods,
                    mouse,
                );
                let Some(press) = press else {
                    report.notes.push(format!(
                        "the program has not enabled mouse reporting, so a real terminal would not \
                         send this click; nothing was sent{}",
                        screen
                            .word_at(*row, *col)
                            .map(|word| format!(" (row {row} col {col} shows {word:?})"))
                            .unwrap_or_default()
                    ));
                    report.ok = false;
                    return Ok(None);
                };
                let release = keys::encode_mouse(
                    keys::MouseAction::Release(button),
                    *row,
                    *col,
                    mods,
                    mouse,
                )
                .unwrap_or_default();
                if *double {
                    vec![press.clone(), release.clone(), press, release]
                } else {
                    vec![press, release]
                }
            }
            Input::Scroll {
                up,
                count,
                row,
                col,
            } => {
                let row = row.unwrap_or(screen.rows.div_ceil(2));
                let col = col.unwrap_or(screen.cols.div_ceil(2));
                check_cell(screen, row, col)?;
                let action = if *up {
                    keys::MouseAction::WheelUp
                } else {
                    keys::MouseAction::WheelDown
                };
                match keys::encode_mouse(action, row, col, keys::Mods::default(), mouse) {
                    Some(event) => vec![event; usize::from((*count).max(1))],
                    None => {
                        // Terminals turn the wheel into arrow keys on the alternate screen.
                        if screen.alternate_screen {
                            report.notes.push(
                                "mouse reporting is off; sent arrow keys, as terminals do for the \
                                 wheel on the alternate screen"
                                    .into(),
                            );
                            let key = keys::parse_key(if *up { "Up" } else { "Down" })?;
                            vec![keys::encode_key(key, key_modes); usize::from((*count).max(1)) * 3]
                        } else {
                            report.notes.push(
                                "mouse reporting is off and the program is not on the alternate \
                                 screen; a terminal would scroll its own history, so nothing was sent"
                                    .into(),
                            );
                            report.ok = false;
                            return Ok(None);
                        }
                    }
                }
            }
            Input::Drag { from, to } => {
                check_cell(screen, from.0, from.1)?;
                check_cell(screen, to.0, to.1)?;
                let button = keys::MouseButton::Left;
                let mods = keys::Mods::default();
                let Some(press) =
                    keys::encode_mouse(keys::MouseAction::Press(button), from.0, from.1, mods, mouse)
                else {
                    report.notes.push(
                        "the program has not enabled mouse reporting; nothing was sent".into(),
                    );
                    report.ok = false;
                    return Ok(None);
                };
                let mut writes = vec![press];
                if let Some(motion) =
                    keys::encode_mouse(keys::MouseAction::Drag(button), to.0, to.1, mods, mouse)
                {
                    writes.push(motion);
                } else {
                    report.notes.push(
                        "the program reports clicks but not drags; only press and release were sent"
                            .into(),
                    );
                }
                writes.extend(keys::encode_mouse(
                    keys::MouseAction::Release(button),
                    to.0,
                    to.1,
                    mods,
                    mouse,
                ));
                writes
            }
        }))
    }

    pub async fn resize(
        &self,
        name: Option<&str>,
        cols: u16,
        rows: u16,
        quiet: Duration,
        timeout: Duration,
    ) -> Result<Report> {
        anyhow::ensure!(
            (2..=1000).contains(&cols) && (2..=1000).contains(&rows),
            "size must be between 2x2 and 1000x1000"
        );
        let (dir, mut record) = session::resolve_session(name)?;
        let _lock = session::lock_session(&dir)?;
        let mut client = session::connect(&dir).await?;
        session::resize(&mut client, &record, cols, rows).await?;
        record.cols = cols;
        record.rows = rows;
        script::append(&dir, &format!("resize {cols}x{rows}"))?;
        let mut report = Report::new(&record.name, format!("resize {cols}x{rows}"));
        let settled =
            settle::settle(&mut client, &record, SettleOptions::new(quiet, timeout)).await?;
        self.finish_action(&dir, &mut client, &mut record, &mut report, settled)
            .await?;
        if let Some(screen) = &report.screen
            && (screen.cols, screen.rows) != (cols, rows)
        {
            report.notes.push(format!(
                "the screen is {}x{}, not the requested {cols}x{rows}",
                screen.cols, screen.rows
            ));
        }
        Ok(report)
    }

    pub async fn look(&self, name: Option<&str>) -> Result<Report> {
        let (dir, mut record) = session::resolve_session(name)?;
        let _lock = session::lock_session(&dir)?;
        let mut client = session::connect(&dir).await?;
        let mut report = Report::new(&record.name, "look");
        let status = session::status(&mut client, &record).await?;
        if !settle::is_running(&status) {
            report.settled = Some(settle::exited_from_status(&status, Duration::ZERO));
            report.notes.push(format!(
                "execution phase {:?}, terminal session {:?}{}",
                status.phase,
                status.session_end,
                status
                    .error
                    .as_deref()
                    .map(|error| format!(", error: {error}"))
                    .unwrap_or_default()
            ));
        }
        let screen = self.observe(&mut client, &record).await?;
        remember(&mut record, &screen);
        report.screen = Some(screen);
        session::write_record(&dir, &record)?;
        Ok(report)
    }

    /// The last `lines` lines of output, including what scrolled off screen.
    pub async fn history(&self, name: Option<&str>, lines: usize) -> Result<Report> {
        let (dir, record) = session::resolve_session(name)?;
        let _lock = session::lock_session(&dir)?;
        let mut client = session::connect(&dir).await?;
        let (events, _, truncated) = session::capture_from(&mut client, &record, 0).await?;
        let (cols, rows) = start_size(&record);
        let mut stream = bmux_terminal_grid::TerminalGridStream::new(
            cols,
            rows,
            bmux_terminal_grid::GridLimits {
                scrollback_rows: 100_000,
            },
        )
        .context("invalid session size")?;
        for event in &events {
            match event {
                session::CaptureEvent::Output { data } => stream.process(data),
                session::CaptureEvent::Resize { cols, rows } => {
                    let _ = stream.resize(*cols, *rows);
                }
            }
        }
        let grid = stream.grid();
        let alternate = grid.mode() == bmux_terminal_grid::GridMode::Alternate;
        let text = if alternate {
            let width = grid.width();
            grid.main_content_rows()
                .iter()
                .map(|row| bmux_terminal_grid::row_text(row, width).trim_end().to_owned())
                .collect::<Vec<_>>()
                .join("\n")
        } else {
            bmux_terminal_grid::content_text(grid)
        };
        let all: Vec<String> = text.lines().map(str::to_owned).collect();
        let total = all.len();
        let mut report = Report::new(&record.name, format!("look --history {lines}"));
        report.history = Some(History {
            lines: all[total.saturating_sub(lines)..].to_vec(),
            total,
            alternate_screen: alternate,
        });
        if truncated {
            report
                .notes
                .push("the output is too large to replay whole; history covers its start".into());
        }
        Ok(report)
    }

    /// Mirror a session's screen in this terminal, read-only, until Ctrl-C or
    /// the session ends. Sends nothing to the program and never resizes it.
    pub async fn watch(&self, name: Option<&str>) -> Result<()> {
        use std::io::Write as _;
        let (dir, record) = session::resolve_session(name)?;
        let mut client = session::connect(&dir).await?;
        let mut stdout = std::io::stdout();
        // Mirror on the alternate screen, and restore everything on the way out.
        stdout.write_all(b"\x1b[?1049h\x1b[?25l")?;
        stdout.flush()?;
        let mirror = async {
            let mut last = Vec::new();
            loop {
                let screen = session::screen(&mut client, &record).await?;
                let status = session::status(&mut client, &record).await?;
                let snapshot: bmux_terminal_grid::GridSnapshot =
                    serde_json::from_slice(&screen.encoded_grid).context("undecodable screen")?;
                let stream = bmux_terminal_grid::TerminalGridStream::from_snapshot(
                    &snapshot,
                    bmux_terminal_grid::GridLimits::default(),
                )
                .map_err(|error| anyhow::anyhow!("unreadable screen: {error:?}"))?;
                let mut frame = b"\x1b[?2026h".to_vec();
                frame.extend(bmux_terminal_grid::full_screen_repaint_bytes(stream.grid()));
                let state = if settle::is_running(&status) {
                    "running".to_owned()
                } else {
                    settle::exited_from_status(&status, Duration::ZERO).describe()
                };
                frame.extend(
                    format!(
                        "\x1b[{};1H\x1b[0m\x1b[7m watching {} {}x{}: {state} (Ctrl-C stops) \x1b[0m\x1b[K\x1b[?2026l",
                        screen.rows.saturating_add(2),
                        record.name,
                        screen.cols,
                        screen.rows,
                    )
                    .as_bytes(),
                );
                if frame != last {
                    stdout.write_all(&frame)?;
                    stdout.flush()?;
                    last = frame;
                }
                if !settle::is_running(&status) {
                    // Keep the last screen up until the viewer stops.
                    std::future::pending::<()>().await;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            #[allow(unreachable_code)]
            Ok::<(), anyhow::Error>(())
        };
        let result = tokio::select! {
            result = mirror => result,
            _ = tokio::signal::ctrl_c() => Ok(()),
        };
        stdout.write_all(b"\x1b[0m\x1b[?25h\x1b[?1049l")?;
        stdout.flush()?;
        result
    }

    pub async fn screenshot(&self, name: Option<&str>, path: Option<PathBuf>) -> Result<Report> {
        let (dir, record) = session::resolve_session(name)?;
        let _lock = session::lock_session(&dir)?;
        let mut client = session::connect(&dir).await?;
        let screen = session::screen(&mut client, &record).await?;
        let snapshot: bmux_terminal_grid::GridSnapshot =
            serde_json::from_slice(&screen.encoded_grid).context("undecodable screen")?;
        let png = png::render_png(&snapshot)?;
        let path = match path {
            Some(path) => path,
            None => std::env::temp_dir().join(format!(
                "bmux-drive-{}-{}.png",
                record.name,
                session::now_ms()
            )),
        };
        std::fs::write(&path, png)
            .with_context(|| format!("failed writing screenshot {}", path.display()))?;
        let mut report = Report::new(&record.name, "screenshot");
        report.screenshot = Some(std::path::absolute(&path).unwrap_or(path));
        Ok(report)
    }

    pub async fn screenshot_bytes(&self, name: Option<&str>) -> Result<(String, Vec<u8>)> {
        let (dir, record) = session::resolve_session(name)?;
        let _lock = session::lock_session(&dir)?;
        let mut client = session::connect(&dir).await?;
        let screen = session::screen(&mut client, &record).await?;
        let snapshot: bmux_terminal_grid::GridSnapshot =
            serde_json::from_slice(&screen.encoded_grid).context("undecodable screen")?;
        Ok((record.name, png::render_png(&snapshot)?))
    }

    pub async fn wait(
        &self,
        name: Option<&str>,
        condition: WaitFor,
        quiet: Duration,
        timeout: Duration,
    ) -> Result<Report> {
        let (dir, mut record) = session::resolve_session(name)?;
        let _lock = session::lock_session(&dir)?;
        let mut client = session::connect(&dir).await?;
        let mut report = Report::new(&record.name, describe_wait(&condition));
        let start = Instant::now();
        let pattern = match &condition {
            WaitFor::Text(pattern) | WaitFor::Gone(pattern) => Some(
                regex::Regex::new(pattern)
                    .with_context(|| format!("invalid regular expression {pattern:?}"))?,
            ),
            _ => None,
        };
        let initial = self.observe(&mut client, &record).await?.text();
        let settled = loop {
            let status = session::status(&mut client, &record).await?;
            let running = settle::is_running(&status);
            let met = match (&condition, &pattern) {
                (WaitFor::Text(_), Some(pattern)) => {
                    pattern.is_match(&self.observe(&mut client, &record).await?.text())
                }
                (WaitFor::Gone(_), Some(pattern)) => {
                    !pattern.is_match(&self.observe(&mut client, &record).await?.text())
                }
                (WaitFor::Exit, _) => !running,
                (WaitFor::Change, _) => self.observe(&mut client, &record).await?.text() != initial,
                (WaitFor::Idle, _) => {
                    let remaining = timeout.saturating_sub(start.elapsed());
                    let settled = settle::settle(
                        &mut client,
                        &record,
                        SettleOptions::new(quiet, remaining),
                    )
                    .await?;
                    report.ok = !matches!(settled, Settled::Busy { .. });
                    break settled;
                }
                _ => false,
            };
            if met {
                let _ = write!(report.action, " (met after {}ms)", settle::millis(start.elapsed()));
                // Let the frame that met the condition finish drawing.
                break settle::settle(
                    &mut client,
                    &record,
                    SettleOptions::new(quiet, Duration::from_secs(2)),
                )
                .await?;
            }
            if !running {
                report.ok = false;
                report
                    .notes
                    .push("the program exited before the condition was met".into());
                break settle::exited_from_status(&status, start.elapsed());
            }
            if start.elapsed() >= timeout {
                report.ok = false;
                report.notes.push(format!(
                    "condition not met within {}ms",
                    settle::millis(timeout)
                ));
                // Report whether the program is still working or waiting.
                break settle::settle(
                    &mut client,
                    &record,
                    SettleOptions::new(quiet, Duration::from_secs(1)),
                )
                .await?;
            }
            tokio::time::sleep(Duration::from_millis(30)).await;
        };
        if report.ok && matches!(condition, WaitFor::Text(_) | WaitFor::Gone(_)) {
            script::append(&dir, &script::wait_line(&condition, timeout))?;
        }
        self.finish_action(&dir, &mut client, &mut record, &mut report, settled)
            .await?;
        Ok(report)
    }

    pub async fn expect(&self, name: Option<&str>, expectation: Expect) -> Result<Report> {
        let (dir, mut record) = session::resolve_session(name)?;
        let _lock = session::lock_session(&dir)?;
        let mut client = session::connect(&dir).await?;
        let status = session::status(&mut client, &record).await?;
        let screen = self.observe(&mut client, &record).await?;
        let mut report = Report::new(&record.name, describe_expect(&expectation));
        let failure = check_expectation(&expectation, &screen, &status);
        if let Some(failure) = failure {
            report.ok = false;
            report.notes.push(failure);
            report.screen = Some(screen.clone());
        } else {
            script::append(&dir, &script::expect_line(&expectation))?;
        }
        remember(&mut record, &screen);
        session::write_record(&dir, &record)?;
        Ok(report)
    }

    pub async fn lint(&self, name: Option<&str>) -> Result<Report> {
        let (dir, record) = session::resolve_session(name)?;
        let _lock = session::lock_session(&dir)?;
        let mut client = session::connect(&dir).await?;
        let status = session::status(&mut client, &record).await?;
        let running = settle::is_running(&status);
        let mut report = Report::new(&record.name, "lint");
        report.lint = Some(self.lint_report(&mut client, &record, !running).await?);
        if running {
            report.notes.push(
                "the program is still running: modes it has on are reported when it exits \
                 (`bmux drive stop` or quit it, then lint again)"
                    .into(),
            );
        }
        Ok(report)
    }

    async fn lint_report(
        &self,
        client: &mut BmuxClient,
        record: &SessionRecord,
        exited: bool,
    ) -> Result<LintReport> {
        let (events, _, truncated) = session::capture_from(client, record, 0).await?;
        let (cols, rows) = start_size(record);
        Ok(lint::lint(&events, cols, rows, exited, truncated))
    }

    pub async fn stop(&self, name: Option<&str>, grace: Duration) -> Result<Report> {
        let (dir, record) = session::resolve_session(name)?;
        let mut report = Report::new(&record.name, "stop");
        let stopped = async {
            let _lock = session::lock_session(&dir)?;
            let mut client = session::connect(&dir).await?;
            let status = session::status(&mut client, &record).await?;
            let settled = if settle::is_running(&status) {
                let start = Instant::now();
                let status = session::terminate(&mut client, &record, grace).await?;
                report.notes.push(
                    "the program was still running, so it was stopped with a signal; quit it with \
                     its own keys first to check its normal exit path"
                        .into(),
                );
                settle::exited_from_status(&status, start.elapsed())
            } else {
                settle::exited_from_status(&status, Duration::ZERO)
            };
            report.settled = Some(settled);
            if let Ok(screen) = self.observe(&mut client, &record).await {
                report.screen = Some(screen);
            }
            report.lint = Some(self.lint_report(&mut client, &record, true).await?);
            report.script = Some(script::read(&dir)?);
            Ok::<_, anyhow::Error>(())
        }
        .await;
        if let Err(error) = stopped {
            report
                .notes
                .push(format!("the session's server was not reachable: {error:#}"));
        }
        session::stop_server(&dir).await?;
        Ok(report)
    }

    pub fn list(&self) -> Vec<SessionSummary> {
        session::list_sessions()
            .into_iter()
            .map(|record| SessionSummary {
                name: record.name,
                command: record.command,
                cwd: record.cwd,
                size: format!("{}x{}", record.cols, record.rows),
                age_secs: session::now_ms().saturating_sub(record.created_at_ms) / 1000,
            })
            .collect()
    }

    pub fn script(&self, name: Option<&str>) -> Result<Report> {
        let (dir, record) = session::resolve_session(name)?;
        let mut report = Report::new(&record.name, "script");
        report.script = Some(script::read(&dir)?);
        Ok(report)
    }

    /// Replay a script at each size; one report per size.
    pub async fn run_script(
        &self,
        source: &str,
        sizes: &[(u16, u16)],
        quiet: Duration,
    ) -> Result<Vec<ScriptRun>> {
        let steps = script::parse(source)?;
        let mut runs = Vec::new();
        let sizes: Vec<Option<(u16, u16)>> = if sizes.is_empty() {
            vec![None]
        } else {
            sizes.iter().copied().map(Some).collect()
        };
        for size in sizes {
            runs.push(self.run_script_once(&steps, size, quiet).await);
        }
        Ok(runs)
    }

    async fn run_script_once(
        &self,
        steps: &[script::Step],
        size: Option<(u16, u16)>,
        quiet: Duration,
    ) -> ScriptRun {
        let name = session::default_name("script-run");
        let mut run = ScriptRun {
            size: size.map(|(cols, rows)| format!("{cols}x{rows}")),
            passed: true,
            failed_step: None,
            failure: None,
            steps: 0,
            final_screen: None,
            findings: Vec::new(),
        };
        let mut started = false;
        for (index, step) in steps.iter().enumerate() {
            let result = self
                .run_step(&name, step, size, quiet, &mut started)
                .await;
            run.steps = index + 1;
            match result {
                Ok(report) if report.ok => {}
                Ok(report) => {
                    run.passed = false;
                    run.failed_step = Some(format!("line {}: {}", step.line, step.text));
                    run.failure = Some(report.notes.join("; "));
                    run.final_screen = report.screen.map(|screen| screen.text());
                    break;
                }
                Err(error) => {
                    run.passed = false;
                    run.failed_step = Some(format!("line {}: {}", step.line, step.text));
                    run.failure = Some(format!("{error:#}"));
                    break;
                }
            }
        }
        if started {
            match self.stop(Some(&name), Duration::from_millis(500)).await {
                Ok(report) => {
                    if run.final_screen.is_none() && !run.passed {
                        run.final_screen = report.screen.map(|screen| screen.text());
                    }
                    run.findings = report.lint.map(|lint| lint.findings).unwrap_or_default();
                }
                Err(error) => {
                    run.findings.push(Finding {
                        id: "stop".into(),
                        severity: Severity::Warning,
                        message: format!("stopping the run failed: {error:#}"),
                        fix: None,
                    });
                }
            }
        }
        run
    }

    async fn run_step(
        &self,
        name: &str,
        step: &script::Step,
        size: Option<(u16, u16)>,
        quiet: Duration,
        started: &mut bool,
    ) -> Result<Report> {
        match &step.command {
            cli::Command::Start(args) => {
                anyhow::ensure!(!*started, "a script starts one program");
                let mut options = args.start_options(Some(name.to_owned()))?;
                if let Some((cols, rows)) = size {
                    options.cols = cols;
                    options.rows = rows;
                }
                options.quiet = quiet;
                let report = self.start(options).await;
                *started = report.is_ok();
                report
            }
            other => {
                anyhow::ensure!(*started, "the script must begin with `start`");
                cli::execute_in_session(self, other, Some(name)).await
            }
        }
    }

    /// The current screen with title, against the previous observation.
    async fn observe(&self, client: &mut BmuxClient, record: &SessionRecord) -> Result<Observation> {
        let screen = session::screen(client, record).await?;
        let snapshot: bmux_terminal_grid::GridSnapshot =
            serde_json::from_slice(&screen.encoded_grid).context("undecodable screen")?;
        let mut observation =
            Observation::from_snapshot(&snapshot, &record.last_rows, &record.last_row_styles);
        if let Some(signals) = session::signals(client, record).await {
            observation.title = signals.title;
        }
        Ok(observation)
    }

    /// Fill in what the program did since the last command, and save state.
    async fn finish_action(
        &self,
        dir: &Path,
        client: &mut BmuxClient,
        record: &mut SessionRecord,
        report: &mut Report,
        settled: Settled,
    ) -> Result<()> {
        let exited = settled.exited();
        if let Settled::Busy { .. } = settled {
            report.notes.push(
                "the program had not settled; the screen may be mid-update. If it animates \
                 (spinner, clock), use `wait --text` for what you expect instead"
                    .into(),
            );
        }
        report.settled = Some(settled);
        match self.observe(client, record).await {
            Ok(screen) => {
                remember(record, &screen);
                report.screen = Some(screen);
            }
            Err(error) => report.notes.push(format!("screen unavailable: {error:#}")),
        }
        if let Ok((events, offset, _)) =
            session::capture_from(client, record, record.capture_offset).await
        {
            report.output = Some(ActionOutput::from_events(&events));
            record.capture_offset = offset;
        }
        if let Ok(lint) = self.lint_report(client, record, exited).await {
            for finding in lint.findings {
                if finding.severity != Severity::Info
                    && !record.reported_findings.contains(&finding.id)
                {
                    record.reported_findings.push(finding.id.clone());
                    report.new_findings.push(finding);
                }
            }
        }
        session::write_record(dir, record)
    }
}

/// Keep what the screen showed, to report what the next action changes.
fn remember(record: &mut SessionRecord, screen: &Observation) {
    record.last_rows.clone_from(&screen.lines);
    record.last_row_styles.clone_from(&screen.row_styles);
}

/// The size a session started at: replays of its output begin there.
const fn start_size(record: &SessionRecord) -> (u16, u16) {
    if record.start_cols == 0 || record.start_rows == 0 {
        (record.cols, record.rows)
    } else {
        (record.start_cols, record.start_rows)
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct SessionSummary {
    pub name: String,
    pub command: Vec<String>,
    pub cwd: String,
    pub size: String,
    pub age_secs: u64,
}

/// The outcome of replaying a script once.
#[derive(Debug, Clone, Serialize)]
pub struct ScriptRun {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<String>,
    pub passed: bool,
    pub steps: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failed_step: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub final_screen: Option<String>,
    pub findings: Vec<Finding>,
}

impl ScriptRun {
    pub fn render(&self) -> String {
        let mut out = format!(
            "{}{}: {} steps",
            if self.passed { "PASS" } else { "FAIL" },
            self.size
                .as_ref()
                .map(|size| format!(" at {size}"))
                .unwrap_or_default(),
            self.steps
        );
        out.push('\n');
        if let Some(step) = &self.failed_step {
            let _ = writeln!(out, "  failed at {step}");
        }
        if let Some(failure) = &self.failure {
            let _ = writeln!(out, "  {failure}");
        }
        if let Some(screen) = &self.final_screen {
            out.push_str("  screen:\n");
            for line in screen.lines() {
                let _ = writeln!(out, "  |{line}");
            }
        }
        for finding in &self.findings {
            let _ = writeln!(out, "  {}", finding.render().replace('\n', "\n  "));
        }
        out
    }
}

fn check_cell(screen: &Observation, row: u16, col: u16) -> Result<()> {
    if row == 0 || col == 0 || row > screen.rows || col > screen.cols {
        bail!(
            "row {row} col {col} is outside the {}x{} screen (rows and columns start at 1)",
            screen.cols,
            screen.rows
        );
    }
    Ok(())
}

fn parse_button(name: &str) -> Result<keys::MouseButton> {
    Ok(match name {
        "left" | "" => keys::MouseButton::Left,
        "right" => keys::MouseButton::Right,
        "middle" => keys::MouseButton::Middle,
        other => bail!("unknown mouse button '{other}': use left, right or middle"),
    })
}

fn check_expectation(
    expectation: &Expect,
    screen: &Observation,
    status: &bmux_pane_runtime_plugin_api::pane_runtime_execution_v4::TerminalExecutionStatus,
) -> Option<String> {
    let running = settle::is_running(status);
    match expectation {
        Expect::Text(needle) => (!screen.text().contains(needle.as_str()))
            .then(|| format!("{needle:?} is not on the screen")),
        Expect::NoText(needle) => {
            let found = screen.find(needle);
            (!found.is_empty()).then(|| {
                format!(
                    "{needle:?} is on the screen at row {} col {}",
                    found[0].0, found[0].1
                )
            })
        }
        Expect::Cursor { row, col } => ((screen.cursor_row, screen.cursor_col) != (*row, *col))
            .then(|| {
                format!(
                    "cursor is at row {} col {}",
                    screen.cursor_row, screen.cursor_col
                )
            }),
        Expect::Running => (!running).then(|| "the program has exited".to_owned()),
        Expect::Exited(code) => {
            if running {
                Some("the program is still running".to_owned())
            } else if code.is_some() && status.exit_code != *code {
                Some(format!(
                    "exit code was {}",
                    status
                        .exit_code
                        .map_or_else(|| "unknown".to_owned(), |code| code.to_string())
                ))
            } else {
                None
            }
        }
        Expect::Style { text, style } => {
            let matching: Vec<_> = screen
                .spans
                .iter()
                .filter(|span| span.text.contains(text.as_str()))
                .collect();
            let wanted: Vec<&str> = style.split_whitespace().collect();
            if matching.iter().any(|span| {
                let words: Vec<&str> = span.style.split_whitespace().collect();
                wanted.iter().all(|part| words.contains(part))
            })
            {
                None
            } else if matching.is_empty() {
                Some(format!(
                    "no styled text contains {text:?}{}",
                    if screen.text().contains(text.as_str()) {
                        format!(" (it is shown in the base style, {})", screen.base_style)
                    } else {
                        " (it is not on the screen)".to_owned()
                    }
                ))
            } else {
                Some(format!(
                    "{text:?} is styled [{}]",
                    matching
                        .iter()
                        .map(|span| span.style.as_str())
                        .collect::<Vec<_>>()
                        .join("], [")
                ))
            }
        }
    }
}

fn describe_input(input: &Input) -> String {
    match input {
        Input::Type { text, enter } => {
            format!("type {text:?}{}", if *enter { " + Enter" } else { "" })
        }
        Input::Press(keys) => format!("press {}", keys.join(" ")),
        Input::Paste(text) => format!("paste {} chars", text.chars().count()),
        Input::Click {
            row,
            col,
            button,
            double,
        } => format!(
            "{}click {button} at row {row} col {col}",
            if *double { "double-" } else { "" }
        ),
        Input::Scroll { up, count, .. } => {
            format!("scroll {} {count}", if *up { "up" } else { "down" })
        }
        Input::Drag { from, to } => format!(
            "drag from row {} col {} to row {} col {}",
            from.0, from.1, to.0, to.1
        ),
    }
}

fn describe_wait(condition: &WaitFor) -> String {
    match condition {
        WaitFor::Text(pattern) => format!("wait for text /{pattern}/"),
        WaitFor::Gone(pattern) => format!("wait for /{pattern}/ to disappear"),
        WaitFor::Idle => "wait until idle".into(),
        WaitFor::Exit => "wait for exit".into(),
        WaitFor::Change => "wait for the screen to change".into(),
    }
}

fn describe_expect(expectation: &Expect) -> String {
    match expectation {
        Expect::Text(text) => format!("expect text {text:?}"),
        Expect::NoText(text) => format!("expect no text {text:?}"),
        Expect::Cursor { row, col } => format!("expect cursor at row {row} col {col}"),
        Expect::Running => "expect running".into(),
        Expect::Exited(Some(code)) => format!("expect exit code {code}"),
        Expect::Exited(None) => "expect exited".into(),
        Expect::Style { text, style } => format!("expect {text:?} styled [{style}]"),
    }
}

/// Join words for display, quoting those a shell would split.
pub fn shell_join(words: &[String]) -> String {
    words
        .iter()
        .map(|word| script::quote(word))
        .collect::<Vec<_>>()
        .join(" ")
}
