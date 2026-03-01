#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]
#![allow(clippy::cargo_common_metadata)]

//! Main CLI application for bmux terminal multiplexer

use anyhow::{Context, Result};
use clap::Parser;
use crossterm::{
    cursor, execute,
    terminal::{self, Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen},
};
use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use std::io::{self, Read, Write};
use std::process::ExitCode;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::Duration;
use std::time::Instant;
use tracing::debug;

const ALT_SCREEN_EXIT_SEQUENCE: &[u8] = b"\x1b[?1049l";
const STARTUP_ALT_SCREEN_GUARD_DURATION: Duration = Duration::from_secs(3);

#[derive(Parser)]
#[command(author, version, about, long_about = None)]
#[command(name = "bmux")]
#[command(about = "A minimal fullscreen PTY runtime for bmux")]
struct Cli {
    /// Enable verbose logging
    #[arg(short, long)]
    verbose: bool,

    /// Shell binary to launch inside the PTY
    #[arg(long)]
    shell: Option<String>,
}

struct TerminalGuard;

impl TerminalGuard {
    fn activate() -> Result<Self> {
        terminal::enable_raw_mode().context("failed to enable terminal raw mode")?;
        execute!(
            io::stdout(),
            EnterAlternateScreen,
            Clear(ClearType::All),
            cursor::MoveTo(0, 0)
        )
        .context("failed to enter alternate screen")?;
        Ok(Self)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
        let _ = terminal::disable_raw_mode();
    }
}

fn main() -> ExitCode {
    match run() {
        Ok(code) => ExitCode::from(code),
        Err(error) => {
            eprintln!("bmux error: {error:#}");
            ExitCode::from(1)
        }
    }
}

fn run() -> Result<u8> {
    let cli = Cli::parse();
    init_logging(cli.verbose);

    let shell = resolve_shell(cli.shell);
    debug!("Starting bmux fullscreen runtime");
    debug!("Launching shell: {shell}");

    run_fullscreen_pty(&shell)
}

fn run_fullscreen_pty(shell: &str) -> Result<u8> {
    let _guard = TerminalGuard::activate()?;

    let pty_system = native_pty_system();
    let (cols, rows) = terminal::size().context("failed to read terminal size")?;
    let pty_pair = pty_system
        .openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .context("failed to open PTY")?;

    let command = CommandBuilder::new(shell);
    let mut child = pty_pair
        .slave
        .spawn_command(command)
        .context("failed to spawn shell in PTY")?;
    drop(pty_pair.slave);

    let mut pty_reader = pty_pair
        .master
        .try_clone_reader()
        .context("failed to clone PTY reader")?;
    let mut pty_writer = pty_pair
        .master
        .take_writer()
        .context("failed to open PTY writer")?;

    let startup_deadline = Instant::now() + STARTUP_ALT_SCREEN_GUARD_DURATION;
    let user_input_seen = Arc::new(AtomicBool::new(false));

    let input_seen_for_thread = Arc::clone(&user_input_seen);
    let _input_thread = std::thread::Builder::new()
        .name("bmux-pty-input".to_string())
        .spawn(move || -> Result<()> {
            let mut stdin = io::stdin().lock();
            let mut buffer = [0_u8; 8192];

            loop {
                let bytes_read = stdin
                    .read(&mut buffer)
                    .context("failed reading terminal input")?;
                if bytes_read == 0 {
                    break;
                }

                input_seen_for_thread.store(true, Ordering::Relaxed);

                if pty_writer
                    .write_all(&buffer[..bytes_read])
                    .and_then(|_| pty_writer.flush())
                    .is_err()
                {
                    break;
                }
            }

            Ok(())
        })
        .context("failed to spawn PTY input thread")?;

    let output_thread = std::thread::Builder::new()
        .name("bmux-pty-output".to_string())
        .spawn(move || -> Result<()> {
            let mut buffer = [0_u8; 8192];
            let mut pending = Vec::new();

            loop {
                let bytes_read = pty_reader
                    .read(&mut buffer)
                    .context("failed reading from PTY")?;
                if bytes_read == 0 {
                    break;
                }

                pending.extend_from_slice(&buffer[..bytes_read]);

                let startup_guard_active =
                    !user_input_seen.load(Ordering::Relaxed) && Instant::now() < startup_deadline;

                let (output, dropped_exit_sequence) =
                    extract_filtered_output(&mut pending, startup_guard_active);

                if dropped_exit_sequence {
                    debug!("Dropped startup alt-screen exit sequence from shell output");
                }

                if output.is_empty() {
                    continue;
                }

                io::stdout()
                    .write_all(&output)
                    .context("failed writing PTY output")?;
                io::stdout().flush().context("failed flushing PTY output")?;
            }

            if !pending.is_empty() {
                io::stdout()
                    .write_all(&pending)
                    .context("failed writing buffered PTY output")?;
                io::stdout()
                    .flush()
                    .context("failed flushing buffered PTY output")?;
            }

            Ok(())
        })
        .context("failed to spawn PTY output thread")?;

    let mut last_size = (cols, rows);

    let exit_code = loop {
        if let Some(status) = child.try_wait().context("failed to poll shell status")? {
            break exit_code_from_u32(status.exit_code());
        }

        let (new_cols, new_rows) = terminal::size().context("failed to read terminal size")?;
        if (new_cols, new_rows) != last_size {
            debug!("Terminal resized to {new_cols}x{new_rows}");
            pty_pair
                .master
                .resize(PtySize {
                    rows: new_rows,
                    cols: new_cols,
                    pixel_width: 0,
                    pixel_height: 0,
                })
                .context("failed to resize PTY")?;
            last_size = (new_cols, new_rows);
        }

        std::thread::sleep(Duration::from_millis(16));
    };

    child.wait().context("failed waiting for shell exit")?;

    match output_thread.join() {
        Ok(result) => result.context("PTY output thread failed")?,
        Err(_) => return Err(anyhow::anyhow!("PTY output thread panicked")),
    }

    Ok(exit_code)
}

fn init_logging(verbose: bool) {
    #[cfg(feature = "logging")]
    {
        let level = if verbose {
            tracing::Level::DEBUG
        } else {
            tracing::Level::INFO
        };

        let _ = tracing_subscriber::fmt()
            .with_max_level(level)
            .with_target(false)
            .try_init();
    }

    #[cfg(not(feature = "logging"))]
    {
        let _ = verbose;
    }
}

fn resolve_shell(cli_shell: Option<String>) -> String {
    if let Some(shell) = cli_shell {
        return shell;
    }

    if let Some(shell) = std::env::var_os("SHELL") {
        return shell.to_string_lossy().into_owned();
    }

    if cfg!(windows) {
        "cmd.exe".to_string()
    } else {
        "/bin/sh".to_string()
    }
}

fn exit_code_from_u32(code: u32) -> u8 {
    match u8::try_from(code) {
        Ok(valid_code) => valid_code,
        Err(_) => u8::MAX,
    }
}

fn extract_filtered_output(pending: &mut Vec<u8>, startup_guard_active: bool) -> (Vec<u8>, bool) {
    if !startup_guard_active {
        if pending.is_empty() {
            return (Vec::new(), false);
        }

        let output = std::mem::take(pending);
        return (output, false);
    }

    let mut output = Vec::new();
    let mut offset = 0;
    let mut dropped_exit_sequence = false;

    while offset < pending.len() {
        if pending[offset..].starts_with(ALT_SCREEN_EXIT_SEQUENCE) {
            dropped_exit_sequence = true;
            offset += ALT_SCREEN_EXIT_SEQUENCE.len();
            continue;
        }

        if pending[offset] == ALT_SCREEN_EXIT_SEQUENCE[0]
            && is_prefix_of_alt_screen_exit_sequence(&pending[offset..])
            && pending.len() - offset < ALT_SCREEN_EXIT_SEQUENCE.len()
        {
            break;
        }

        output.push(pending[offset]);
        offset += 1;
    }

    if offset > 0 {
        pending.drain(0..offset);
    }

    (output, dropped_exit_sequence)
}

fn is_prefix_of_alt_screen_exit_sequence(bytes: &[u8]) -> bool {
    ALT_SCREEN_EXIT_SEQUENCE.starts_with(bytes)
}
