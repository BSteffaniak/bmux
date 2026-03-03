use super::{PaneProcess, PaneRuntime, PaneState, MIN_PANE_COLS, MIN_PANE_ROWS};
use crate::pane::{Layout, Rect};
use crate::pty::extract_filtered_output;
use anyhow::{Context, Result};
use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use std::io::Read;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Instant;
use tracing::debug;
use vt100::Parser as VtParser;

pub(super) fn spawn_pane(
    shell: &str,
    title: String,
    pane_inner: Rect,
    startup_deadline: Instant,
    user_input_seen: Arc<AtomicBool>,
) -> Result<PaneRuntime> {
    let state = Arc::new(PaneState {
        parser: Mutex::new(VtParser::new(
            pane_inner.height.max(MIN_PANE_ROWS),
            pane_inner.width.max(MIN_PANE_COLS),
            10_000,
        )),
        dirty: AtomicBool::new(true),
    });

    Ok(PaneRuntime {
        title: title.clone(),
        shell: shell.to_string(),
        process: Some(spawn_pane_process(
            shell,
            title,
            pane_inner,
            startup_deadline,
            user_input_seen,
            Arc::clone(&state),
        )?),
        state,
        closed: false,
        exit_code: None,
    })
}

pub(super) fn spawn_pane_process(
    shell: &str,
    title: String,
    pane_inner: Rect,
    startup_deadline: Instant,
    user_input_seen: Arc<AtomicBool>,
    state: Arc<PaneState>,
) -> Result<PaneProcess> {
    let pty_system = native_pty_system();
    let pty_pair = pty_system
        .openpty(PtySize {
            rows: pane_inner.height.max(MIN_PANE_ROWS),
            cols: pane_inner.width.max(MIN_PANE_COLS),
            pixel_width: 0,
            pixel_height: 0,
        })
        .context("failed to open pane PTY")?;

    let command = CommandBuilder::new(shell);
    let child = pty_pair
        .slave
        .spawn_command(command)
        .context("failed to spawn shell in pane")?;
    drop(pty_pair.slave);

    {
        let mut parser = state.parser.lock().expect("pane parser mutex poisoned");
        parser.screen_mut().set_size(
            pane_inner.height.max(MIN_PANE_ROWS),
            pane_inner.width.max(MIN_PANE_COLS),
        );
    }
    state.dirty.store(true, Ordering::Relaxed);

    let mut reader = pty_pair
        .master
        .try_clone_reader()
        .context("failed to clone pane PTY reader")?;
    let writer = pty_pair
        .master
        .take_writer()
        .context("failed to open pane PTY writer")?;

    let state_for_thread = Arc::clone(&state);
    let output_thread = thread::Builder::new()
        .name(format!("bmux-pane-output-{title}"))
        .spawn(move || -> Result<()> {
            let mut buffer = [0_u8; 8192];
            let mut pending = Vec::new();

            loop {
                let bytes_read = reader
                    .read(&mut buffer)
                    .context("failed reading pane PTY output")?;
                if bytes_read == 0 {
                    break;
                }

                pending.extend_from_slice(&buffer[..bytes_read]);
                let startup_guard_active =
                    !user_input_seen.load(Ordering::Relaxed) && Instant::now() < startup_deadline;

                let (output, dropped_exit_sequence) =
                    extract_filtered_output(&mut pending, startup_guard_active);

                if dropped_exit_sequence {
                    debug!("Dropped startup alt-screen exit sequence from pane output");
                }

                if output.is_empty() {
                    continue;
                }

                let mut parser = state_for_thread
                    .parser
                    .lock()
                    .expect("pane parser mutex poisoned");
                parser.process(&output);
                state_for_thread.dirty.store(true, Ordering::Relaxed);
            }

            Ok(())
        })
        .context("failed to spawn pane output thread")?;

    Ok(PaneProcess {
        master: pty_pair.master,
        writer,
        child,
        output_thread: Some(output_thread),
    })
}

pub(super) fn refresh_exit_codes(panes: &mut [PaneRuntime]) -> Result<()> {
    for pane in panes.iter_mut() {
        let Some(process) = pane.process.as_mut() else {
            continue;
        };

        if let Some(status) = process
            .child
            .try_wait()
            .context("failed to poll pane shell status")?
        {
            pane.exit_code = Some(super::exit_code_from_u32(status.exit_code()));
            stop_pane_process(pane, false)?;
            pane.closed = false;
            pane.state.dirty.store(true, Ordering::Relaxed);
        }
    }

    Ok(())
}

pub(super) fn stop_pane_process(pane: &mut PaneRuntime, kill: bool) -> Result<()> {
    if let Some(mut process) = pane.process.take() {
        if kill {
            let _ = process.child.kill();
        }

        let _ = process.child.wait();

        if let Some(output_thread) = process.output_thread.take() {
            match output_thread.join() {
                Ok(result) => result.context("PTY output thread failed")?,
                Err(_) => return Err(anyhow::anyhow!("PTY output thread panicked")),
            }
        }
    }

    Ok(())
}

pub(super) fn pane_is_running(pane: &PaneRuntime) -> bool {
    pane.process.is_some()
}

pub(super) fn any_running_panes(panes: &[PaneRuntime]) -> bool {
    panes.iter().any(pane_is_running)
}

pub(super) fn first_running_pane_index(panes: &[PaneRuntime]) -> Option<usize> {
    panes.iter().position(pane_is_running)
}

pub(super) fn next_focusable_pane_index(panes: &[PaneRuntime], current: usize) -> usize {
    if panes.is_empty() {
        return 0;
    }

    for offset in 1..=panes.len() {
        let index = (current + offset) % panes.len();
        if pane_is_running(&panes[index]) {
            return index;
        }
    }

    current.min(panes.len() - 1)
}

pub(super) fn resize_panes(panes: &mut [PaneRuntime], layout: &Layout) -> Result<()> {
    for (pane, rect) in panes
        .iter_mut()
        .zip([layout.left.inner(), layout.right.inner()])
    {
        if let Some(process) = pane.process.as_mut() {
            process
                .master
                .resize(PtySize {
                    rows: rect.height.max(MIN_PANE_ROWS),
                    cols: rect.width.max(MIN_PANE_COLS),
                    pixel_width: 0,
                    pixel_height: 0,
                })
                .context("failed to resize pane PTY")?;
        }

        let mut parser = pane
            .state
            .parser
            .lock()
            .expect("pane parser mutex poisoned");
        parser.screen_mut().set_size(
            rect.height.max(MIN_PANE_ROWS),
            rect.width.max(MIN_PANE_COLS),
        );
        pane.state.dirty.store(true, Ordering::Relaxed);
    }

    Ok(())
}
