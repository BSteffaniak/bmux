use crate::cli::Cli;
use crate::pty::{STARTUP_ALT_SCREEN_GUARD_DURATION, extract_filtered_output};
use crate::status::{build_status_line, write_status_line};
use crate::terminal::TerminalGuard;
use anyhow::{Context, Result};
use clap::Parser;
use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::thread;
use std::time::{Duration, Instant};
use tracing::debug;

const STATUS_REDRAW_INTERVAL: Duration = Duration::from_millis(250);
const EXIT_KEY_PREFIX: u8 = 0x01;

pub(crate) fn run() -> Result<u8> {
    let cli = Cli::parse();
    init_logging(cli.verbose);

    let shell = resolve_shell(cli.shell);
    debug!("Starting bmux runtime");
    debug!("Launching shell: {shell}");

    run_fullscreen_pty(&shell, !cli.no_alt_screen)
}

fn run_fullscreen_pty(shell: &str, use_alt_screen: bool) -> Result<u8> {
    let terminal_guard = TerminalGuard::activate(use_alt_screen, true)?;

    let pty_system = native_pty_system();
    let (cols, rows) = crossterm::terminal::size().context("failed to read terminal size")?;
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
    let shutdown_requested = Arc::new(AtomicBool::new(false));
    let stdout_lock = Arc::new(Mutex::new(()));

    let input_seen_for_thread = Arc::clone(&user_input_seen);
    let shutdown_for_input_thread = Arc::clone(&shutdown_requested);
    let input_thread = thread::Builder::new()
        .name("bmux-pty-input".to_string())
        .spawn(move || -> Result<()> {
            let mut stdin = io::stdin().lock();
            let mut buffer = [0_u8; 8192];
            let mut prefix_pending = false;

            loop {
                let bytes_read = stdin
                    .read(&mut buffer)
                    .context("failed reading terminal input")?;
                if bytes_read == 0 {
                    if prefix_pending {
                        let _ = pty_writer.write_all(&[EXIT_KEY_PREFIX]);
                        let _ = pty_writer.flush();
                    }
                    break;
                }

                input_seen_for_thread.store(true, Ordering::Relaxed);

                let mut forwarded = Vec::with_capacity(bytes_read + 1);
                for byte in &buffer[..bytes_read] {
                    if prefix_pending {
                        prefix_pending = false;
                        if *byte == b'q' || *byte == b'Q' {
                            shutdown_for_input_thread.store(true, Ordering::Relaxed);
                            continue;
                        }

                        forwarded.push(EXIT_KEY_PREFIX);
                        forwarded.push(*byte);
                        continue;
                    }

                    if *byte == EXIT_KEY_PREFIX {
                        prefix_pending = true;
                        continue;
                    }

                    forwarded.push(*byte);
                }

                if forwarded.is_empty() {
                    continue;
                }

                if pty_writer
                    .write_all(&forwarded)
                    .and_then(|_| pty_writer.flush())
                    .is_err()
                {
                    break;
                }
            }

            Ok(())
        })
        .context("failed to spawn PTY input thread")?;

    let user_input_for_output_thread = Arc::clone(&user_input_seen);
    let stdout_lock_for_output_thread = Arc::clone(&stdout_lock);
    let output_thread = thread::Builder::new()
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
                let startup_guard_active = !user_input_for_output_thread.load(Ordering::Relaxed)
                    && Instant::now() < startup_deadline;

                let (output, dropped_exit_sequence) =
                    extract_filtered_output(&mut pending, startup_guard_active);

                if dropped_exit_sequence {
                    debug!("Dropped startup alt-screen exit sequence from shell output");
                }

                if output.is_empty() {
                    continue;
                }

                let _lock = stdout_lock_for_output_thread
                    .lock()
                    .expect("stdout mutex poisoned");
                io::stdout()
                    .write_all(&output)
                    .context("failed writing PTY output")?;
                io::stdout().flush().context("failed flushing PTY output")?;
            }

            if !pending.is_empty() {
                let _lock = stdout_lock_for_output_thread
                    .lock()
                    .expect("stdout mutex poisoned");
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

    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("?"));
    let shell_name = shell_name(shell);
    draw_status(shell_name, &cwd, cols, rows, &stdout_lock)?;

    let mut last_size = (cols, rows);
    let mut next_status_draw = Instant::now() + STATUS_REDRAW_INTERVAL;
    let mut kill_sent = false;

    let exit_code = loop {
        if shutdown_requested.load(Ordering::Relaxed) && !kill_sent {
            debug!("Received Ctrl-A q, terminating shell");
            let _ = child.kill();
            kill_sent = true;
        }

        if let Some(status) = child.try_wait().context("failed to poll shell status")? {
            break exit_code_from_u32(status.exit_code());
        }

        let (new_cols, new_rows) =
            crossterm::terminal::size().context("failed to read terminal size")?;
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
            terminal_guard.refresh_layout(new_rows)?;
            draw_status(shell_name, &cwd, new_cols, new_rows, &stdout_lock)?;
            last_size = (new_cols, new_rows);
            next_status_draw = Instant::now() + STATUS_REDRAW_INTERVAL;
        }

        if Instant::now() >= next_status_draw {
            draw_status(shell_name, &cwd, last_size.0, last_size.1, &stdout_lock)?;
            next_status_draw = Instant::now() + STATUS_REDRAW_INTERVAL;
        }

        thread::sleep(Duration::from_millis(16));
    };

    child.wait().context("failed waiting for shell exit")?;

    match input_thread.join() {
        Ok(result) => result.context("PTY input thread failed")?,
        Err(_) => return Err(anyhow::anyhow!("PTY input thread panicked")),
    }

    match output_thread.join() {
        Ok(result) => result.context("PTY output thread failed")?,
        Err(_) => return Err(anyhow::anyhow!("PTY output thread panicked")),
    }

    Ok(exit_code)
}

fn draw_status(
    shell_name: &str,
    cwd: &Path,
    cols: u16,
    rows: u16,
    stdout_lock: &Arc<Mutex<()>>,
) -> Result<()> {
    let status_line = build_status_line(shell_name, cwd, cols, rows);
    let _lock = stdout_lock.lock().expect("stdout mutex poisoned");
    write_status_line(&status_line, cols).context("failed drawing status line")
}

fn init_logging(verbose: bool) {
    #[cfg(feature = "logging")]
    {
        let level = if verbose {
            tracing::Level::DEBUG
        } else {
            tracing::Level::WARN
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

fn shell_name(shell: &str) -> &str {
    Path::new(shell)
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or(shell)
}

fn exit_code_from_u32(code: u32) -> u8 {
    match u8::try_from(code) {
        Ok(valid_code) => valid_code,
        Err(_) => u8::MAX,
    }
}
