use super::pane_runtime::{next_focusable_pane_id, spawn_pane_process, stop_pane_process};
use super::{PaneRuntime, StatusMessage};
use crate::input::RuntimeAction;
use crate::pane::{LayoutTree, PaneId, Rect, SplitDirection};
use anyhow::{Context, Result};
use std::collections::BTreeMap;
use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::Arc;
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

#[cfg(test)]
mod tests {
    use super::process_input_events;
    use crate::input::RuntimeAction;
    use crate::pane::{LayoutNode, LayoutTree, PaneId, SplitDirection};
    use crate::runtime::{PaneRuntime, PaneState};
    use std::collections::BTreeMap;
    use std::sync::atomic::AtomicBool;
    use std::sync::mpsc;
    use std::sync::{Arc, Mutex};
    use std::time::Instant;
    use vt100::Parser as VtParser;

    fn make_pane(title: &str) -> PaneRuntime {
        PaneRuntime {
            title: title.to_string(),
            shell: "/bin/sh".to_string(),
            state: Arc::new(PaneState {
                parser: Mutex::new(VtParser::new(10, 10, 100)),
                dirty: AtomicBool::new(false),
            }),
            process: None,
            closed: false,
            exit_code: None,
        }
    }

    #[test]
    fn close_focused_removes_middle_pane_and_rebalances() {
        let mut panes = BTreeMap::new();
        panes.insert(PaneId(1), make_pane("pane-1"));
        panes.insert(PaneId(2), make_pane("pane-2"));
        panes.insert(PaneId(3), make_pane("pane-3"));

        let mut layout = LayoutTree::two_pane(PaneId(1), PaneId(2), SplitDirection::Vertical, 0.5);
        layout.focused = PaneId(2);
        assert!(layout.split_focused(SplitDirection::Horizontal, PaneId(3), 0.5));

        let mut focused = PaneId(2);
        layout.focused = focused;
        let pane_rects = layout.compute_rects(120, 40);
        let shutdown_requested = Arc::new(AtomicBool::new(false));
        let mut force_redraw = false;
        let mut exit_override = None;
        let mut status_message = None;

        let (tx, rx) = mpsc::channel();
        tx.send(RuntimeAction::CloseFocusedPane)
            .expect("send close action");
        drop(tx);

        let updated = process_input_events(
            &rx,
            &mut panes,
            &pane_rects,
            &layout,
            &mut focused,
            &shutdown_requested,
            &mut force_redraw,
            &mut exit_override,
            &mut status_message,
            Instant::now(),
            Arc::new(AtomicBool::new(false)),
        )
        .expect("process input events")
        .expect("tree updated by close");

        assert!(force_redraw);
        assert!(!panes.contains_key(&PaneId(2)));
        assert_eq!(updated.pane_order(), vec![PaneId(1), PaneId(3)]);
        assert_eq!(focused, PaneId(1));
    }

    #[test]
    fn toggle_and_resize_affect_focused_subtree() {
        let mut panes = BTreeMap::new();
        panes.insert(PaneId(1), make_pane("pane-1"));
        panes.insert(PaneId(2), make_pane("pane-2"));
        panes.insert(PaneId(3), make_pane("pane-3"));

        let mut layout = LayoutTree::two_pane(PaneId(1), PaneId(2), SplitDirection::Vertical, 0.5);
        layout.focused = PaneId(2);
        assert!(layout.split_focused(SplitDirection::Horizontal, PaneId(3), 0.5));

        let mut focused = PaneId(3);
        layout.focused = focused;
        let pane_rects = layout.compute_rects(120, 40);
        let shutdown_requested = Arc::new(AtomicBool::new(false));
        let mut force_redraw = false;
        let mut exit_override = None;
        let mut status_message = None;

        let (tx, rx) = mpsc::channel();
        tx.send(RuntimeAction::ToggleSplitDirection)
            .expect("send toggle");
        tx.send(RuntimeAction::IncreaseSplit).expect("send resize");
        drop(tx);

        let updated = process_input_events(
            &rx,
            &mut panes,
            &pane_rects,
            &layout,
            &mut focused,
            &shutdown_requested,
            &mut force_redraw,
            &mut exit_override,
            &mut status_message,
            Instant::now(),
            Arc::new(AtomicBool::new(false)),
        )
        .expect("process input events")
        .expect("tree updated by commands");

        match updated.root {
            LayoutNode::Split {
                direction,
                first,
                second,
                ..
            } => {
                assert_eq!(direction, SplitDirection::Vertical);
                assert!(matches!(*first, LayoutNode::Leaf { pane_id: PaneId(1) }));
                match *second {
                    LayoutNode::Split {
                        direction, ratio, ..
                    } => {
                        assert_eq!(direction, SplitDirection::Vertical);
                        assert!((ratio - 0.55).abs() < 0.001);
                    }
                    _ => panic!("expected nested split"),
                }
            }
            _ => panic!("expected split root"),
        }
    }
}
