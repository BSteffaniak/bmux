use std::io::{self, Write};
use std::path::Path;

pub(crate) struct AttachTab {
    pub(crate) index: usize,
    pub(crate) title: String,
    pub(crate) active: bool,
}

pub(crate) fn build_status_line(
    shell_name: &str,
    cwd: &Path,
    cols: u16,
    rows: u16,
    focused_pane: usize,
    debug_suffix: Option<&str>,
) -> String {
    let focused_label = if focused_pane == 0 { "left" } else { "right" };

    let mut status = format!(
        " bmux | shell: {shell_name} | cwd: {} | size: {cols}x{rows} | focus: {focused_label} | Ctrl-A o switch | Ctrl-A [ scroll | Ctrl-A ? help | Ctrl-A q quit ",
        cwd.display()
    );

    if let Some(suffix) = debug_suffix {
        status.push_str(" | ");
        status.push_str(suffix);
    }

    status
}

pub(crate) fn write_status_line(status_line: &str, cols: u16) -> io::Result<()> {
    if cols == 0 {
        return Ok(());
    }

    let width = usize::from(cols);
    let mut rendered = status_line.to_string();

    if rendered.len() > width {
        rendered.truncate(width);
    } else {
        rendered.push_str(&" ".repeat(width - rendered.len()));
    }

    write!(io::stdout(), "\x1b7\x1b[1;1H\x1b[7m{rendered}\x1b[0m\x1b8")?;
    io::stdout().flush()
}

pub(crate) fn build_attach_status_line(
    session_label: &str,
    tabs: &[AttachTab],
    mode_label: &str,
    role_label: &str,
    follow_label: Option<&str>,
    hint: &str,
) -> String {
    let mut status = format!(" bmux [{mode_label}] [{role_label}] | session: {session_label} | ");

    if tabs.is_empty() {
        status.push_str("tabs: (none)");
    } else {
        status.push_str("tabs: ");
        for tab in tabs {
            if tab.active {
                status.push_str(&format!("[{}:{}] ", tab.index, tab.title));
            } else {
                status.push_str(&format!(" {}:{} ", tab.index, tab.title));
            }
        }
    }

    if let Some(follow) = follow_label {
        status.push_str("| ");
        status.push_str(follow);
        status.push(' ');
    }

    status.push_str("| ");
    status.push_str(hint);
    status.push(' ');
    status
}
