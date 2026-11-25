//! The `bmux drive` command line, shared by scripts (a script line is a
//! command without `bmux drive`).

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use clap::{Args, Parser, Subcommand};

use super::{
    DEFAULT_ACTION_TIMEOUT, DEFAULT_QUIET, DEFAULT_START_TIMEOUT, Driver, Expect, Input, Report,
    ScreenDetail, StartOptions, WaitFor,
};

const ABOUT: &str = "Drive a terminal program (TUI or CLI) the way you would test it by hand, with \
feedback after every action";

const LONG_ABOUT: &str = "\
Drive a terminal program (TUI or CLI) the way you would test it by hand, with feedback after \
every action.

`start` runs the program in a private bmux server. Every action (type, press, click, scroll, \
resize) then waits until the program has settled (no output and no CPU use for a moment, or it \
exited), so you never guess with sleep. Each action reports:
  - the screen, with rows numbered from 1 and changed rows marked with *
  - cells styled differently from the rest (selection bars, focus, errors)
  - cursor, alternate screen, mouse mode and window title
  - how much the program wrote to draw the change, and whether it EXITED
  - new terminal protocol problems (modes left on at exit, flicker, bad UTF-8)

Sessions persist between commands. With one session running, -s is not needed. Every action is \
recorded: `script` prints it as a replayable test and `run FILE --size 80x24,40x12` replays it.

Typical loop:
  bmux drive start -- ./target/debug/my-tui
  bmux drive press Down Down Enter
  bmux drive type 'hello' --enter
  bmux drive wait --text 'Saved'
  bmux drive stop                # quits, then lint: did it restore the terminal?

Keys: Enter Esc Tab S-Tab Backspace Space Up Down Left Right Home End PgUp PgDn Insert Delete \
F1-F12, single characters (q Q / :), C-x (ctrl), M-x (alt), S-x (shift), repeats like Down*5.

Run `bmux drive guide` for a fuller guide, or `bmux drive mcp` to serve these tools over MCP.";

#[derive(Debug, Parser)]
#[command(
    name = "bmux drive",
    about = ABOUT,
    long_about = LONG_ABOUT,
    disable_help_subcommand = true
)]
pub struct DriveCli {
    #[command(flatten)]
    pub global: GlobalArgs,
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Clone, Args)]
pub struct GlobalArgs {
    /// Session to act on (needed only when several are running)
    #[arg(short = 's', long = "session", global = true, value_name = "NAME")]
    pub session: Option<String>,
    /// Print the report as JSON
    #[arg(long, global = true)]
    pub json: bool,
    /// Show only the rows that changed, not the whole screen
    #[arg(long, global = true, conflicts_with = "no_screen")]
    pub changes: bool,
    /// Do not print the screen (summary line only)
    #[arg(long, global = true)]
    pub no_screen: bool,
    /// List every styled span instead of the first 24
    #[arg(long, global = true)]
    pub styles: bool,
    /// Milliseconds of no output and no CPU use that count as settled
    #[arg(long, global = true, value_name = "MS")]
    pub settle_ms: Option<u64>,
}

impl GlobalArgs {
    fn detail(&self) -> ScreenDetail {
        if self.no_screen {
            ScreenDetail::None
        } else if self.changes {
            ScreenDetail::Changes
        } else {
            ScreenDetail::Full
        }
    }

    fn quiet(&self) -> Duration {
        self.settle_ms.map_or(DEFAULT_QUIET, Duration::from_millis)
    }
}

#[derive(Debug, Clone, Subcommand)]
pub enum Command {
    /// Start a program in a new session and show its first screen
    #[command(after_help = "Example: bmux drive start --size 100x30 -- htop -d 10")]
    Start(StartArgs),
    /// Type text (newlines are sent as Enter)
    #[command(after_help = "Example: bmux drive type 'git status' --enter")]
    Type {
        text: String,
        /// Press Enter after the text
        #[arg(long)]
        enter: bool,
        #[command(flatten)]
        wait: ActionWait,
    },
    /// Press keys: Enter Esc Tab Up Down PgDn F5 C-c M-x S-Tab q Down*3 ...
    #[command(after_help = "Example: bmux drive press Down Down Enter\n         bmux drive press C-c")]
    Press {
        #[arg(required = true, num_args = 1..)]
        keys: Vec<String>,
        #[command(flatten)]
        wait: ActionWait,
    },
    /// Paste text (bracketed paste when the program enabled it)
    Paste {
        text: String,
        #[command(flatten)]
        wait: ActionWait,
    },
    /// Click a cell (rows and columns start at 1, as numbered in the screen)
    #[command(after_help = "Example: bmux drive click 5 12 --double")]
    Click {
        row: u16,
        col: u16,
        /// left, right or middle
        #[arg(long, default_value = "left")]
        button: String,
        /// Double-click
        #[arg(long)]
        double: bool,
        #[command(flatten)]
        wait: ActionWait,
    },
    /// Scroll the mouse wheel
    #[command(after_help = "Example: bmux drive scroll down 3 --at 10 40")]
    Scroll {
        /// up or down
        direction: String,
        /// Wheel steps
        #[arg(default_value_t = 1)]
        count: u16,
        /// Cell to scroll over (default: the middle of the screen)
        #[arg(long, num_args = 2, value_names = ["ROW", "COL"])]
        at: Option<Vec<u16>>,
        #[command(flatten)]
        wait: ActionWait,
    },
    /// Drag with the left button from one cell to another
    Drag {
        from_row: u16,
        from_col: u16,
        to_row: u16,
        to_col: u16,
        #[command(flatten)]
        wait: ActionWait,
    },
    /// Resize the terminal (the program gets SIGWINCH)
    #[command(after_help = "Example: bmux drive resize 40x12")]
    Resize {
        /// COLSxROWS
        size: String,
        #[command(flatten)]
        wait: ActionWait,
    },
    /// Show the current screen without acting (--history: output that scrolled off)
    #[command(after_help = "Example: bmux drive look --history 300")]
    Look {
        /// Print the last N lines of output instead, including lines that
        /// scrolled off the screen (default 200)
        #[arg(long, value_name = "N", num_args = 0..=1, default_missing_value = "200")]
        history: Option<usize>,
    },
    /// Wait for text to appear or disappear, the program to go idle, or exit
    #[command(after_help = "Example: bmux drive wait --text 'Build (finished|failed)' --timeout 120")]
    Wait {
        /// Regular expression that must appear on screen
        #[arg(long, value_name = "REGEX", group = "condition")]
        text: Option<String>,
        /// Regular expression that must disappear from the screen
        #[arg(long, value_name = "REGEX", group = "condition")]
        gone: Option<String>,
        /// No output and no CPU use for the settle window
        #[arg(long, group = "condition")]
        idle: bool,
        /// The program exits
        #[arg(long, group = "condition")]
        exit: bool,
        /// The screen changes at all
        #[arg(long, group = "condition")]
        change: bool,
        /// Seconds to wait (default 30)
        #[arg(long, default_value_t = 30.0)]
        timeout: f64,
    },
    /// Check the screen now; exits 1 when the check fails
    #[command(after_help = "Example: bmux drive expect --styled 'Save' --style inverse")]
    Expect {
        /// Text that must be on screen
        #[arg(long, group = "check")]
        text: Option<String>,
        /// Text that must not be on screen
        #[arg(long, group = "check")]
        no_text: Option<String>,
        /// Cursor position: ROW COL
        #[arg(long, num_args = 2, value_names = ["ROW", "COL"], group = "check")]
        cursor: Option<Vec<u16>>,
        /// The program is still running
        #[arg(long, group = "check")]
        running: bool,
        /// The program has exited
        #[arg(long, group = "check")]
        exited: bool,
        /// The program exited with this code
        #[arg(long, group = "check", value_name = "CODE")]
        exit_code: Option<u32>,
        /// Text that must be styled as --style says
        #[arg(long, group = "check", requires = "style", value_name = "TEXT")]
        styled: Option<String>,
        /// Style words, as reports print them: inverse, bold, fg=red, bg=#303030
        #[arg(long, requires = "styled")]
        style: Option<String>,
    },
    /// Save a PNG screenshot of the terminal (read it if you can see images)
    Screenshot {
        /// Where to write it (default: a temporary file)
        path: Option<PathBuf>,
    },
    /// Check what the program did to the terminal: modes left on, flicker, bad UTF-8
    Lint,
    /// Stop the program (if running) and the session; reports its exit and lint
    Stop {
        /// Seconds the program gets to exit after being signalled, before it is killed
        #[arg(long, default_value_t = 2.0)]
        grace: f64,
    },
    /// List running sessions
    List,
    /// Print the recorded script of this session (a replayable test)
    Script {
        /// Also write it to this file
        #[arg(long, short)]
        output: Option<PathBuf>,
    },
    /// Replay a script, optionally at several sizes; exits 1 if any run fails
    #[command(after_help = "Example: bmux drive run tests/menu.drive --size 80x24,40x12,200x60")]
    Run {
        file: PathBuf,
        /// Comma-separated sizes to replay at, overriding the script's
        #[arg(long, value_delimiter = ',')]
        size: Vec<String>,
    },
    /// Mirror a session's screen live in your terminal, read-only (for a human)
    Watch,
    /// Serve the drive tools over the Model Context Protocol on stdio
    Mcp,
    /// Print a guide for agents (suitable as a skill or AGENTS.md section)
    Guide,
}

#[derive(Debug, Clone, Args)]
pub struct StartArgs {
    /// Session name (default: the program's name)
    #[arg(long)]
    pub name: Option<String>,
    /// Terminal size as COLSxROWS
    #[arg(long, default_value = "80x24")]
    pub size: String,
    /// Working directory (default: the current directory)
    #[arg(long)]
    pub cwd: Option<PathBuf>,
    /// Extra environment variable KEY=VALUE (repeatable); the rest is inherited
    #[arg(long = "env", value_name = "KEY=VALUE")]
    pub env: Vec<String>,
    /// Seconds to wait for the first screen to settle
    #[arg(long, default_value_t = DEFAULT_START_TIMEOUT.as_secs_f64())]
    pub timeout: f64,
    /// The program and its arguments, after --
    #[arg(last = true, required = true, value_name = "PROGRAM")]
    pub command: Vec<String>,
}

impl StartArgs {
    pub fn start_options(&self, name: Option<String>) -> Result<StartOptions> {
        let (cols, rows) = parse_size(&self.size)?;
        let mut env = BTreeMap::new();
        for entry in &self.env {
            let (key, value) = entry
                .split_once('=')
                .with_context(|| format!("--env {entry}: expected KEY=VALUE"))?;
            anyhow::ensure!(!key.is_empty(), "--env {entry}: empty variable name");
            env.insert(key.to_owned(), value.to_owned());
        }
        Ok(StartOptions {
            name: name.or_else(|| self.name.clone()),
            command: self.command.clone(),
            cwd: self.cwd.clone(),
            env,
            cols,
            rows,
            timeout: seconds(self.timeout)?,
            quiet: DEFAULT_QUIET,
        })
    }
}

#[derive(Debug, Clone, Args)]
pub struct ActionWait {
    /// Seconds to wait for the program to settle (default 5)
    #[arg(long, value_name = "SECS")]
    pub timeout: Option<f64>,
}

impl ActionWait {
    fn timeout(&self) -> Result<Duration> {
        self.timeout.map_or(Ok(DEFAULT_ACTION_TIMEOUT), seconds)
    }
}

fn seconds(value: f64) -> Result<Duration> {
    Duration::try_from_secs_f64(value)
        .ok()
        .filter(|duration| *duration <= Duration::from_secs(24 * 3600))
        .with_context(|| format!("invalid number of seconds: {value}"))
}

pub fn parse_size(size: &str) -> Result<(u16, u16)> {
    let (cols, rows) = size
        .split_once(['x', 'X'])
        .with_context(|| format!("invalid size '{size}': expected COLSxROWS such as 80x24"))?;
    let cols: u16 = cols
        .trim()
        .parse()
        .with_context(|| format!("invalid size '{size}'"))?;
    let rows: u16 = rows
        .trim()
        .parse()
        .with_context(|| format!("invalid size '{size}'"))?;
    anyhow::ensure!(
        (2..=1000).contains(&cols) && (2..=1000).contains(&rows),
        "size {size} must be between 2x2 and 1000x1000"
    );
    Ok((cols, rows))
}

/// Parse one script line (a command without `bmux drive`).
pub fn parse_script_command(words: &[String]) -> Result<Command> {
    #[derive(Parser)]
    #[command(name = "", no_binary_name = true, disable_help_flag = true)]
    struct Line {
        #[command(subcommand)]
        command: Command,
    }
    Line::try_parse_from(words)
        .map(|line| line.command)
        .map_err(|error| anyhow::anyhow!("{}", error.render().to_string().trim()))
}

/// Whether a command can appear in a script.
pub const fn scriptable(command: &Command) -> bool {
    !matches!(
        command,
        Command::List
            | Command::Script { .. }
            | Command::Run { .. }
            | Command::Watch
            | Command::Mcp
            | Command::Guide
            | Command::Stop { .. }
            | Command::Look { .. }
            | Command::Lint
            | Command::Screenshot { .. }
    )
}

/// Run a session command (anything but start/list/run/mcp/guide/watch).
pub async fn execute_in_session(
    driver: &Driver,
    command: &Command,
    session: Option<&str>,
) -> Result<Report> {
    execute_with(driver, command, session, DEFAULT_QUIET).await
}

async fn execute_with(
    driver: &Driver,
    command: &Command,
    session: Option<&str>,
    quiet: Duration,
) -> Result<Report> {
    let input = |input: Input, wait: &ActionWait| -> Result<(Input, Duration)> {
        Ok((input, wait.timeout()?))
    };
    let (input, timeout) = match command {
        Command::Type { text, enter, wait } => input(
            Input::Type {
                text: text.clone(),
                enter: *enter,
            },
            wait,
        )?,
        Command::Press { keys, wait } => input(Input::Press(keys.clone()), wait)?,
        Command::Paste { text, wait } => input(Input::Paste(text.clone()), wait)?,
        Command::Click {
            row,
            col,
            button,
            double,
            wait,
        } => input(
            Input::Click {
                row: *row,
                col: *col,
                button: button.clone(),
                double: *double,
            },
            wait,
        )?,
        Command::Scroll {
            direction,
            count,
            at,
            wait,
        } => {
            let up = match direction.as_str() {
                "up" => true,
                "down" => false,
                other => bail!("scroll direction must be up or down, not '{other}'"),
            };
            let (row, col) = match at.as_deref() {
                Some([row, col]) => (Some(*row), Some(*col)),
                _ => (None, None),
            };
            input(
                Input::Scroll {
                    up,
                    count: *count,
                    row,
                    col,
                },
                wait,
            )?
        }
        Command::Drag {
            from_row,
            from_col,
            to_row,
            to_col,
            wait,
        } => input(
            Input::Drag {
                from: (*from_row, *from_col),
                to: (*to_row, *to_col),
            },
            wait,
        )?,
        Command::Resize { size, wait } => {
            let (cols, rows) = parse_size(size)?;
            return driver
                .resize(session, cols, rows, quiet, wait.timeout()?)
                .await;
        }
        Command::Look { history: None } => return driver.look(session).await,
        Command::Look {
            history: Some(lines),
        } => return driver.history(session, (*lines).max(1)).await,
        Command::Wait {
            text,
            gone,
            idle,
            exit,
            change,
            timeout,
        } => {
            let condition = if let Some(text) = text {
                WaitFor::Text(text.clone())
            } else if let Some(gone) = gone {
                WaitFor::Gone(gone.clone())
            } else if *exit {
                WaitFor::Exit
            } else if *change {
                WaitFor::Change
            } else if *idle {
                WaitFor::Idle
            } else {
                bail!("wait for what? use --text REGEX, --gone REGEX, --idle, --exit or --change")
            };
            return driver
                .wait(session, condition, quiet, seconds(*timeout)?)
                .await;
        }
        Command::Expect {
            text,
            no_text,
            cursor,
            running,
            exited,
            exit_code,
            styled,
            style,
        } => {
            let expectation = if let Some(text) = text {
                Expect::Text(text.clone())
            } else if let Some(text) = no_text {
                Expect::NoText(text.clone())
            } else if let Some([row, col]) = cursor.as_deref() {
                Expect::Cursor {
                    row: *row,
                    col: *col,
                }
            } else if *running {
                Expect::Running
            } else if let Some(code) = exit_code {
                Expect::Exited(Some(*code))
            } else if *exited {
                Expect::Exited(None)
            } else if let (Some(text), Some(style)) = (styled, style) {
                Expect::Style {
                    text: text.clone(),
                    style: style.clone(),
                }
            } else {
                bail!(
                    "expect what? use --text, --no-text, --cursor ROW COL, --running, --exited, \
                     --exit-code N or --styled TEXT --style STYLE"
                )
            };
            return driver.expect(session, expectation).await;
        }
        Command::Screenshot { path } => return driver.screenshot(session, path.clone()).await,
        Command::Lint => return driver.lint(session).await,
        Command::Stop { grace } => return driver.stop(session, seconds(*grace)?).await,
        Command::Script { output } => {
            let report = driver.script(session)?;
            if let (Some(path), Some(script)) = (output, &report.script) {
                std::fs::write(path, script)
                    .with_context(|| format!("failed writing {}", path.display()))?;
            }
            return Ok(report);
        }
        Command::Start(_)
        | Command::List
        | Command::Run { .. }
        | Command::Watch
        | Command::Mcp
        | Command::Guide => bail!("not a session command"),
    };
    driver.input(session, input, quiet, timeout).await
}

/// Entry point for `bmux drive ARGS...`. Returns the process exit code:
/// 0 success, 1 a failed check (`expect`, `wait`, `run`), 2 an error.
pub async fn run(bmux: PathBuf, arguments: Vec<String>) -> Result<u8> {
    let cli = match DriveCli::try_parse_from(
        std::iter::once("bmux drive".to_owned()).chain(arguments),
    ) {
        Ok(cli) => cli,
        Err(error) => {
            let _ = error.print();
            return Ok(if error.use_stderr() { 2 } else { 0 });
        }
    };
    let driver = Driver::new(bmux);
    let global = &cli.global;
    let session = global.session.as_deref();
    let report = match &cli.command {
        Command::Start(args) => {
            let mut options = args.start_options(global.session.clone())?;
            options.quiet = global.quiet();
            driver.start(options).await
        }
        Command::List => {
            let sessions = driver.list();
            if global.json {
                println!("{}", serde_json::to_string_pretty(&sessions)?);
            } else if sessions.is_empty() {
                println!("no drive sessions running");
            } else {
                for session in sessions {
                    println!(
                        "{}  {}  {}  ({}s, in {})",
                        session.name,
                        session.size,
                        super::shell_join(&session.command),
                        session.age_secs,
                        session.cwd
                    );
                }
            }
            return Ok(0);
        }
        Command::Run { file, size } => {
            let source = std::fs::read_to_string(file)
                .with_context(|| format!("failed reading {}", file.display()))?;
            let sizes = size
                .iter()
                .map(|size| parse_size(size))
                .collect::<Result<Vec<_>>>()?;
            let runs = driver.run_script(&source, &sizes, global.quiet()).await?;
            if global.json {
                println!("{}", serde_json::to_string_pretty(&runs)?);
            } else {
                for run in &runs {
                    print!("{}", run.render());
                }
            }
            return Ok(u8::from(runs.iter().any(|run| !run.passed)));
        }
        Command::Watch => return driver.watch(session).await.map(|()| 0),
        Command::Mcp => return super::mcp::serve(driver).await.map(|()| 0),
        Command::Guide => {
            print!("{}", guide());
            return Ok(0);
        }
        other => execute_with(&driver, other, session, global.quiet()).await,
    };
    match report {
        Ok(report) => {
            if global.json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                print!("{}", report.render(global.detail(), global.styles));
            }
            Ok(u8::from(!report.ok))
        }
        Err(error) => {
            if global.json {
                println!(
                    "{}",
                    serde_json::json!({ "ok": false, "error": format!("{error:#}") })
                );
            } else {
                eprintln!("bmux drive: {error:#}");
            }
            Ok(2)
        }
    }
}

/// A guide for agents, suitable as a skill file.
pub const fn guide() -> &'static str {
    include_str!("guide.md")
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory as _;

    #[test]
    fn cli_definition_is_consistent() {
        DriveCli::command().debug_assert();
    }

    #[test]
    fn global_session_flag_goes_anywhere() {
        let cli = DriveCli::try_parse_from(["bmux drive", "press", "Down", "-s", "app"]).unwrap();
        assert_eq!(cli.global.session.as_deref(), Some("app"));
        let cli = DriveCli::try_parse_from(["bmux drive", "-s", "app", "look"]).unwrap();
        assert_eq!(cli.global.session.as_deref(), Some("app"));
    }

    #[test]
    fn start_takes_the_program_after_double_dash() {
        let cli = DriveCli::try_parse_from([
            "bmux drive", "start", "--size", "100x30", "--", "htop", "-d", "10",
        ])
        .unwrap();
        let Command::Start(args) = cli.command else {
            panic!("not start");
        };
        assert_eq!(args.command, vec!["htop", "-d", "10"]);
        let options = args.start_options(None).unwrap();
        assert_eq!((options.cols, options.rows), (100, 30));
    }

    #[test]
    fn keys_that_look_like_flags_are_keys() {
        let cli = DriveCli::try_parse_from(["bmux drive", "press", "-", "Down", "--changes"])
            .unwrap();
        assert!(cli.global.changes);
        let Command::Press { keys, .. } = cli.command else {
            panic!("not press");
        };
        assert_eq!(keys, vec!["-", "Down"]);
    }

    #[test]
    fn sizes_parse_with_clear_errors() {
        assert_eq!(parse_size("80x24").unwrap(), (80, 24));
        assert!(parse_size("80").unwrap_err().to_string().contains("COLSxROWS"));
        assert!(parse_size("1x1").is_err());
    }

    #[test]
    fn the_guide_documents_every_command() {
        let guide = guide();
        for command in DriveCli::command().get_subcommands() {
            let name = command.get_name();
            assert!(
                guide.contains(&format!("drive {name}")),
                "the guide does not mention `drive {name}`"
            );
        }
    }
}
