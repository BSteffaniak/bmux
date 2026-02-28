#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]
#![allow(clippy::cargo_common_metadata)]

//! Main CLI application for bmux terminal multiplexer

use anyhow::{Context, Result};
use clap::Parser;
use crossterm::{
    event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    execute,
    terminal::{self, EnterAlternateScreen, LeaveAlternateScreen},
};
use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use std::io::{self, Read, Write};
use std::process::ExitCode;
use std::time::Duration;
use tracing::{debug, info};

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
        execute!(io::stdout(), EnterAlternateScreen).context("failed to enter alternate screen")?;
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
    info!("Starting bmux fullscreen runtime");
    info!("Launching shell: {shell}");

    run_fullscreen_pty(&shell)
}

fn run_fullscreen_pty(shell: &str) -> Result<u8> {
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

    let _guard = TerminalGuard::activate()?;

    let output_thread = std::thread::Builder::new()
        .name("bmux-pty-output".to_string())
        .spawn(move || -> Result<()> {
            let mut buffer = [0_u8; 8192];
            loop {
                let bytes_read = pty_reader
                    .read(&mut buffer)
                    .context("failed reading from PTY")?;
                if bytes_read == 0 {
                    break;
                }
                io::stdout()
                    .write_all(&buffer[..bytes_read])
                    .context("failed writing PTY output")?;
                io::stdout().flush().context("failed flushing PTY output")?;
            }
            Ok(())
        })
        .context("failed to spawn PTY output thread")?;

    let mut exit_code = 0_u8;

    loop {
        if let Some(status) = child.try_wait().context("failed to poll shell status")? {
            exit_code = exit_code_from_u32(status.exit_code());
            break;
        }

        if !event::poll(Duration::from_millis(16)).context("failed to poll terminal events")? {
            continue;
        }

        match event::read().context("failed to read terminal event")? {
            Event::Key(key_event) => {
                if matches!(key_event.kind, KeyEventKind::Release) {
                    continue;
                }

                let bytes = encode_key_event(key_event);
                if bytes.is_empty() {
                    continue;
                }

                if pty_writer
                    .write_all(&bytes)
                    .and_then(|_| pty_writer.flush())
                    .is_err()
                {
                    break;
                }
            }
            Event::Resize(new_cols, new_rows) => {
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
            }
            _ => {}
        }
    }

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

fn encode_key_event(key_event: KeyEvent) -> Vec<u8> {
    let mut bytes = Vec::new();

    let mut push_escaped = |sequence: &[u8]| {
        if key_event.modifiers.contains(KeyModifiers::ALT) {
            bytes.push(0x1b);
        }
        bytes.extend_from_slice(sequence);
    };

    match key_event.code {
        KeyCode::Char(character) => {
            if key_event.modifiers.contains(KeyModifiers::CONTROL) {
                if let Some(control_byte) = control_character(character) {
                    push_escaped(&[control_byte]);
                    return bytes;
                }
            }

            let mut utf8 = [0_u8; 4];
            let encoded = character.encode_utf8(&mut utf8);
            push_escaped(encoded.as_bytes());
        }
        KeyCode::Enter => push_escaped(b"\r"),
        KeyCode::Tab => push_escaped(b"\t"),
        KeyCode::BackTab => push_escaped(b"\x1b[Z"),
        KeyCode::Backspace => push_escaped(&[0x7f]),
        KeyCode::Delete => push_escaped(b"\x1b[3~"),
        KeyCode::Insert => push_escaped(b"\x1b[2~"),
        KeyCode::Esc => push_escaped(&[0x1b]),
        KeyCode::Up => push_escaped(b"\x1b[A"),
        KeyCode::Down => push_escaped(b"\x1b[B"),
        KeyCode::Right => push_escaped(b"\x1b[C"),
        KeyCode::Left => push_escaped(b"\x1b[D"),
        KeyCode::Home => push_escaped(b"\x1b[H"),
        KeyCode::End => push_escaped(b"\x1b[F"),
        KeyCode::PageUp => push_escaped(b"\x1b[5~"),
        KeyCode::PageDown => push_escaped(b"\x1b[6~"),
        KeyCode::F(function_number) => {
            if let Some(sequence) = function_key_sequence(function_number) {
                push_escaped(sequence);
            }
        }
        _ => {}
    }

    bytes
}

fn control_character(character: char) -> Option<u8> {
    if character.is_ascii_alphabetic() {
        let lower = character.to_ascii_lowercase();
        return Some((lower as u8) & 0x1f);
    }

    match character {
        ' ' => Some(0),
        '[' => Some(27),
        '\\' => Some(28),
        ']' => Some(29),
        '^' => Some(30),
        '_' => Some(31),
        _ => None,
    }
}

fn function_key_sequence(function_number: u8) -> Option<&'static [u8]> {
    match function_number {
        1 => Some(b"\x1bOP"),
        2 => Some(b"\x1bOQ"),
        3 => Some(b"\x1bOR"),
        4 => Some(b"\x1bOS"),
        5 => Some(b"\x1b[15~"),
        6 => Some(b"\x1b[17~"),
        7 => Some(b"\x1b[18~"),
        8 => Some(b"\x1b[19~"),
        9 => Some(b"\x1b[20~"),
        10 => Some(b"\x1b[21~"),
        11 => Some(b"\x1b[23~"),
        12 => Some(b"\x1b[24~"),
        _ => None,
    }
}
