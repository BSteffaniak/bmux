use clap::{Parser, Subcommand, ValueEnum};

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
    #[command(subcommand)]
    pub(crate) command: Option<Command>,

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

#[derive(Debug, Subcommand)]
pub(crate) enum Command {
    /// Keymap tools and diagnostics
    Keymap {
        #[command(subcommand)]
        command: KeymapCommand,
    },
}

#[derive(Debug, Subcommand)]
pub(crate) enum KeymapCommand {
    /// Print compiled keymap and overlap diagnostics
    Doctor {
        /// Print diagnostics as JSON
        #[arg(long)]
        json: bool,
    },
}

#[cfg(test)]
mod tests {
    use super::{Cli, Command, KeymapCommand};
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

    #[test]
    fn parses_keymap_doctor_subcommand() {
        let cli = Cli::try_parse_from(["bmux", "keymap", "doctor"]).expect("valid CLI args");
        let Some(Command::Keymap { command }) = cli.command else {
            panic!("expected keymap subcommand");
        };
        assert!(matches!(command, KeymapCommand::Doctor { json: false }));
    }

    #[test]
    fn parses_keymap_doctor_json_flag() {
        let cli =
            Cli::try_parse_from(["bmux", "keymap", "doctor", "--json"]).expect("valid CLI args");
        let Some(Command::Keymap { command }) = cli.command else {
            panic!("expected keymap subcommand");
        };
        assert!(matches!(command, KeymapCommand::Doctor { json: true }));
    }
}
