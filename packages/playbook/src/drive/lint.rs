//! What a program did to the terminal, read from everything it wrote: the
//! modes it turned on and whether it turned them off again, the queries it
//! sent, how it redrew, and bytes a terminal cannot display.
//!
//! These are the problems screen text never shows: a program that exits
//! leaving mouse reporting on, the alternate screen active or the cursor
//! hidden breaks the user's shell afterwards, and one that clears the whole
//! screen for every frame flickers on real terminals.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use bmux_terminal_grid::{
    GridLimits, SequenceHeader, SequenceObserver, TerminalGrid, TerminalGridStream,
};
use serde::Serialize;

use super::session::CaptureEvent;

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Error,
    Warning,
    Info,
}

impl Severity {
    const fn label(self) -> &'static str {
        match self {
            Self::Error => "error",
            Self::Warning => "warning",
            Self::Info => "info",
        }
    }
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Finding {
    /// Stable identifier, for telling new findings from ones already reported.
    pub id: String,
    pub severity: Severity,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fix: Option<String>,
}

impl Finding {
    pub fn render(&self) -> String {
        let mut line = format!("{}: {}", self.severity.label(), self.message);
        if let Some(fix) = &self.fix {
            let _ = write!(line, "\n    fix: {fix}");
        }
        line
    }
}

/// Counts over the whole output.
#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
pub struct OutputStats {
    pub bytes: u64,
    pub writes: u64,
    pub full_clears: u64,
    pub synchronized_frames: u64,
    pub bells: u64,
    pub titles: u64,
    pub hyperlinks: u64,
    pub clipboard_writes: u64,
    pub notifications: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct LintReport {
    pub findings: Vec<Finding>,
    pub stats: OutputStats,
    /// Terminal queries the program sent, by name, with counts.
    pub queries: BTreeMap<String, u64>,
    /// The capture was too large to read whole; findings cover its start.
    pub truncated: bool,
}

impl LintReport {
    pub fn render(&self) -> String {
        let mut out = String::new();
        if self.findings.is_empty() {
            out.push_str("no terminal protocol problems found\n");
        }
        for finding in &self.findings {
            out.push_str(&finding.render());
            out.push('\n');
        }
        let stats = &self.stats;
        let _ = writeln!(
            out,
            "output: {} bytes in {} writes, {} full-screen clears, {} synchronized frames, {} bells",
            stats.bytes, stats.writes, stats.full_clears, stats.synchronized_frames, stats.bells
        );
        if !self.queries.is_empty() {
            let queries: Vec<String> = self
                .queries
                .iter()
                .map(|(name, count)| format!("{name} x{count}"))
                .collect();
            let _ = writeln!(out, "terminal queries sent: {}", queries.join(", "));
        }
        if self.truncated {
            out.push_str("(output too large to read whole: only its start was checked)\n");
        }
        out
    }
}

/// Lint a program's whole ordered capture. `exited` decides whether modes
/// still on are problems (the terminal is handed back) or just state.
pub fn lint(events: &[CaptureEvent], cols: u16, rows: u16, exited: bool, truncated: bool) -> LintReport {
    let mut stream = TerminalGridStream::new(cols.max(1), rows.max(1), GridLimits::default())
        .expect("drive lint dimensions are valid");
    let mut observer = Observer::default();
    let mut utf8 = Utf8Check::default();
    let mut stats = OutputStats::default();
    for event in events {
        match event {
            CaptureEvent::Output { data, .. } => {
                stats.bytes += data.len() as u64;
                stats.writes += 1;
                utf8.feed(data, stats.bytes - data.len() as u64);
                stream.process_observed(data, &mut observer);
            }
            CaptureEvent::Resize { cols, rows } => {
                let _ = stream.resize((*cols).max(1), (*rows).max(1));
            }
        }
    }
    stats.full_clears = observer.full_clears;
    stats.synchronized_frames = observer.sync_frames;
    stats.bells = observer.bells;
    stats.titles = observer.titles;
    stats.hyperlinks = observer.hyperlinks;
    stats.clipboard_writes = observer.clipboard_writes;
    stats.notifications = observer.notifications;

    let mut findings = Vec::new();
    observer.state_findings(exited, &mut findings);
    utf8.findings(&mut findings);
    if observer.full_resets > 0 {
        findings.push(Finding {
            id: "full-reset".into(),
            severity: Severity::Warning,
            message: format!(
                "sent a full terminal reset (ESC c) {} time(s); it can erase the user's scrollback and settings",
                observer.full_resets
            ),
            fix: Some("restore only the modes you changed instead of resetting the terminal".into()),
        });
    }
    if observer.full_clears >= 4 && observer.sync_frames == 0 {
        findings.push(Finding {
            id: "clear-flicker".into(),
            severity: Severity::Warning,
            message: format!(
                "cleared the whole screen {} times without synchronized updates; real terminals can flicker",
                observer.full_clears
            ),
            fix: Some(
                "overwrite changed cells instead of clearing, or wrap each frame in CSI ?2026h ... CSI ?2026l"
                    .into(),
            ),
        });
    }
    if observer.clipboard_reads > 0 {
        findings.push(Finding {
            id: "clipboard-read".into(),
            severity: Severity::Info,
            message: format!(
                "asked to read the clipboard (OSC 52 ?) {} time(s); most terminals refuse or prompt",
                observer.clipboard_reads
            ),
            fix: None,
        });
    }
    if observer.bells > 0 {
        findings.push(Finding {
            id: "bell".into(),
            severity: Severity::Info,
            message: format!("rang the bell {} time(s)", observer.bells),
            fix: None,
        });
    }
    findings.sort_by(|a, b| a.severity.cmp(&b.severity).then(a.id.cmp(&b.id)));
    LintReport {
        findings,
        stats,
        queries: observer.queries,
        truncated,
    }
}

/// Modes and state a program can leave behind.
#[derive(Default)]
#[allow(clippy::struct_excessive_bools)] // Independent terminal modes.
struct Observer {
    private_modes: BTreeMap<u16, bool>,
    keypad_application: bool,
    kitty_keyboard_depth: i64,
    kitty_keyboard_set: bool,
    sgr_active: bool,
    scroll_region_set: bool,
    sync_open: bool,
    sync_frames: u64,
    full_clears: u64,
    full_resets: u64,
    bells: u64,
    titles: u64,
    hyperlinks: u64,
    clipboard_writes: u64,
    clipboard_reads: u64,
    notifications: u64,
    queries: BTreeMap<String, u64>,
}

impl Observer {
    fn query(&mut self, name: &str) {
        *self.queries.entry(name.to_owned()).or_default() += 1;
    }

    fn mode(&self, mode: u16) -> bool {
        self.private_modes.get(&mode).copied().unwrap_or(mode == 25 || mode == 7)
    }

    fn state_findings(&self, exited: bool, findings: &mut Vec<Finding>) {
        if self.sync_open {
            findings.push(Finding {
                id: "sync-open".into(),
                severity: Severity::Error,
                message: "began a synchronized update (CSI ?2026h) and never ended it; real terminals \
                          freeze the screen until it ends or times out"
                    .into(),
                fix: Some("send CSI ?2026l after every frame".into()),
            });
        }
        if !exited {
            return;
        }
        let mut left_on = Vec::new();
        if [47, 1047, 1049].iter().any(|mode| self.mode(*mode)) {
            left_on.push(("alternate screen", "CSI ?1049l"));
        }
        if [1000, 1002, 1003].iter().any(|mode| self.mode(*mode)) {
            left_on.push(("mouse reporting", "CSI ?1000l ?1002l ?1003l ?1006l"));
        }
        if self.mode(2004) {
            left_on.push(("bracketed paste", "CSI ?2004l"));
        }
        if self.mode(1004) {
            left_on.push(("focus reporting", "CSI ?1004l"));
        }
        if !self.mode(25) {
            left_on.push(("hidden cursor", "CSI ?25h"));
        }
        if self.mode(1) {
            left_on.push(("application cursor keys", "CSI ?1l"));
        }
        if self.keypad_application {
            left_on.push(("application keypad", "ESC >"));
        }
        if self.kitty_keyboard_depth > 0 || self.kitty_keyboard_set {
            left_on.push(("kitty keyboard protocol", "CSI <u"));
        }
        if self.scroll_region_set {
            left_on.push(("a scrolling region", "CSI r"));
        }
        if self.sgr_active {
            left_on.push(("text attributes/colors", "CSI 0m"));
        }
        for (what, fix) in left_on {
            findings.push(Finding {
                id: format!("left-on:{what}"),
                severity: Severity::Error,
                message: format!(
                    "exited with {what} still on; the user's shell inherits it"
                ),
                fix: Some(format!("send {fix} on exit, including on error and signal paths")),
            });
        }
    }

    fn csi_mode(&mut self, set: bool, values: &[u16]) {
        for &mode in values {
            if mode == 2026 {
                if set {
                    self.sync_open = true;
                } else if self.sync_open {
                    self.sync_open = false;
                    self.sync_frames += 1;
                }
            }
            self.private_modes.insert(mode, set);
        }
    }
}

impl SequenceObserver for Observer {
    fn filter_esc(
        &mut self,
        intermediates: &[u8],
        byte: u8,
        _replacement: &mut Vec<u8>,
    ) -> bmux_terminal_grid::FilterVerdict {
        if intermediates.is_empty() {
            match byte {
                b'=' => self.keypad_application = true,
                b'>' => self.keypad_application = false,
                b'c' => self.full_resets += 1,
                _ => {}
            }
        }
        bmux_terminal_grid::FilterVerdict::Keep
    }

    fn csi(&mut self, grid: &TerminalGrid, sequence: &SequenceHeader<'_>) {
        let intermediates = sequence.intermediates();
        let action = sequence.action();
        let values: Vec<u16> = sequence.values().map(|v| v.to_vec()).unwrap_or_default();
        match (intermediates, action) {
            (b"?", 'h') => self.csi_mode(true, &values),
            (b"?", 'l') => self.csi_mode(false, &values),
            (b"", 'm') => {
                // Any non-reset attribute leaves text styled until a reset.
                self.sgr_active = !values.iter().all(|value| *value == 0);
            }
            (b"", 'r') => {
                // A region covering the whole screen is the same as none.
                let top = values.first().copied().unwrap_or(0).max(1);
                let bottom = values.get(1).copied().unwrap_or(0);
                let height = u16::try_from(grid.height()).unwrap_or(u16::MAX);
                self.scroll_region_set = top > 1 || (bottom != 0 && bottom < height);
            }
            (b"", 'J') if values.first().is_some_and(|value| *value == 2 || *value == 3) => {
                self.full_clears += 1;
            }
            (b"", 'c') => self.query("primary device attributes (DA1)"),
            (b">", 'c') => self.query("secondary device attributes (DA2)"),
            (b"=", 'c') => self.query("tertiary device attributes (DA3)"),
            (b"", 'n') if values.first() == Some(&6) => self.query("cursor position (CPR)"),
            (b"", 'n') if values.first() == Some(&5) => self.query("device status (DSR)"),
            (b"?", 'n') => self.query("DEC device status"),
            (b">", 'q') => self.query("terminal version (XTVERSION)"),
            (b"?$", 'p') | (b"$", 'p') => self.query("mode report (DECRQM)"),
            (b"?", 'u') => self.query("kitty keyboard flags"),
            (b">", 'u') => self.kitty_keyboard_depth += 1,
            (b"<", 'u') => {
                let pops = i64::from(values.first().copied().unwrap_or(1).max(1));
                self.kitty_keyboard_depth = (self.kitty_keyboard_depth - pops).max(0);
            }
            (b"=", 'u') => {
                self.kitty_keyboard_set = values.first().is_some_and(|flags| *flags != 0);
            }
            (b"", 't') if matches!(values.first(), Some(14 | 16 | 18 | 19)) => {
                self.query("window size (XTWINOPS)");
            }
            _ => {}
        }
    }

    fn osc(&mut self, _grid: &TerminalGrid, params: &[&[u8]], _bell_terminated: bool) {
        let number = params
            .first()
            .and_then(|number| std::str::from_utf8(number).ok())
            .and_then(|number| number.parse::<u32>().ok());
        let last_is_query = params.last().is_some_and(|value| *value == b"?");
        match number {
            Some(0 | 2) => self.titles += 1,
            Some(8) if params.get(2).is_some_and(|uri| !uri.is_empty()) => self.hyperlinks += 1,
            Some(52) if last_is_query => self.clipboard_reads += 1,
            Some(52) => self.clipboard_writes += 1,
            Some(9 | 777) => self.notifications += 1,
            Some(4) if last_is_query => self.query("palette color (OSC 4)"),
            Some(10) if last_is_query => self.query("foreground color (OSC 10)"),
            Some(11) if last_is_query => self.query("background color (OSC 11)"),
            Some(12) if last_is_query => self.query("cursor color (OSC 12)"),
            _ => {}
        }
    }

    fn wants_dcs(&mut self, introducer: &SequenceHeader<'_>) -> bool {
        match (introducer.intermediates(), introducer.action()) {
            (b"+", 'q') => self.query("terminfo capability (XTGETTCAP)"),
            (b"$", 'q') => self.query("setting (DECRQSS)"),
            _ => {}
        }
        false
    }

    fn bell(&mut self, _grid: &TerminalGrid) {
        self.bells += 1;
    }
}

/// Invalid UTF-8 in output, which terminals show as replacement characters.
#[derive(Default)]
struct Utf8Check {
    pending: Vec<u8>,
    invalid: u64,
    first: Option<(u64, Vec<u8>)>,
}

impl Utf8Check {
    fn feed(&mut self, data: &[u8], offset: u64) {
        let start = offset - self.pending.len() as u64;
        let mut bytes = std::mem::take(&mut self.pending);
        bytes.extend_from_slice(data);
        let mut rest = bytes.as_slice();
        let mut position = start;
        loop {
            match std::str::from_utf8(rest) {
                Ok(_) => break,
                Err(error) => {
                    let valid = error.valid_up_to();
                    match error.error_len() {
                        Some(len) => {
                            self.invalid += 1;
                            if self.first.is_none() {
                                let at = valid;
                                let context_start = at.saturating_sub(12);
                                let context_end = (at + len + 12).min(rest.len());
                                self.first = Some((
                                    position + at as u64,
                                    rest[context_start..context_end].to_vec(),
                                ));
                            }
                            rest = &rest[valid + len..];
                            position += (valid + len) as u64;
                        }
                        None => {
                            // An incomplete character at the end: finish it with the next write.
                            self.pending = rest[valid..].to_vec();
                            break;
                        }
                    }
                }
            }
        }
    }

    fn findings(&self, findings: &mut Vec<Finding>) {
        let Some((offset, context)) = &self.first else {
            return;
        };
        findings.push(Finding {
            id: "invalid-utf8".into(),
            severity: Severity::Warning,
            message: format!(
                "wrote {} invalid UTF-8 sequence(s); terminals show them as \u{fffd}. First at output byte {offset}, near {:?}",
                self.invalid,
                String::from_utf8_lossy(context)
            ),
            fix: Some("write text as UTF-8; look for stray bytes or text encoded as Latin-1".into()),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(bytes: &[u8], exited: bool) -> LintReport {
        lint(
            &[CaptureEvent::Output {
                data: bytes.to_vec(),
            }],
            80,
            24,
            exited,
            false,
        )
    }

    fn ids(report: &LintReport) -> Vec<&str> {
        report.findings.iter().map(|f| f.id.as_str()).collect()
    }

    #[test]
    fn a_clean_tui_exit_has_no_findings() {
        let report = run(
            b"\x1b[?1049h\x1b[?25l\x1b[?1000h\x1b[?1006hhello\x1b[?1000l\x1b[?1006l\x1b[?25h\x1b[?1049l",
            true,
        );
        assert!(report.findings.is_empty(), "{:?}", report.findings);
    }

    #[test]
    fn modes_left_on_at_exit_are_errors() {
        let report = run(b"\x1b[?1049h\x1b[?1002h\x1b[?2004h\x1b[?25l\x1b[31mx", true);
        let ids = ids(&report);
        for id in [
            "left-on:alternate screen",
            "left-on:mouse reporting",
            "left-on:bracketed paste",
            "left-on:hidden cursor",
            "left-on:text attributes/colors",
        ] {
            assert!(ids.contains(&id), "{id} missing from {ids:?}");
        }
        // While running, those modes are state, not problems.
        assert!(run(b"\x1b[?1049h\x1b[?1002h", false).findings.is_empty());
    }

    #[test]
    fn full_screen_scrolling_region_counts_as_reset() {
        assert!(run(b"\x1b[1;24r", true).findings.is_empty());
        assert!(run(b"\x1b[2;10r\x1b[r", true).findings.is_empty());
        assert_eq!(ids(&run(b"\x1b[2;10r", true)), vec!["left-on:a scrolling region"]);
    }

    #[test]
    fn unbalanced_synchronized_update_is_reported_while_running() {
        let report = run(b"\x1b[?2026hframe", false);
        assert_eq!(ids(&report), vec!["sync-open"]);
        let report = run(b"\x1b[?2026hframe\x1b[?2026l", false);
        assert!(report.findings.is_empty());
        assert_eq!(report.stats.synchronized_frames, 1);
    }

    #[test]
    fn repeated_clears_without_sync_flicker() {
        let report = run(&b"\x1b[2Jframe".repeat(5), false);
        assert_eq!(ids(&report), vec!["clear-flicker"]);
        assert_eq!(report.stats.full_clears, 5);
    }

    #[test]
    fn queries_and_invalid_utf8_are_reported() {
        let report = run(b"\x1b[c\x1b[6n\x1b]11;?\x07ok \xff\xfe bad", false);
        assert_eq!(report.queries.len(), 3, "{:?}", report.queries);
        assert!(ids(&report).contains(&"invalid-utf8"));
    }

    #[test]
    fn utf8_split_across_writes_is_valid() {
        let text = "é".as_bytes();
        let report = lint(
            &[
                CaptureEvent::Output {
                    data: text[..1].to_vec(),
                },
                CaptureEvent::Output {
                    data: text[1..].to_vec(),
                },
            ],
            80,
            24,
            false,
            false,
        );
        assert!(report.findings.is_empty(), "{:?}", report.findings);
    }
}
