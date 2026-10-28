//! Bounded startup observation; never claims Live before a frame acknowledgement.
use bmux_broadcast_plugin_api::broadcast_turnkey_v1 as turnkey;
use bmux_keyboard::KeyCode;
use bmux_tui::{
    buffer::Buffer,
    component::{Component, Constraints, LayoutCx},
    composition::{Column, TextBlock},
    event::Event,
    frame::Frame,
    geometry::{Rect, Size},
    inline::InlineTerminal,
    paint::PaintCx,
    style::{Color, Style},
};
use bmux_tui_runtime::{
    PresentReport, Presenter, Program, ResetReason, Runtime, RuntimeConfig, RuntimeError,
    RuntimeEvent, TerminalInput, Update,
};
use std::io::{self, IsTerminal};

struct Progress {
    size: Size,
    text: String,
    outcome: Option<Result<(), String>>,
}
enum Message {
    Status(String),
    Finished(Result<(), String>),
    Input(io::Error),
}
impl Program for Progress {
    type Message = Message;
    type Error = io::Error;
    fn update(&mut self, event: RuntimeEvent<Message>) -> io::Result<Update<Message>> {
        match event {
            RuntimeEvent::Message(Message::Input(error)) => return Err(error),
            RuntimeEvent::Message(Message::Status(text)) => self.text = text,
            RuntimeEvent::Message(Message::Finished(result)) => {
                self.outcome = Some(result);
                return Ok(Update::exit());
            }
            RuntimeEvent::Terminal(Event::Resize(size)) => self.size = size,
            RuntimeEvent::Terminal(Event::Key(key))
                if key.key == KeyCode::Escape
                    || (key.key == KeyCode::Char('c') && key.modifiers.ctrl) =>
            {
                self.outcome = Some(Err("Startup monitor closed; broadcast may continue. Inspect broadcast status or use broadcast stop.".into()));
                return Ok(Update::exit());
            }
            _ => return Ok(Update::none()),
        }
        Ok(Update::redraw())
    }
}
struct InlineProgress {
    terminal: InlineTerminal<io::Stderr>,
}
impl Presenter<Progress> for InlineProgress {
    type Error = io::Error;
    fn reset(&mut self, _: ResetReason) {}
    fn present(&mut self, progress: &mut Progress) -> io::Result<PresentReport> {
        let width = progress.size.width.saturating_sub(1).min(90);
        let height = progress.size.height.saturating_sub(1).min(5);
        let root = Column::new()
            .child(TextBlock::new("Broadcast · Starting").style(Style::new().fg(Color::Cyan)))
            .child(TextBlock::new(progress.text.as_str()))
            .child(TextBlock::new(
                "Esc closes monitor; it does not stop broadcasting",
            ));
        let layout = root.layout(
            Constraints::tight(Size::new(width, height)),
            &mut LayoutCx::default(),
        );
        let mut buffer = Buffer::empty(Rect::new(0, 0, width, height));
        root.paint(&layout, &mut PaintCx::new(&mut Frame::new(&mut buffer)));
        self.terminal.draw(&buffer)?;
        Ok(PresentReport::default())
    }
}

async fn poll(
    client: &mut bmux_client::BmuxClient,
    source: &str,
    handle: Option<bmux_tui_runtime::RuntimeHandle<Message>>,
) -> Result<(), String> {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(15);
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return Err("No first-frame acknowledgement within 15s; use broadcast diagnose --json and broadcast events --last 50 to inspect source progress, transport and ACK failures. Broadcast may continue.".into());
        }
        let status = tokio::time::timeout(remaining, turnkey::client::status(client))
            .await
            .map_err(|_| "Broadcast status timed out".to_owned())?
            .map_err(|_| "Broadcast status unavailable".to_owned())?;
        if status.source_id.as_deref() != Some(source) {
            return Err(
                "Broadcast source changed while starting; not modifying the new publication".into(),
            );
        }
        if let Some(code) = status.error_code {
            return Err(format!(
                "Broadcast has not acknowledged its first frame: {code} (local={}, remote={}). Use broadcast diagnose --json and broadcast events --last 50. This alone does not imply a bad profile or credential.",
                status.local, status.remote
            ));
        }
        if status.local == "running" && status.remote == "live" {
            return Ok(());
        }
        if matches!(status.local.as_str(), "unavailable" | "stopped") {
            return Err(format!(
                "Broadcast is {} / {}; no live frame acknowledged",
                status.local, status.remote
            ));
        }
        if let Some(handle) = &handle {
            let _ = handle
                .send(Message::Status(format!(
                    "Waiting for first frame acknowledgement · {} / {}",
                    status.local, status.remote
                )))
                .await;
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
}

pub(super) async fn wait(
    client: &mut bmux_client::BmuxClient,
    source: &str,
    interactive: bool,
) -> Result<bool, String> {
    if !interactive || !io::stdin().is_terminal() || !io::stderr().is_terminal() {
        // Noninteractive sources may be driven by the caller's own event loop;
        // waiting here would deadlock that producer. Keep accepted != live.
        return Ok(false);
    }
    let (width, height) = crossterm::terminal::size().map_err(|error| error.to_string())?;
    let presenter = InlineProgress {
        terminal: InlineTerminal::enter(io::stderr()).map_err(|error| error.to_string())?,
    };
    let progress = Progress {
        size: Size::new(width, height),
        text: "Waiting for terminal capture and first relay acknowledgement".into(),
        outcome: None,
    };
    let (runtime, handle) = Runtime::new(progress, presenter, RuntimeConfig::default());
    let mut input = TerminalInput::start::<Progress>(handle.clone(), Message::Input);
    let polling = async {
        let result = poll(client, source, Some(handle.clone())).await;
        let _ = handle.send(Message::Finished(result)).await;
        std::future::pending::<()>().await;
    };
    tokio::pin!(polling);
    let result =
        tokio::select! { output = runtime.run() => output, () = &mut polling => unreachable!() };
    input.shutdown().await;
    let mut output = match result {
        Ok(output) => output,
        Err(RuntimeError::Program { error, .. } | RuntimeError::Presenter { error, .. }) => {
            return Err(error.to_string());
        }
    };
    output
        .presenter
        .terminal
        .finish()
        .map_err(|error| error.to_string())?;
    output
        .program
        .outcome
        .unwrap_or_else(|| Err("Startup monitor ended without a result".into()))
        .map(|()| true)
}
