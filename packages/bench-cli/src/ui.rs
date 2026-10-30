//! Mode-owning announcements before/after runs; raw-safe progress between samples.
use bmux_tui::{
    buffer::Buffer,
    geometry::Rect,
    inline::InlineTerminal,
    text::{Line, Span},
};
use bmux_tui_components::theme::ComponentTheme;
use std::io::{self, IsTerminal};

pub(crate) fn enabled(plain: bool, json: bool) -> bool {
    decoration_allowed(
        plain,
        json,
        std::env::var_os("NO_COLOR").is_some(),
        io::stdout().is_terminal(),
        io::stdin().is_terminal(),
    )
}

fn decoration_allowed(
    plain: bool,
    json: bool,
    no_color: bool,
    stdout_tty: bool,
    stdin_tty: bool,
) -> bool {
    !plain && !json && !no_color && stdout_tty && stdin_tty
}

pub(crate) fn announce(message: &str) -> Result<(), String> {
    let (columns, rows) =
        crossterm::terminal::size().map_err(|e| format!("measure inline UI: {e}"))?;
    if columns < 2 || rows < 3 {
        return Ok(());
    }
    let area = Rect::new(0, 0, columns.saturating_sub(1), 1);
    let mut frame = Buffer::empty(area);
    let styles = ComponentTheme::opaque_dark().badge_styles();
    frame.write_line(
        area,
        &Line::from_spans(vec![Span::styled(message, styles.info)]),
    );
    let mut terminal =
        InlineTerminal::enter(io::stdout()).map_err(|e| format!("enter inline UI: {e}"))?;
    let drawn = terminal.draw(&frame);
    let restored = terminal.finish();
    match (drawn, restored) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) => Err(format!("draw inline UI: {error}")),
        (Ok(()), Err(error)) => Err(format!("restore inline UI: {error}")),
        (Err(error), Err(cleanup)) => Err(format!(
            "draw inline UI: {error}; restore inline UI: {cleanup}"
        )),
    }
}

/// No retained cursor geometry: benchmark output moves the cursor arbitrarily.
/// Each event appends one raw-safe row through BMUX's inline encoder. This does
/// not enable raw mode, toggle signal processing, or take ownership from backend.
pub(crate) struct ProgressDisplay {
    enabled: bool,
    categories: std::collections::BTreeMap<String, String>,
    last: String,
    error: Option<String>,
}

impl ProgressDisplay {
    pub(crate) fn new(
        enabled: bool,
        categories: std::collections::BTreeMap<String, String>,
    ) -> Self {
        Self {
            enabled,
            categories,
            last: String::new(),
            error: None,
        }
    }

    pub(crate) fn update(&mut self, phase: &bmux_bench::model::Progress) -> Result<(), String> {
        if let Some(error) = &self.error {
            return Err(error.clone());
        }
        if !self.enabled {
            return Ok(());
        }
        let message = progress_message(phase, &self.categories);
        if message == self.last {
            return Ok(());
        }
        let result = crossterm::terminal::size()
            .and_then(|(columns, rows)| {
                if columns < 2 || rows < 2 {
                    return Ok(());
                }
                write_progress(&mut io::stdout().lock(), &message, columns)
            })
            .map_err(|e| format!("benchmark progress output failed: {e}"));
        if let Err(error) = &result {
            self.error = Some(error.clone());
        }
        self.last = message;
        result
    }

    pub(crate) fn finish(self) -> Result<(), String> {
        self.error.map_or(Ok(()), Err)
    }
}

fn progress_message(
    phase: &bmux_bench::model::Progress,
    categories: &std::collections::BTreeMap<String, String>,
) -> String {
    use bmux_bench::model::Progress;
    let (id, state) = match phase {
        Progress::CaseStarted { id, index, total } => (id, format!("start {}/{total}", index + 1)),
        Progress::Warmup { id } => (id, "warmup".into()),
        Progress::Sampling { id } => (id, "sampling".into()),
        Progress::CaseFinished { id, status } => (id, format!("{status:?}")),
    };
    let category = categories.get(id).map_or("unknown", String::as_str);
    format!(
        "BMUX bench · {} · {} · {state}",
        super::safe(category),
        super::safe(id)
    )
}

fn write_progress(writer: &mut impl io::Write, message: &str, columns: u16) -> io::Result<()> {
    let area = Rect::new(0, 0, columns.saturating_sub(1), 1);
    let mut frame = Buffer::empty(area);
    let style = ComponentTheme::opaque_dark().badge_styles().info;
    frame.write_line(area, &Line::from_spans(vec![Span::styled(message, style)]));
    // Explicit CRLF in this encoder is required while backend owns raw termios.
    bmux_tui::ansi::write_ansi_inline_frame(writer, &frame)?;
    writer.flush()
}

#[cfg(test)]
mod tests {
    use super::{ProgressDisplay, decoration_allowed, progress_message, write_progress};
    use bmux_bench::model::{Progress, Status};
    use std::{collections::BTreeMap, io};

    #[test]
    fn progress_contains_category_phase_and_status_and_raw_safe_line_breaks() {
        let categories = BTreeMap::from([("case".into(), "unicode".into())]);
        for (event, expected) in [
            (
                Progress::CaseStarted {
                    id: "case".into(),
                    index: 0,
                    total: 2,
                },
                "start 1/2",
            ),
            (Progress::Warmup { id: "case".into() }, "warmup"),
            (Progress::Sampling { id: "case".into() }, "sampling"),
            (
                Progress::CaseFinished {
                    id: "case".into(),
                    status: Status::Complete,
                },
                "Complete",
            ),
        ] {
            let message = progress_message(&event, &categories);
            assert!(message.contains("unicode"));
            assert!(message.contains(expected));
            let mut bytes = Vec::new();
            write_progress(&mut bytes, &message, 100).unwrap();
            assert!(bytes.windows(2).any(|pair| pair == b"\r\n"));
            for (index, byte) in bytes.iter().enumerate() {
                if *byte == b'\n' {
                    assert!(index > 0 && bytes[index - 1] == b'\r');
                }
            }
            assert!(!String::from_utf8(bytes).unwrap().contains("?1049"));
        }
    }

    struct Broken;
    impl io::Write for Broken {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Err(io::Error::other("broken output"))
        }
        fn flush(&mut self) -> io::Result<()> {
            Err(io::Error::other("broken flush"))
        }
    }

    #[test]
    fn progress_propagates_output_errors_and_disabled_display_has_no_io() {
        assert!(write_progress(&mut Broken, "sampling", 80).is_err());
        let mut display = ProgressDisplay::new(false, BTreeMap::new());
        display
            .update(&Progress::Sampling { id: "case".into() })
            .unwrap();
        assert!(display.finish().is_ok());
        let mut failed = ProgressDisplay::new(true, BTreeMap::new());
        failed.error = Some("injected failure".into());
        assert!(
            failed
                .update(&Progress::Warmup { id: "case".into() })
                .is_err()
        );
        assert!(failed.finish().is_err());
    }

    #[test]
    fn decorations_require_explicitly_safe_terminal_output() {
        assert!(decoration_allowed(false, false, false, true, true));
        assert!(!decoration_allowed(true, false, false, true, true));
        assert!(!decoration_allowed(false, true, false, true, true));
        assert!(!decoration_allowed(false, false, true, true, true));
        assert!(!decoration_allowed(false, false, false, false, true));
        assert!(!decoration_allowed(false, false, false, true, false));
    }
}
