use super::pane_runtime::{
    next_focusable_pane_index, pane_is_running, spawn_pane_process, stop_pane_process,
};
use super::{PaneRuntime, SPLIT_RATIO_STEP, StatusMessage};
use crate::input::RuntimeAction;
use crate::pane::{Layout, SplitDirection};
use anyhow::{Context, Result};
use std::io::Write;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::time::Instant;

pub(super) fn process_input_events(
    input_rx: &Receiver<RuntimeAction>,
    panes: &mut [PaneRuntime],
    layout: &Layout,
    focused_pane: &mut usize,
    split_direction: &mut SplitDirection,
    split_ratio: &mut f32,
    shutdown_requested: &Arc<AtomicBool>,
    force_redraw: &mut bool,
    exit_override: &mut Option<u8>,
    status_message: &mut Option<StatusMessage>,
    startup_deadline: Instant,
    user_input_seen: Arc<AtomicBool>,
) -> Result<()> {
    loop {
        match input_rx.try_recv() {
            Ok(RuntimeAction::ForwardToPane(bytes)) => {
                if let Some(active_pane) = panes.get_mut(*focused_pane) {
                    if let Some(process) = active_pane.process.as_mut() {
                        process
                            .writer
                            .write_all(&bytes)
                            .and_then(|_| process.writer.flush())
                            .context("failed writing input to pane")?;
                    }
                }
            }
            Ok(action) => {
                match action {
                    RuntimeAction::Quit => {
                        shutdown_requested.store(true, Ordering::Relaxed);
                        *exit_override = Some(0);
                    }
                    RuntimeAction::FocusNext => {
                        *focused_pane = next_focusable_pane_index(panes, *focused_pane);
                    }
                    RuntimeAction::ToggleSplitDirection => {
                        *split_direction = match *split_direction {
                            SplitDirection::Vertical => SplitDirection::Horizontal,
                            SplitDirection::Horizontal => SplitDirection::Vertical,
                        };
                        let label = match *split_direction {
                            SplitDirection::Vertical => "vertical",
                            SplitDirection::Horizontal => "horizontal",
                        };
                        *status_message = Some(StatusMessage::new(format!("split: {label}")));
                    }
                    RuntimeAction::IncreaseSplit => {
                        *split_ratio = (*split_ratio + SPLIT_RATIO_STEP).clamp(0.2, 0.8);
                    }
                    RuntimeAction::DecreaseSplit => {
                        *split_ratio = (*split_ratio - SPLIT_RATIO_STEP).clamp(0.2, 0.8);
                    }
                    RuntimeAction::RestartFocusedPane => {
                        let pane_inner = if *focused_pane == 0 {
                            layout.left.inner()
                        } else {
                            layout.right.inner()
                        };
                        if let Some(pane) = panes.get_mut(*focused_pane) {
                            stop_pane_process(pane, true)?;
                            pane.process = Some(spawn_pane_process(
                                &pane.shell,
                                pane.title.clone(),
                                pane_inner,
                                startup_deadline,
                                Arc::clone(&user_input_seen),
                                Arc::clone(&pane.state),
                            )?);
                            pane.closed = false;
                            pane.exit_code = None;
                            pane.state.dirty.store(true, Ordering::Relaxed);
                            *status_message = Some(StatusMessage::new(format!(
                                "pane '{}' restarted",
                                pane.title
                            )));
                        }
                    }
                    RuntimeAction::CloseFocusedPane => {
                        let running_count =
                            panes.iter().filter(|pane| pane_is_running(pane)).count();
                        if running_count <= 1 {
                            *status_message = Some(StatusMessage::new(
                                "cannot close the last running pane".to_string(),
                            ));
                        } else if let Some(pane) = panes.get_mut(*focused_pane) {
                            let closed_title = pane.title.clone();
                            stop_pane_process(pane, true)?;
                            pane.closed = true;
                            pane.exit_code = None;
                            pane.state.dirty.store(true, Ordering::Relaxed);
                            *status_message =
                                Some(StatusMessage::new(format!("pane '{closed_title}' closed")));
                        }

                        *focused_pane = next_focusable_pane_index(panes, *focused_pane);
                    }
                    RuntimeAction::ShowHelp => {
                        *status_message = Some(StatusMessage::new(
                            "Ctrl-A: q quit | o focus | t toggle split | +/- resize | r restart | x close | ? help"
                                .to_string(),
                        ));
                    }
                    RuntimeAction::Eof => {
                        shutdown_requested.store(true, Ordering::Relaxed);
                        *exit_override = Some(0);
                    }
                    RuntimeAction::ForwardToPane(_) => unreachable!(),
                }

                *force_redraw = true;
            }
            Err(mpsc::TryRecvError::Empty) => break,
            Err(mpsc::TryRecvError::Disconnected) => {
                shutdown_requested.store(true, Ordering::Relaxed);
                break;
            }
        }
    }

    Ok(())
}
