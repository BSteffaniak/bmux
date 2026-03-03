use clap::{Parser, ValueEnum};

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub(crate) enum DebugRenderLogFormat {
    Text,
    Csv,
}

#[derive(Debug, Parser)]
#[command(author, version, about, long_about = None)]
#[command(name = "bmux")]
#[command(about = "A minimal fullscreen PTY runtime for bmux")]
pub(crate) struct Cli {
    /// Enable verbose logging
    #[arg(short, long)]
    pub(crate) verbose: bool,

    /// Shell binary to launch inside the PTY
    #[arg(long)]
    pub(crate) shell: Option<String>,

    /// Disable alternate screen mode (debug fallback)
    #[arg(long)]
    pub(crate) no_alt_screen: bool,

    /// Show live render diagnostics in status bar
    #[arg(long)]
    pub(crate) debug_render: bool,

    /// Append render diagnostics to a log file
    #[arg(long, value_name = "PATH")]
    pub(crate) debug_render_log: Option<std::path::PathBuf>,

    /// Render diagnostics log format
    #[arg(long, value_enum, default_value_t = DebugRenderLogFormat::Text)]
    pub(crate) debug_render_log_format: DebugRenderLogFormat,
}

#[cfg(test)]
mod tests {
    use super::Cli;
    use clap::Parser;

    #[test]
    fn parses_no_alt_screen_flag() {
        let cli = Cli::try_parse_from(["bmux", "--no-alt-screen"]).expect("valid CLI args");
        assert!(cli.no_alt_screen);
    }

    #[test]
    fn parses_shell_flag() {
        let cli = Cli::try_parse_from(["bmux", "--shell", "/bin/sh"]).expect("valid CLI args");
        assert_eq!(cli.shell.as_deref(), Some("/bin/sh"));
    }

    #[test]
    fn parses_debug_render_flag() {
        let cli = Cli::try_parse_from(["bmux", "--debug-render"]).expect("valid CLI args");
        assert!(cli.debug_render);
    }

    #[test]
    fn parses_debug_render_log_flag() {
        let cli = Cli::try_parse_from(["bmux", "--debug-render-log", "render.log"])
            .expect("valid CLI args");
        assert_eq!(
            cli.debug_render_log.as_deref(),
            Some(std::path::Path::new("render.log"))
        );
    }

    #[test]
    fn parses_debug_render_log_csv_format() {
        let cli = Cli::try_parse_from([
            "bmux",
            "--debug-render-log",
            "render.log",
            "--debug-render-log-format",
            "csv",
        ])
        .expect("valid CLI args");
        assert_eq!(
            cli.debug_render_log_format,
            super::DebugRenderLogFormat::Csv
        );
    }
}
