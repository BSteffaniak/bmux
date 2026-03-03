use super::pane_runtime::{next_focusable_pane_id, spawn_pane_process, stop_pane_process};
use super::{PaneRuntime, StatusMessage};
use crate::input::RuntimeAction;
use crate::pane::{LayoutTree, PaneId, Rect, SplitDirection};
use anyhow::{Context, Result};
use std::collections::BTreeMap;
use std::io::Write;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::time::Instant;

const SPLIT_RATIO_STEP: f32 = 0.05;

pub(super) fn process_input_events(
    input_rx: &Receiver<RuntimeAction>,
    panes: &mut BTreeMap<PaneId, PaneRuntime>,
    pane_rects: &BTreeMap<PaneId, Rect>,
    layout_tree: &LayoutTree,
    focused_pane: &mut PaneId,
    shutdown_requested: &Arc<AtomicBool>,
    force_redraw: &mut bool,
    exit_override: &mut Option<u8>,
    status_message: &mut Option<StatusMessage>,
    startup_deadline: Instant,
    user_input_seen: Arc<AtomicBool>,
) -> Result<Option<LayoutTree>> {
    let mut pending_tree_update = None;

    loop {
        match input_rx.try_recv() {
            Ok(RuntimeAction::ForwardToPane(bytes)) => {
                if let Some(active_pane) = panes.get_mut(focused_pane) {
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
                        *focused_pane =
                            next_focusable_pane_id(&layout_tree.pane_order(), panes, *focused_pane);
                    }
                    RuntimeAction::ToggleSplitDirection => {
                        let mut updated_tree = pending_tree_update
                            .as_ref()
                            .cloned()
                            .unwrap_or_else(|| layout_tree.clone());
                        if let Some(next_direction) = updated_tree.toggle_focused_split_direction()
                        {
                            pending_tree_update = Some(updated_tree);

                            let label = match next_direction {
                                SplitDirection::Vertical => "vertical",
                                SplitDirection::Horizontal => "horizontal",
                            };
                            *status_message = Some(StatusMessage::new(format!("split: {label}")));
                        }
                    }
                    RuntimeAction::IncreaseSplit => {
                        let mut updated_tree = pending_tree_update
                            .as_ref()
                            .cloned()
                            .unwrap_or_else(|| layout_tree.clone());
                        if updated_tree
                            .adjust_focused_split_ratio(SPLIT_RATIO_STEP)
                            .is_some()
                        {
                            pending_tree_update = Some(updated_tree);
                        }
                    }
                    RuntimeAction::DecreaseSplit => {
                        let mut updated_tree = pending_tree_update
                            .as_ref()
                            .cloned()
                            .unwrap_or_else(|| layout_tree.clone());
                        if updated_tree
                            .adjust_focused_split_ratio(-SPLIT_RATIO_STEP)
                            .is_some()
                        {
                            pending_tree_update = Some(updated_tree);
                        }
                    }
                    RuntimeAction::SplitFocusedVertical | RuntimeAction::SplitFocusedHorizontal => {
                        let split_direction = match action {
                            RuntimeAction::SplitFocusedVertical => SplitDirection::Vertical,
                            RuntimeAction::SplitFocusedHorizontal => SplitDirection::Horizontal,
                            _ => unreachable!(),
                        };

                        let mut updated_tree = pending_tree_update
                            .as_ref()
                            .cloned()
                            .unwrap_or_else(|| layout_tree.clone());

                        let new_pane_id = next_pane_id(panes);
                        let pane_title = format!("pane-{}", new_pane_id.0);

                        if let Some(active_pane) = panes.get(focused_pane) {
                            let pane_inner = pane_rects
                                .get(focused_pane)
                                .map(|rect| rect.inner())
                                .unwrap_or_default();

                            panes.insert(
                                new_pane_id,
                                super::pane_runtime::spawn_pane(
                                    &active_pane.shell,
                                    pane_title.clone(),
                                    pane_inner,
                                    startup_deadline,
                                    Arc::clone(&user_input_seen),
                                )?,
                            );

                            if updated_tree.split_focused(split_direction, new_pane_id, 0.5) {
                                *focused_pane = new_pane_id;
                                pending_tree_update = Some(updated_tree);
                                let label = match split_direction {
                                    SplitDirection::Vertical => "vertical",
                                    SplitDirection::Horizontal => "horizontal",
                                };
                                *status_message = Some(StatusMessage::new(format!(
                                    "split {label}: {pane_title}"
                                )));
                            } else {
                                if let Some(mut pane) = panes.remove(&new_pane_id) {
                                    stop_pane_process(&mut pane, true)?;
                                }
                                *status_message = Some(StatusMessage::new(
                                    "failed to split focused pane".to_string(),
                                ));
                            }
                        }
                    }
                    RuntimeAction::RestartFocusedPane => {
                        if let Some(rect) = pane_rects.get(focused_pane) {
                            let pane_inner = rect.inner();
                            if let Some(pane) = panes.get_mut(focused_pane) {
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
                    }
                    RuntimeAction::CloseFocusedPane => {
                        let mut updated_tree = pending_tree_update
                            .as_ref()
                            .cloned()
                            .unwrap_or_else(|| layout_tree.clone());

                        if updated_tree.pane_order().len() <= 1 {
                            *status_message =
                                Some(StatusMessage::new("cannot close the last pane".to_string()));
                        } else {
                            let closing_pane = *focused_pane;
                            if updated_tree.remove_pane(closing_pane) {
                                if let Some(mut pane) = panes.remove(&closing_pane) {
                                    let closed_title = pane.title.clone();
                                    stop_pane_process(&mut pane, true)?;
                                    *status_message = Some(StatusMessage::new(format!(
                                        "pane '{closed_title}' closed"
                                    )));
                                }

                                *focused_pane = updated_tree.focused;
                                pending_tree_update = Some(updated_tree);
                            }
                        }
                    }
                    RuntimeAction::ShowHelp => {
                        *status_message = Some(StatusMessage::new(
                            "Ctrl-A: q quit | o focus | t toggle layout | % split-v | \" split-h | +/- resize | r restart | x close | ? help"
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

    Ok(pending_tree_update)
}

fn next_pane_id(panes: &BTreeMap<PaneId, PaneRuntime>) -> PaneId {
    let next = panes
        .keys()
        .map(|pane_id| pane_id.0)
        .max()
        .unwrap_or(0)
        .saturating_add(1);
    PaneId(next)
}
