use super::pane_runtime::{next_focusable_pane_id, spawn_pane_process, stop_pane_process};
use super::terminal_protocol::{ProtocolProfile, SharedProtocolTraceBuffer};
use super::{PaneRuntime, ScrollState, StatusMessage};
use crate::input::RuntimeAction;
use crate::pane::{LayoutTree, PaneId, Rect, ResizeDirection, SplitDirection};
use anyhow::{Context, Result};
use std::collections::BTreeMap;
use std::io::Write;
use std::process::{Command as ProcessCommand, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::time::Instant;

const SPLIT_RATIO_STEP: f32 = 0.05;

#[derive(Clone, Copy)]
enum FocusDirection {
    Left,
    Right,
    Up,
    Down,
}

pub(super) fn process_input_events(
    input_rx: &Receiver<RuntimeAction>,
    panes: &mut BTreeMap<PaneId, PaneRuntime>,
    pane_rects: &BTreeMap<PaneId, Rect>,
    layout_tree: &LayoutTree,
    focused_pane: &mut PaneId,
    shutdown_requested: &Arc<AtomicBool>,
    confirm_quit_destroy: bool,
    pending_destroy_confirm: &mut bool,
    destroy_state_requested: &mut bool,
    detach_requested: &mut bool,
    force_redraw: &mut bool,
    exit_override: &mut Option<u8>,
    status_message: &mut Option<StatusMessage>,
    scroll_state: &mut ScrollState,
    internal_clipboard: &mut Option<String>,
    startup_deadline: Instant,
    user_input_seen: Arc<AtomicBool>,
    scrollback_limit: usize,
    pane_term: &str,
    protocol_profile: ProtocolProfile,
    protocol_trace: Option<SharedProtocolTraceBuffer>,
) -> Result<Option<LayoutTree>> {
    let mut pending_tree_update = None;

    loop {
        match input_rx.try_recv() {
            Ok(RuntimeAction::ForwardToPane(bytes)) => {
                if *pending_destroy_confirm {
                    let confirmed = bytes
                        .first()
                        .is_some_and(|value| matches!(value, b'y' | b'Y'));
                    if confirmed {
                        *pending_destroy_confirm = false;
                        *destroy_state_requested = true;
                        shutdown_requested.store(true, Ordering::Relaxed);
                        *exit_override = Some(0);
                        *status_message = Some(StatusMessage::new(
                            "quitting and clearing persisted state".to_string(),
                        ));
                    } else {
                        *pending_destroy_confirm = false;
                        *status_message = Some(StatusMessage::new("quit cancelled".to_string()));
                    }
                    continue;
                }
                if scroll_state.active {
                    continue;
                }
                if let Some(active_pane) = panes.get_mut(focused_pane) {
                    if let Some(process) = active_pane.process.as_mut() {
                        let mut writer = process
                            .writer
                            .lock()
                            .expect("pane PTY writer mutex poisoned");
                        writer
                            .write_all(&bytes)
                            .and_then(|_| writer.flush())
                            .context("failed writing input to pane")?;
                    }
                }
            }
            Ok(action) => {
                match action {
                    RuntimeAction::Detach => {
                        *pending_destroy_confirm = false;
                        *detach_requested = true;
                        shutdown_requested.store(true, Ordering::Relaxed);
                        *exit_override = Some(0);
                    }
                    RuntimeAction::Quit => {
                        if confirm_quit_destroy {
                            *pending_destroy_confirm = true;
                            *status_message = Some(StatusMessage::new(
                                "destroy persisted state and quit? [y/N]".to_string(),
                            ));
                        } else {
                            *pending_destroy_confirm = false;
                            *destroy_state_requested = true;
                            shutdown_requested.store(true, Ordering::Relaxed);
                            *exit_override = Some(0);
                        }
                    }
                    RuntimeAction::NewWindow => {
                        *status_message = Some(StatusMessage::new(
                            "new window is only available in attach mode".to_string(),
                        ));
                    }
                    RuntimeAction::NewSession => {
                        *status_message = Some(StatusMessage::new(
                            "new session is only available in attach mode".to_string(),
                        ));
                    }
                    RuntimeAction::FocusNext => {
                        *focused_pane =
                            next_focusable_pane_id(&layout_tree.pane_order(), panes, *focused_pane);
                    }
                    RuntimeAction::FocusLeft => {
                        *focused_pane = focus_in_direction(
                            *focused_pane,
                            panes,
                            pane_rects,
                            FocusDirection::Left,
                        );
                    }
                    RuntimeAction::FocusRight => {
                        *focused_pane = focus_in_direction(
                            *focused_pane,
                            panes,
                            pane_rects,
                            FocusDirection::Right,
                        );
                    }
                    RuntimeAction::FocusUp => {
                        *focused_pane = focus_in_direction(
                            *focused_pane,
                            panes,
                            pane_rects,
                            FocusDirection::Up,
                        );
                    }
                    RuntimeAction::FocusDown => {
                        *focused_pane = focus_in_direction(
                            *focused_pane,
                            panes,
                            pane_rects,
                            FocusDirection::Down,
                        );
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
                    RuntimeAction::ResizeLeft => {
                        apply_directional_resize(
                            pending_tree_update
                                .as_ref()
                                .cloned()
                                .unwrap_or_else(|| layout_tree.clone()),
                            ResizeDirection::Left,
                            &mut pending_tree_update,
                            status_message,
                        );
                    }
                    RuntimeAction::ResizeRight => {
                        apply_directional_resize(
                            pending_tree_update
                                .as_ref()
                                .cloned()
                                .unwrap_or_else(|| layout_tree.clone()),
                            ResizeDirection::Right,
                            &mut pending_tree_update,
                            status_message,
                        );
                    }
                    RuntimeAction::ResizeUp => {
                        apply_directional_resize(
                            pending_tree_update
                                .as_ref()
                                .cloned()
                                .unwrap_or_else(|| layout_tree.clone()),
                            ResizeDirection::Up,
                            &mut pending_tree_update,
                            status_message,
                        );
                    }
                    RuntimeAction::ResizeDown => {
                        apply_directional_resize(
                            pending_tree_update
                                .as_ref()
                                .cloned()
                                .unwrap_or_else(|| layout_tree.clone()),
                            ResizeDirection::Down,
                            &mut pending_tree_update,
                            status_message,
                        );
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
                                    new_pane_id,
                                    &active_pane.shell,
                                    scrollback_limit,
                                    pane_term,
                                    protocol_profile,
                                    pane_title.clone(),
                                    pane_inner,
                                    startup_deadline,
                                    Arc::clone(&user_input_seen),
                                    protocol_trace.clone(),
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
                                    scrollback_limit,
                                    pane_term,
                                    protocol_profile,
                                    *focused_pane,
                                    pane.title.clone(),
                                    pane_inner,
                                    startup_deadline,
                                    Arc::clone(&user_input_seen),
                                    Arc::clone(&pane.state),
                                    protocol_trace.clone(),
                                )?);
                                pane.closed = false;
                                pane.exit_code = None;
                                pane.state.dirty.store(true, Ordering::Relaxed);
                                scroll_state.offsets.remove(focused_pane);
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
                                    scroll_state.offsets.remove(&closing_pane);
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
                            "Ctrl-A: c new window | C new session | d detach(save) | q quit(destroy) | o cycle | h/j/k/l or arrows focus | H/J/K/L directional resize | t toggle layout | % split-v | \" split-h | +/- resize | [ scroll mode | Esc (or ]) exit scroll | arrows/Ctrl-Y/Ctrl-E line | PgUp/PgDn page | g/G top/bottom | y copy view | r restart | x close | ? help"
                                .to_string(),
                        ));
                    }
                    RuntimeAction::EnterScrollMode => {
                        scroll_state.active = true;
                        if let Some(active_pane) = panes.get_mut(focused_pane) {
                            let parser = active_pane
                                .state
                                .parser
                                .lock()
                                .expect("pane parser mutex poisoned");
                            let offset = parser.screen().scrollback();
                            let (cursor_row, cursor_col) = parser.screen().cursor_position();
                            scroll_state.offsets.insert(*focused_pane, offset);
                            let (max_row, max_col) = pane_rects
                                .get(focused_pane)
                                .map(|rect| {
                                    (
                                        rect.inner().height.saturating_sub(1),
                                        rect.inner().width.saturating_sub(1),
                                    )
                                })
                                .unwrap_or((0, 0));
                            scroll_state.cursors.insert(
                                *focused_pane,
                                (cursor_row.min(max_row), cursor_col.min(max_col)),
                            );
                            active_pane.state.dirty.store(true, Ordering::Relaxed);
                        }
                        *status_message = Some(StatusMessage::new(
                            "scroll mode: arrows/Ctrl-Y/Ctrl-E line, PgUp/PgDn page, g/G top/bottom, v select, y copy, Esc exit"
                                .to_string(),
                        ));
                    }
                    RuntimeAction::ExitScrollMode => {
                        if scroll_state
                            .selection_anchors
                            .remove(focused_pane)
                            .is_some()
                        {
                            *status_message =
                                Some(StatusMessage::new("selection cancelled".to_string()));
                            if let Some(active_pane) = panes.get_mut(focused_pane) {
                                active_pane.state.dirty.store(true, Ordering::Relaxed);
                            }
                            continue;
                        }

                        scroll_state.active = false;
                        scroll_state.offsets.clear();
                        scroll_state.cursors.clear();
                        scroll_state.selection_anchors.clear();
                        for pane in panes.values_mut() {
                            let mut parser = pane
                                .state
                                .parser
                                .lock()
                                .expect("pane parser mutex poisoned");
                            parser.screen_mut().set_scrollback(0);
                            pane.state.dirty.store(true, Ordering::Relaxed);
                        }
                        *status_message =
                            Some(StatusMessage::new("scroll mode exited".to_string()));
                    }
                    RuntimeAction::ScrollUpLine
                    | RuntimeAction::ScrollDownLine
                    | RuntimeAction::ScrollUpPage
                    | RuntimeAction::ScrollDownPage
                    | RuntimeAction::ScrollTop
                    | RuntimeAction::ScrollBottom => {
                        if scroll_state.active {
                            if scroll_state.selection_anchors.contains_key(focused_pane) {
                                apply_selection_scroll_action(
                                    action,
                                    *focused_pane,
                                    panes,
                                    pane_rects,
                                    scroll_state,
                                );
                            } else {
                                apply_scrollback_action(
                                    action,
                                    *focused_pane,
                                    panes,
                                    pane_rects,
                                    scroll_state,
                                );
                            }
                        }
                    }
                    RuntimeAction::BeginSelection => {
                        if scroll_state.active {
                            let cursor = scroll_state
                                .cursors
                                .get(focused_pane)
                                .copied()
                                .or_else(|| pane_rects.get(focused_pane).map(|_| (0, 0)));
                            if let Some(cursor) = cursor {
                                scroll_state.selection_anchors.insert(*focused_pane, cursor);
                                if let Some(active_pane) = panes.get_mut(focused_pane) {
                                    active_pane.state.dirty.store(true, Ordering::Relaxed);
                                }
                                *status_message = Some(StatusMessage::new(
                                    "selection started (move with h/j/k/l)".to_string(),
                                ));
                            }
                        }
                    }
                    RuntimeAction::MoveCursorLeft
                    | RuntimeAction::MoveCursorRight
                    | RuntimeAction::MoveCursorUp
                    | RuntimeAction::MoveCursorDown => {
                        if scroll_state.active {
                            move_selection_cursor(
                                action,
                                *focused_pane,
                                pane_rects,
                                panes,
                                scroll_state,
                            );
                        }
                    }
                    RuntimeAction::CopyScrollback => {
                        if scroll_state.active {
                            if let Some(active_pane) = panes.get_mut(focused_pane) {
                                let parser = active_pane
                                    .state
                                    .parser
                                    .lock()
                                    .expect("pane parser mutex poisoned");
                                let text = if let Some(anchor) =
                                    scroll_state.selection_anchors.get(focused_pane).copied()
                                {
                                    let cursor = scroll_state
                                        .cursors
                                        .get(focused_pane)
                                        .copied()
                                        .unwrap_or(anchor);
                                    let (start_row, start_col, end_row, end_col) =
                                        ordered_range(anchor, cursor);
                                    parser.screen().contents_between(
                                        start_row,
                                        start_col,
                                        end_row,
                                        end_col.saturating_add(1),
                                    )
                                } else {
                                    parser.screen().contents()
                                };
                                drop(parser);

                                if text.is_empty() {
                                    *status_message = Some(StatusMessage::new(
                                        "nothing to copy in current pane view".to_string(),
                                    ));
                                } else if copy_to_system_clipboard(&text) {
                                    scroll_state.selection_anchors.remove(focused_pane);
                                    *status_message = Some(StatusMessage::new(format!(
                                        "copied {} chars to system clipboard",
                                        text.chars().count()
                                    )));
                                } else {
                                    *internal_clipboard = Some(text.clone());
                                    scroll_state.selection_anchors.remove(focused_pane);
                                    *status_message = Some(StatusMessage::new(format!(
                                        "system clipboard unavailable; copied {} chars to internal buffer",
                                        text.chars().count()
                                    )));
                                }
                                active_pane.state.dirty.store(true, Ordering::Relaxed);
                            }
                        }
                    }
                    RuntimeAction::EnterWindowMode
                    | RuntimeAction::ExitMode
                    | RuntimeAction::WindowPrev
                    | RuntimeAction::WindowNext
                    | RuntimeAction::WindowGoto1
                    | RuntimeAction::WindowGoto2
                    | RuntimeAction::WindowGoto3
                    | RuntimeAction::WindowGoto4
                    | RuntimeAction::WindowGoto5
                    | RuntimeAction::WindowGoto6
                    | RuntimeAction::WindowGoto7
                    | RuntimeAction::WindowGoto8
                    | RuntimeAction::WindowGoto9
                    | RuntimeAction::WindowClose => {
                        *status_message = Some(StatusMessage::new(
                            "window-mode actions are only available in attach mode".to_string(),
                        ));
                    }
                    RuntimeAction::ForwardToPane(_) => unreachable!(),
                }

                if scroll_state.active {
                    ensure_scroll_cursor(*focused_pane, panes, pane_rects, scroll_state);
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

fn apply_directional_resize(
    mut updated_tree: LayoutTree,
    direction: ResizeDirection,
    pending_tree_update: &mut Option<LayoutTree>,
    status_message: &mut Option<StatusMessage>,
) {
    if updated_tree
        .adjust_focused_split_toward(direction, SPLIT_RATIO_STEP)
        .is_some()
    {
        *pending_tree_update = Some(updated_tree);
    } else {
        let axis = match direction {
            ResizeDirection::Left | ResizeDirection::Right => "vertical",
            ResizeDirection::Up | ResizeDirection::Down => "horizontal",
        };
        *status_message = Some(StatusMessage::new(format!(
            "no {axis} split found for directional resize"
        )));
    }
}

fn apply_scrollback_action(
    action: RuntimeAction,
    focused_pane: PaneId,
    panes: &mut BTreeMap<PaneId, PaneRuntime>,
    pane_rects: &BTreeMap<PaneId, Rect>,
    scroll_state: &mut ScrollState,
) {
    let Some(active_pane) = panes.get_mut(&focused_pane) else {
        return;
    };
    let page_step = pane_rects
        .get(&focused_pane)
        .map(|rect| usize::from(rect.inner().height.saturating_sub(1)).max(1))
        .unwrap_or(10);

    let mut parser = active_pane
        .state
        .parser
        .lock()
        .expect("pane parser mutex poisoned");
    let current = parser.screen().scrollback();
    let requested = match action {
        RuntimeAction::ScrollUpLine => current.saturating_add(1),
        RuntimeAction::ScrollDownLine => current.saturating_sub(1),
        RuntimeAction::ScrollUpPage => current.saturating_add(page_step),
        RuntimeAction::ScrollDownPage => current.saturating_sub(page_step),
        RuntimeAction::ScrollTop => usize::MAX,
        RuntimeAction::ScrollBottom => 0,
        _ => current,
    };
    parser.screen_mut().set_scrollback(requested);
    let applied = parser.screen().scrollback();
    scroll_state.offsets.insert(focused_pane, applied);
    active_pane.state.dirty.store(true, Ordering::Relaxed);
}

fn apply_selection_scroll_action(
    action: RuntimeAction,
    focused_pane: PaneId,
    panes: &mut BTreeMap<PaneId, PaneRuntime>,
    pane_rects: &BTreeMap<PaneId, Rect>,
    scroll_state: &mut ScrollState,
) {
    let Some(rect) = pane_rects.get(&focused_pane) else {
        return;
    };
    let max_row = rect.inner().height.saturating_sub(1);

    match action {
        RuntimeAction::ScrollUpLine => {
            let at_top = scroll_state
                .cursors
                .get(&focused_pane)
                .map_or(true, |cursor| cursor.0 == 0);
            if at_top {
                apply_scrollback_action(
                    RuntimeAction::ScrollUpLine,
                    focused_pane,
                    panes,
                    pane_rects,
                    scroll_state,
                );
            } else {
                move_selection_cursor(
                    RuntimeAction::MoveCursorUp,
                    focused_pane,
                    pane_rects,
                    panes,
                    scroll_state,
                );
            }
        }
        RuntimeAction::ScrollDownLine => {
            let at_bottom = scroll_state
                .cursors
                .get(&focused_pane)
                .map_or(true, |cursor| cursor.0 >= max_row);
            if at_bottom {
                apply_scrollback_action(
                    RuntimeAction::ScrollDownLine,
                    focused_pane,
                    panes,
                    pane_rects,
                    scroll_state,
                );
            } else {
                move_selection_cursor(
                    RuntimeAction::MoveCursorDown,
                    focused_pane,
                    pane_rects,
                    panes,
                    scroll_state,
                );
            }
        }
        RuntimeAction::ScrollUpPage => {
            apply_scrollback_action(
                RuntimeAction::ScrollUpPage,
                focused_pane,
                panes,
                pane_rects,
                scroll_state,
            );
            if let Some(cursor) = scroll_state.cursors.get_mut(&focused_pane) {
                cursor.0 = 0;
            }
            if let Some(active_pane) = panes.get_mut(&focused_pane) {
                active_pane.state.dirty.store(true, Ordering::Relaxed);
            }
        }
        RuntimeAction::ScrollDownPage => {
            apply_scrollback_action(
                RuntimeAction::ScrollDownPage,
                focused_pane,
                panes,
                pane_rects,
                scroll_state,
            );
            if let Some(cursor) = scroll_state.cursors.get_mut(&focused_pane) {
                cursor.0 = max_row;
            }
            if let Some(active_pane) = panes.get_mut(&focused_pane) {
                active_pane.state.dirty.store(true, Ordering::Relaxed);
            }
        }
        RuntimeAction::ScrollTop => {
            apply_scrollback_action(
                RuntimeAction::ScrollTop,
                focused_pane,
                panes,
                pane_rects,
                scroll_state,
            );
            if let Some(cursor) = scroll_state.cursors.get_mut(&focused_pane) {
                cursor.0 = 0;
            }
            if let Some(active_pane) = panes.get_mut(&focused_pane) {
                active_pane.state.dirty.store(true, Ordering::Relaxed);
            }
        }
        RuntimeAction::ScrollBottom => {
            apply_scrollback_action(
                RuntimeAction::ScrollBottom,
                focused_pane,
                panes,
                pane_rects,
                scroll_state,
            );
            if let Some(cursor) = scroll_state.cursors.get_mut(&focused_pane) {
                cursor.0 = max_row;
            }
            if let Some(active_pane) = panes.get_mut(&focused_pane) {
                active_pane.state.dirty.store(true, Ordering::Relaxed);
            }
        }
        _ => {}
    }
}

fn move_selection_cursor(
    action: RuntimeAction,
    focused_pane: PaneId,
    pane_rects: &BTreeMap<PaneId, Rect>,
    panes: &mut BTreeMap<PaneId, PaneRuntime>,
    scroll_state: &mut ScrollState,
) {
    let Some(rect) = pane_rects.get(&focused_pane) else {
        return;
    };

    let max_row = rect.inner().height.saturating_sub(1);
    let max_col = rect.inner().width.saturating_sub(1);
    let (mut row, mut col) = scroll_state
        .cursors
        .get(&focused_pane)
        .copied()
        .unwrap_or((0, 0));
    let mut changed_cursor = false;
    match action {
        RuntimeAction::MoveCursorLeft => {
            col = col.saturating_sub(1);
            changed_cursor = true;
        }
        RuntimeAction::MoveCursorRight => {
            col = col.saturating_add(1).min(max_col);
            changed_cursor = true;
        }
        RuntimeAction::MoveCursorUp => {
            if row == 0 {
                apply_scrollback_action(
                    RuntimeAction::ScrollUpLine,
                    focused_pane,
                    panes,
                    pane_rects,
                    scroll_state,
                );
            } else {
                row = row.saturating_sub(1);
                changed_cursor = true;
            }
        }
        RuntimeAction::MoveCursorDown => {
            if row >= max_row {
                apply_scrollback_action(
                    RuntimeAction::ScrollDownLine,
                    focused_pane,
                    panes,
                    pane_rects,
                    scroll_state,
                );
            } else {
                row = row.saturating_add(1).min(max_row);
                changed_cursor = true;
            }
        }
        _ => {}
    }

    if changed_cursor {
        scroll_state.cursors.insert(focused_pane, (row, col));
    }

    if let Some(active_pane) = panes.get_mut(&focused_pane) {
        active_pane.state.dirty.store(true, Ordering::Relaxed);
    }
}

fn ordered_range(start: (u16, u16), end: (u16, u16)) -> (u16, u16, u16, u16) {
    if start <= end {
        (start.0, start.1, end.0, end.1)
    } else {
        (end.0, end.1, start.0, start.1)
    }
}

fn ensure_scroll_cursor(
    focused_pane: PaneId,
    panes: &mut BTreeMap<PaneId, PaneRuntime>,
    pane_rects: &BTreeMap<PaneId, Rect>,
    scroll_state: &mut ScrollState,
) {
    if scroll_state.cursors.contains_key(&focused_pane) {
        return;
    }
    let Some(active_pane) = panes.get_mut(&focused_pane) else {
        return;
    };
    let Some(rect) = pane_rects.get(&focused_pane) else {
        return;
    };

    let parser = active_pane
        .state
        .parser
        .lock()
        .expect("pane parser mutex poisoned");
    let (row, col) = parser.screen().cursor_position();
    drop(parser);

    scroll_state.cursors.insert(
        focused_pane,
        (
            row.min(rect.inner().height.saturating_sub(1)),
            col.min(rect.inner().width.saturating_sub(1)),
        ),
    );
}

fn copy_to_system_clipboard(contents: &str) -> bool {
    #[cfg(target_os = "macos")]
    {
        if run_clipboard_command("pbcopy", &[], contents) {
            return true;
        }
    }

    #[cfg(not(target_os = "macos"))]
    {
        if run_clipboard_command("wl-copy", &[], contents) {
            return true;
        }
        if run_clipboard_command("xclip", &["-selection", "clipboard"], contents) {
            return true;
        }
        if run_clipboard_command("xsel", &["--clipboard", "--input"], contents) {
            return true;
        }
    }

    false
}

fn run_clipboard_command(command: &str, args: &[&str], contents: &str) -> bool {
    let mut child = match ProcessCommand::new(command)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(child) => child,
        Err(_) => return false,
    };

    if let Some(mut stdin) = child.stdin.take() {
        if stdin.write_all(contents.as_bytes()).is_err() {
            let _ = child.kill();
            let _ = child.wait();
            return false;
        }
    }

    child.wait().is_ok_and(|status| status.success())
}

fn focus_in_direction(
    current: PaneId,
    panes: &BTreeMap<PaneId, PaneRuntime>,
    pane_rects: &BTreeMap<PaneId, Rect>,
    direction: FocusDirection,
) -> PaneId {
    let Some(current_rect) = pane_rects.get(&current).copied() else {
        return current;
    };

    let mut best: Option<(i32, i32, i32, PaneId)> = None;

    for (pane_id, rect) in pane_rects {
        if *pane_id == current || !panes.contains_key(pane_id) {
            continue;
        }

        let (primary_distance, overlap_penalty, center_distance) =
            directional_metrics(current_rect, *rect, direction);
        let Some(primary) = primary_distance else {
            continue;
        };
        let candidate = (primary, overlap_penalty, center_distance, *pane_id);

        if best.is_none_or(|existing| candidate < existing) {
            best = Some(candidate);
        }
    }

    best.map(|value| value.3).unwrap_or(current)
}

fn directional_metrics(
    current: Rect,
    candidate: Rect,
    direction: FocusDirection,
) -> (Option<i32>, i32, i32) {
    let current_left = i32::from(current.x);
    let current_top = i32::from(current.y);
    let current_right = i32::from(current.x.saturating_add(current.width.saturating_sub(1)));
    let current_bottom = i32::from(current.y.saturating_add(current.height.saturating_sub(1)));

    let candidate_left = i32::from(candidate.x);
    let candidate_top = i32::from(candidate.y);
    let candidate_right = i32::from(
        candidate
            .x
            .saturating_add(candidate.width.saturating_sub(1)),
    );
    let candidate_bottom = i32::from(
        candidate
            .y
            .saturating_add(candidate.height.saturating_sub(1)),
    );

    let current_center_x = (current_left + current_right) / 2;
    let current_center_y = (current_top + current_bottom) / 2;
    let candidate_center_x = (candidate_left + candidate_right) / 2;
    let candidate_center_y = (candidate_top + candidate_bottom) / 2;

    match direction {
        FocusDirection::Left => {
            if candidate_right >= current_left {
                return (None, i32::MAX, i32::MAX);
            }
            let primary = current_left - candidate_right;
            let overlap =
                axis_overlap(current_top, current_bottom, candidate_top, candidate_bottom);
            let overlap_penalty = -overlap;
            let center_distance = (current_center_y - candidate_center_y).abs();
            (Some(primary), overlap_penalty, center_distance)
        }
        FocusDirection::Right => {
            if candidate_left <= current_right {
                return (None, i32::MAX, i32::MAX);
            }
            let primary = candidate_left - current_right;
            let overlap =
                axis_overlap(current_top, current_bottom, candidate_top, candidate_bottom);
            let overlap_penalty = -overlap;
            let center_distance = (current_center_y - candidate_center_y).abs();
            (Some(primary), overlap_penalty, center_distance)
        }
        FocusDirection::Up => {
            if candidate_bottom >= current_top {
                return (None, i32::MAX, i32::MAX);
            }
            let primary = current_top - candidate_bottom;
            let overlap =
                axis_overlap(current_left, current_right, candidate_left, candidate_right);
            let overlap_penalty = -overlap;
            let center_distance = (current_center_x - candidate_center_x).abs();
            (Some(primary), overlap_penalty, center_distance)
        }
        FocusDirection::Down => {
            if candidate_top <= current_bottom {
                return (None, i32::MAX, i32::MAX);
            }
            let primary = candidate_top - current_bottom;
            let overlap =
                axis_overlap(current_left, current_right, candidate_left, candidate_right);
            let overlap_penalty = -overlap;
            let center_distance = (current_center_x - candidate_center_x).abs();
            (Some(primary), overlap_penalty, center_distance)
        }
    }
}

fn axis_overlap(a_start: i32, a_end: i32, b_start: i32, b_end: i32) -> i32 {
    let start = a_start.max(b_start);
    let end = a_end.min(b_end);
    (end - start + 1).max(0)
}

#[cfg(test)]
mod tests {
    use super::{
        FocusDirection, ProtocolProfile, ScrollState, focus_in_direction, process_input_events,
    };
    use crate::input::RuntimeAction;
    use crate::pane::{LayoutNode, LayoutTree, PaneId, Rect, SplitDirection};
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

    fn feed_lines(pane: &PaneRuntime, lines: usize) {
        let mut parser = pane
            .state
            .parser
            .lock()
            .expect("pane parser mutex poisoned");
        for index in 0..lines {
            parser.process(format!("line-{index}\r\n").as_bytes());
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn process_input_events_for_test(
        input_rx: &mpsc::Receiver<RuntimeAction>,
        panes: &mut BTreeMap<PaneId, PaneRuntime>,
        pane_rects: &BTreeMap<PaneId, Rect>,
        layout_tree: &LayoutTree,
        focused_pane: &mut PaneId,
        shutdown_requested: &Arc<AtomicBool>,
        force_redraw: &mut bool,
        exit_override: &mut Option<u8>,
        status_message: &mut Option<super::StatusMessage>,
        scroll_state: &mut ScrollState,
        internal_clipboard: &mut Option<String>,
        startup_deadline: Instant,
        user_input_seen: Arc<AtomicBool>,
        scrollback_limit: usize,
        pane_term: &str,
        protocol_profile: ProtocolProfile,
        protocol_trace: Option<super::SharedProtocolTraceBuffer>,
    ) -> anyhow::Result<Option<LayoutTree>> {
        let mut pending_destroy_confirm = false;
        let mut destroy_state_requested = false;
        let mut detach_requested = false;
        process_input_events(
            input_rx,
            panes,
            pane_rects,
            layout_tree,
            focused_pane,
            shutdown_requested,
            true,
            &mut pending_destroy_confirm,
            &mut destroy_state_requested,
            &mut detach_requested,
            force_redraw,
            exit_override,
            status_message,
            scroll_state,
            internal_clipboard,
            startup_deadline,
            user_input_seen,
            scrollback_limit,
            pane_term,
            protocol_profile,
            protocol_trace,
        )
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
        let mut scroll_state = ScrollState::default();
        let mut internal_clipboard = None;

        let (tx, rx) = mpsc::channel();
        tx.send(RuntimeAction::CloseFocusedPane)
            .expect("send close action");
        drop(tx);

        let updated = process_input_events_for_test(
            &rx,
            &mut panes,
            &pane_rects,
            &layout,
            &mut focused,
            &shutdown_requested,
            &mut force_redraw,
            &mut exit_override,
            &mut status_message,
            &mut scroll_state,
            &mut internal_clipboard,
            Instant::now(),
            Arc::new(AtomicBool::new(false)),
            10_000,
            "bmux-256color",
            ProtocolProfile::Conservative,
            None,
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
        let mut scroll_state = ScrollState::default();
        let mut internal_clipboard = None;

        let (tx, rx) = mpsc::channel();
        tx.send(RuntimeAction::ToggleSplitDirection)
            .expect("send toggle");
        tx.send(RuntimeAction::IncreaseSplit).expect("send resize");
        drop(tx);

        let updated = process_input_events_for_test(
            &rx,
            &mut panes,
            &pane_rects,
            &layout,
            &mut focused,
            &shutdown_requested,
            &mut force_redraw,
            &mut exit_override,
            &mut status_message,
            &mut scroll_state,
            &mut internal_clipboard,
            Instant::now(),
            Arc::new(AtomicBool::new(false)),
            10_000,
            "bmux-256color",
            ProtocolProfile::Conservative,
            None,
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

    #[test]
    fn directional_focus_moves_to_adjacent_pane() {
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
        let mut scroll_state = ScrollState::default();
        let mut internal_clipboard = None;

        let (tx, rx) = mpsc::channel();
        tx.send(RuntimeAction::FocusLeft).expect("send left");
        tx.send(RuntimeAction::FocusRight).expect("send right");
        tx.send(RuntimeAction::FocusUp).expect("send up");
        drop(tx);

        let _ = process_input_events_for_test(
            &rx,
            &mut panes,
            &pane_rects,
            &layout,
            &mut focused,
            &shutdown_requested,
            &mut force_redraw,
            &mut exit_override,
            &mut status_message,
            &mut scroll_state,
            &mut internal_clipboard,
            Instant::now(),
            Arc::new(AtomicBool::new(false)),
            10_000,
            "bmux-256color",
            ProtocolProfile::Conservative,
            None,
        )
        .expect("process input events");

        assert_eq!(focused, PaneId(2));
    }

    #[test]
    fn directional_focus_prefers_axis_overlap_before_center_distance() {
        let mut panes = BTreeMap::new();
        panes.insert(PaneId(1), make_pane("current"));
        panes.insert(PaneId(2), make_pane("left-overlap"));
        panes.insert(PaneId(3), make_pane("left-no-overlap"));

        let mut rects = BTreeMap::new();
        rects.insert(
            PaneId(1),
            Rect {
                x: 50,
                y: 10,
                width: 10,
                height: 10,
            },
        );
        rects.insert(
            PaneId(2),
            Rect {
                x: 40,
                y: 11,
                width: 10,
                height: 10,
            },
        );
        rects.insert(
            PaneId(3),
            Rect {
                x: 40,
                y: 40,
                width: 10,
                height: 10,
            },
        );

        let next = focus_in_direction(PaneId(1), &panes, &rects, FocusDirection::Left);
        assert_eq!(next, PaneId(2));
    }

    #[test]
    fn directional_focus_uses_center_distance_as_tiebreaker() {
        let mut panes = BTreeMap::new();
        panes.insert(PaneId(1), make_pane("current"));
        panes.insert(PaneId(2), make_pane("down-near"));
        panes.insert(PaneId(3), make_pane("down-far"));

        let mut rects = BTreeMap::new();
        rects.insert(
            PaneId(1),
            Rect {
                x: 10,
                y: 10,
                width: 10,
                height: 10,
            },
        );
        rects.insert(
            PaneId(2),
            Rect {
                x: 8,
                y: 20,
                width: 10,
                height: 10,
            },
        );
        rects.insert(
            PaneId(3),
            Rect {
                x: 40,
                y: 20,
                width: 10,
                height: 10,
            },
        );

        let next = focus_in_direction(PaneId(1), &panes, &rects, FocusDirection::Down);
        assert_eq!(next, PaneId(2));
    }

    #[test]
    fn directional_resize_updates_matching_split() {
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
        let mut scroll_state = ScrollState::default();
        let mut internal_clipboard = None;

        let (tx, rx) = mpsc::channel();
        tx.send(RuntimeAction::ResizeUp).expect("send resize up");
        drop(tx);

        let updated = process_input_events_for_test(
            &rx,
            &mut panes,
            &pane_rects,
            &layout,
            &mut focused,
            &shutdown_requested,
            &mut force_redraw,
            &mut exit_override,
            &mut status_message,
            &mut scroll_state,
            &mut internal_clipboard,
            Instant::now(),
            Arc::new(AtomicBool::new(false)),
            10_000,
            "bmux-256color",
            ProtocolProfile::Conservative,
            None,
        )
        .expect("process input events")
        .expect("tree updated by directional resize");

        match updated.root {
            LayoutNode::Split { second, .. } => match *second {
                LayoutNode::Split { ratio, .. } => assert!((ratio - 0.45).abs() < 0.001),
                _ => panic!("expected nested split"),
            },
            _ => panic!("expected root split"),
        }
        assert!(status_message.is_none());
    }

    #[test]
    fn directional_resize_reports_noop_when_axis_missing() {
        let mut panes = BTreeMap::new();
        panes.insert(PaneId(1), make_pane("pane-1"));
        panes.insert(PaneId(2), make_pane("pane-2"));

        let mut layout = LayoutTree::two_pane(PaneId(1), PaneId(2), SplitDirection::Vertical, 0.5);
        let mut focused = PaneId(1);
        layout.focused = focused;
        let pane_rects = layout.compute_rects(120, 40);
        let shutdown_requested = Arc::new(AtomicBool::new(false));
        let mut force_redraw = false;
        let mut exit_override = None;
        let mut status_message = None;
        let mut scroll_state = ScrollState::default();
        let mut internal_clipboard = None;

        let (tx, rx) = mpsc::channel();
        tx.send(RuntimeAction::ResizeUp).expect("send resize up");
        drop(tx);

        let updated = process_input_events_for_test(
            &rx,
            &mut panes,
            &pane_rects,
            &layout,
            &mut focused,
            &shutdown_requested,
            &mut force_redraw,
            &mut exit_override,
            &mut status_message,
            &mut scroll_state,
            &mut internal_clipboard,
            Instant::now(),
            Arc::new(AtomicBool::new(false)),
            10_000,
            "bmux-256color",
            ProtocolProfile::Conservative,
            None,
        )
        .expect("process input events");

        assert!(updated.is_none());
        let message = status_message.expect("status should explain no-op");
        assert!(message.text.contains("no horizontal split"));
    }

    #[test]
    fn scroll_mode_preserves_offsets_per_pane_across_focus_changes() {
        let mut panes = BTreeMap::new();
        panes.insert(PaneId(1), make_pane("pane-1"));
        panes.insert(PaneId(2), make_pane("pane-2"));
        feed_lines(panes.get(&PaneId(1)).expect("pane 1 exists"), 80);
        feed_lines(panes.get(&PaneId(2)).expect("pane 2 exists"), 80);

        let mut layout = LayoutTree::two_pane(PaneId(1), PaneId(2), SplitDirection::Vertical, 0.5);
        let mut focused = PaneId(1);
        layout.focused = focused;
        let pane_rects = layout.compute_rects(120, 40);
        let shutdown_requested = Arc::new(AtomicBool::new(false));
        let mut force_redraw = false;
        let mut exit_override = None;
        let mut status_message = None;
        let mut scroll_state = ScrollState::default();
        let mut internal_clipboard = None;

        let (tx, rx) = mpsc::channel();
        tx.send(RuntimeAction::EnterScrollMode)
            .expect("send enter scroll mode");
        tx.send(RuntimeAction::ScrollUpPage)
            .expect("send page up pane 1");
        tx.send(RuntimeAction::FocusRight)
            .expect("send focus right");
        tx.send(RuntimeAction::ScrollUpLine)
            .expect("send line up pane 2");
        tx.send(RuntimeAction::FocusLeft).expect("send focus left");
        drop(tx);

        let updated = process_input_events_for_test(
            &rx,
            &mut panes,
            &pane_rects,
            &layout,
            &mut focused,
            &shutdown_requested,
            &mut force_redraw,
            &mut exit_override,
            &mut status_message,
            &mut scroll_state,
            &mut internal_clipboard,
            Instant::now(),
            Arc::new(AtomicBool::new(false)),
            10_000,
            "bmux-256color",
            ProtocolProfile::Conservative,
            None,
        )
        .expect("process input events");

        assert!(updated.is_none());
        assert_eq!(focused, PaneId(1));
        assert!(scroll_state.active);
        assert!(scroll_state.offsets.get(&PaneId(1)).copied().unwrap_or(0) > 0);
        assert!(scroll_state.offsets.get(&PaneId(2)).copied().unwrap_or(0) > 0);

        let pane_one_offset = panes
            .get(&PaneId(1))
            .expect("pane 1 exists")
            .state
            .parser
            .lock()
            .expect("pane parser mutex poisoned")
            .screen()
            .scrollback();
        let pane_two_offset = panes
            .get(&PaneId(2))
            .expect("pane 2 exists")
            .state
            .parser
            .lock()
            .expect("pane parser mutex poisoned")
            .screen()
            .scrollback();
        assert!(pane_one_offset > 0);
        assert!(pane_two_offset > 0);
    }

    #[test]
    fn selection_scroll_actions_move_cursor_with_page_keys() {
        let mut panes = BTreeMap::new();
        panes.insert(PaneId(1), make_pane("pane-1"));
        panes.insert(PaneId(2), make_pane("pane-2"));
        feed_lines(panes.get(&PaneId(1)).expect("pane 1 exists"), 200);

        let mut layout = LayoutTree::two_pane(PaneId(1), PaneId(2), SplitDirection::Vertical, 0.5);
        let mut focused = PaneId(1);
        layout.focused = focused;
        let pane_rects = layout.compute_rects(120, 40);
        let max_row = pane_rects
            .get(&PaneId(1))
            .expect("pane rect exists")
            .inner()
            .height
            .saturating_sub(1);
        let shutdown_requested = Arc::new(AtomicBool::new(false));
        let mut force_redraw = false;
        let mut exit_override = None;
        let mut status_message = None;
        let mut scroll_state = ScrollState::default();
        let mut internal_clipboard = None;

        let (tx, rx) = mpsc::channel();
        tx.send(RuntimeAction::EnterScrollMode)
            .expect("send enter scroll mode");
        tx.send(RuntimeAction::BeginSelection)
            .expect("send begin selection");
        tx.send(RuntimeAction::ScrollUpPage)
            .expect("send scroll up page");
        tx.send(RuntimeAction::ScrollDownPage)
            .expect("send scroll down page");
        drop(tx);

        let updated = process_input_events_for_test(
            &rx,
            &mut panes,
            &pane_rects,
            &layout,
            &mut focused,
            &shutdown_requested,
            &mut force_redraw,
            &mut exit_override,
            &mut status_message,
            &mut scroll_state,
            &mut internal_clipboard,
            Instant::now(),
            Arc::new(AtomicBool::new(false)),
            10_000,
            "bmux-256color",
            ProtocolProfile::Conservative,
            None,
        )
        .expect("process input events");

        assert!(updated.is_none());
        assert!(scroll_state.selection_anchors.contains_key(&PaneId(1)));
        assert_eq!(
            scroll_state.cursors.get(&PaneId(1)).map(|(row, _col)| *row),
            Some(max_row)
        );
    }

    #[test]
    fn move_cursor_up_prefers_viewport_before_scrolling() {
        let mut panes = BTreeMap::new();
        panes.insert(PaneId(1), make_pane("pane-1"));
        panes.insert(PaneId(2), make_pane("pane-2"));
        feed_lines(panes.get(&PaneId(1)).expect("pane 1 exists"), 200);

        let mut layout = LayoutTree::two_pane(PaneId(1), PaneId(2), SplitDirection::Vertical, 0.5);
        let mut focused = PaneId(1);
        layout.focused = focused;
        let pane_rects = layout.compute_rects(120, 40);
        let shutdown_requested = Arc::new(AtomicBool::new(false));
        let mut force_redraw = false;
        let mut exit_override = None;
        let mut status_message = None;
        let mut scroll_state = ScrollState::default();
        let mut internal_clipboard = None;

        let (tx, rx) = mpsc::channel();
        tx.send(RuntimeAction::EnterScrollMode)
            .expect("send enter scroll mode");
        tx.send(RuntimeAction::ScrollUpPage)
            .expect("scroll up to create offset");
        for _ in 0..80 {
            tx.send(RuntimeAction::MoveCursorUp)
                .expect("move cursor up until top and then scroll");
        }
        drop(tx);

        let _ = process_input_events_for_test(
            &rx,
            &mut panes,
            &pane_rects,
            &layout,
            &mut focused,
            &shutdown_requested,
            &mut force_redraw,
            &mut exit_override,
            &mut status_message,
            &mut scroll_state,
            &mut internal_clipboard,
            Instant::now(),
            Arc::new(AtomicBool::new(false)),
            10_000,
            "bmux-256color",
            ProtocolProfile::Conservative,
            None,
        )
        .expect("process input events");

        assert!(scroll_state.active);
        assert_eq!(scroll_state.cursors.get(&PaneId(1)).copied(), Some((0, 0)));
        assert!(scroll_state.offsets.get(&PaneId(1)).copied().unwrap_or(0) > 0);
    }

    #[test]
    fn quit_prompts_for_confirmation_when_enabled() {
        let mut panes = BTreeMap::new();
        panes.insert(PaneId(1), make_pane("pane-1"));
        panes.insert(PaneId(2), make_pane("pane-2"));

        let mut layout = LayoutTree::two_pane(PaneId(1), PaneId(2), SplitDirection::Vertical, 0.5);
        let mut focused = PaneId(1);
        layout.focused = focused;
        let pane_rects = layout.compute_rects(120, 40);
        let shutdown_requested = Arc::new(AtomicBool::new(false));
        let mut force_redraw = false;
        let mut exit_override = None;
        let mut status_message = None;
        let mut scroll_state = ScrollState::default();
        let mut internal_clipboard = None;
        let mut pending_destroy_confirm = false;
        let mut destroy_state_requested = false;
        let mut detach_requested = false;

        let (tx, rx) = mpsc::channel();
        tx.send(RuntimeAction::Quit).expect("send quit action");

        let _ = process_input_events(
            &rx,
            &mut panes,
            &pane_rects,
            &layout,
            &mut focused,
            &shutdown_requested,
            true,
            &mut pending_destroy_confirm,
            &mut destroy_state_requested,
            &mut detach_requested,
            &mut force_redraw,
            &mut exit_override,
            &mut status_message,
            &mut scroll_state,
            &mut internal_clipboard,
            Instant::now(),
            Arc::new(AtomicBool::new(false)),
            10_000,
            "bmux-256color",
            ProtocolProfile::Conservative,
            None,
        )
        .expect("process input events");

        assert!(!shutdown_requested.load(std::sync::atomic::Ordering::Relaxed));
        assert!(pending_destroy_confirm);
        assert!(!destroy_state_requested);
        assert_eq!(
            status_message.map(|msg| msg.text),
            Some("destroy persisted state and quit? [y/N]".to_string())
        );
    }

    #[test]
    fn quit_confirmation_accepts_y_and_sets_destroy_flag() {
        let mut panes = BTreeMap::new();
        panes.insert(PaneId(1), make_pane("pane-1"));
        panes.insert(PaneId(2), make_pane("pane-2"));

        let mut layout = LayoutTree::two_pane(PaneId(1), PaneId(2), SplitDirection::Vertical, 0.5);
        let mut focused = PaneId(1);
        layout.focused = focused;
        let pane_rects = layout.compute_rects(120, 40);
        let shutdown_requested = Arc::new(AtomicBool::new(false));
        let mut force_redraw = false;
        let mut exit_override = None;
        let mut status_message = None;
        let mut scroll_state = ScrollState::default();
        let mut internal_clipboard = None;
        let mut pending_destroy_confirm = false;
        let mut destroy_state_requested = false;
        let mut detach_requested = false;

        let (tx, rx) = mpsc::channel();
        tx.send(RuntimeAction::Quit).expect("send quit action");
        tx.send(RuntimeAction::ForwardToPane(vec![b'y']))
            .expect("send confirmation byte");
        drop(tx);

        let _ = process_input_events(
            &rx,
            &mut panes,
            &pane_rects,
            &layout,
            &mut focused,
            &shutdown_requested,
            true,
            &mut pending_destroy_confirm,
            &mut destroy_state_requested,
            &mut detach_requested,
            &mut force_redraw,
            &mut exit_override,
            &mut status_message,
            &mut scroll_state,
            &mut internal_clipboard,
            Instant::now(),
            Arc::new(AtomicBool::new(false)),
            10_000,
            "bmux-256color",
            ProtocolProfile::Conservative,
            None,
        )
        .expect("process input events");

        assert!(shutdown_requested.load(std::sync::atomic::Ordering::Relaxed));
        assert!(!pending_destroy_confirm);
        assert!(destroy_state_requested);
        assert_eq!(exit_override, Some(0));
    }
}
