use std::io::{self, Write};
use std::path::Path;

pub(crate) fn build_status_line(
    shell_name: &str,
    cwd: &Path,
    cols: u16,
    rows: u16,
    focused_pane: usize,
) -> String {
    let focused_label = if focused_pane == 0 { "left" } else { "right" };

    format!(
        " bmux | shell: {shell_name} | cwd: {} | size: {cols}x{rows} | focus: {focused_label} | Ctrl-A o switch | Ctrl-A +/- resize | Ctrl-A q quit ",
        cwd.display()
    )
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
