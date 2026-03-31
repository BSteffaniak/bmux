use super::super::*;

pub(crate) async fn run_session_attach_with_client(
    mut client: BmuxClient,
    target: Option<&str>,
    follow: Option<&str>,
    global: bool,
    capture_plan: Option<AttachDisplayCapturePlan>,
) -> Result<u8> {
    if target.is_none() && follow.is_none() {
        anyhow::bail!("attach requires a session target or --follow <client-uuid>");
    }
    if target.is_some() && follow.is_some() {
        anyhow::bail!("attach accepts either a session target or --follow, not both");
    }

    let follow_target_id = match follow {
        Some(follow_target) => Some(parse_uuid_value(follow_target, "follow target client id")?),
        None => None,
    };

    let attach_config = match BmuxConfig::load() {
        Ok(config) => config,
        Err(error) => {
            eprintln!(
                "bmux warning: failed loading config for attach keymap, using defaults ({error})"
            );
            BmuxConfig::default()
        }
    };
    let attach_keymap = attach_keymap_from_config(&attach_config);
    let attach_help_lines = build_attach_help_lines(&attach_config);

    if let Some(leader_client_id) = follow_target_id {
        client
            .subscribe_events()
            .await
            .map_err(map_attach_client_error)?;
        client
            .follow_client(leader_client_id, global)
            .await
            .map_err(map_attach_client_error)?;
    }

    let self_client_id = client.whoami().await.map_err(map_attach_client_error)?;
    let mut display_capture = recording::DisplayCaptureWriter::new(capture_plan, self_client_id)?;

    let attach_info = if let Some(leader_client_id) = follow_target_id {
        let context_id = resolve_follow_target_context(&mut client, leader_client_id)
            .await
            .map_err(map_attach_client_error)?;
        open_attach_for_context(&mut client, context_id)
            .await
            .map_err(map_attach_client_error)?
    } else {
        let target = target.expect("target is present when not follow");
        let grant = client
            .attach_grant(parse_session_selector(target))
            .await
            .map_err(map_attach_client_error)?;
        client
            .open_attach_stream_info(&grant)
            .await
            .map_err(map_attach_client_error)?
    };

    if let Some(leader_client_id) = follow_target_id {
        println!(
            "attached to session: {} (following {}{})",
            attach_info.session_id,
            leader_client_id,
            if global { ", global" } else { "" }
        );
    } else {
        println!("attached to session: {}", attach_info.session_id);
    }

    let mut view_state = AttachViewState::new(attach_info);
    view_state.mouse.config = attach_config.attach_mouse_config();

    update_attach_viewport(&mut client, view_state.attached_id).await?;
    hydrate_attach_state_from_snapshot(&mut client, &mut view_state).await?;
    view_state.set_transient_status(
        initial_attach_status(&attach_keymap, view_state.can_write),
        Instant::now(),
        ATTACH_WELCOME_STATUS_TTL,
    );

    if !view_state.can_write {
        println!("read-only attach: input disabled");
    }
    if let Some(detach_key) = attach_keymap.primary_binding_for_action(&RuntimeAction::Detach) {
        println!("press {detach_key} to detach");
    } else {
        println!("detach is unbound in current keymap");
    }
    client
        .subscribe_events()
        .await
        .map_err(map_attach_client_error)?;
    let _ = client
        .poll_events(256)
        .await
        .map_err(map_attach_client_error)?;

    let raw_mode_guard = RawModeGuard::enable(
        attach_config.behavior.kitty_keyboard,
        attach_config.attach_mouse_config().enabled,
    )
    .context("failed to enable raw mode for attach")?;
    let mut attach_input_processor =
        InputProcessor::new(attach_keymap.clone(), raw_mode_guard.keyboard_enhanced);
    let mut exit_reason = AttachExitReason::Detached;

    loop {
        let server_events = client
            .poll_events(16)
            .await
            .map_err(map_attach_client_error)?;
        let terminal_event = poll_attach_terminal_event(ATTACH_IO_POLL_INTERVAL).await?;
        let loop_events = collect_attach_loop_events(server_events, terminal_event);
        let mut should_break = false;
        for loop_event in loop_events {
            if let AttachLoopEvent::Terminal(Event::Resize(cols, rows)) = loop_event
                && let Some(capture) = display_capture.as_mut()
            {
                let _ = capture.record_resize(cols, rows);
            }
            match handle_attach_loop_event(
                loop_event,
                &mut client,
                &mut attach_input_processor,
                follow_target_id,
                Some(self_client_id),
                global,
                &attach_help_lines,
                &mut view_state,
            )
            .await?
            {
                AttachLoopControl::Continue => {}
                AttachLoopControl::Break(reason) => {
                    exit_reason = reason;
                    should_break = true;
                    break;
                }
            }
        }

        if should_break {
            break;
        }

        let _ = view_state.clear_expired_transient_status(Instant::now());
        if let Err(error) =
            refresh_attached_session_from_context(&mut client, &mut view_state).await
        {
            view_state.set_transient_status(
                format!(
                    "context refresh delayed: {}",
                    map_attach_client_error(error)
                ),
                Instant::now(),
                ATTACH_TRANSIENT_STATUS_TTL,
            );
        }

        let mut frame_needs_render = view_state.dirty.status_needs_redraw
            || view_state.dirty.full_pane_redraw
            || !view_state.dirty.pane_dirty_ids.is_empty();

        if view_state.dirty.layout_needs_refresh || view_state.cached_layout_state.is_none() {
            let previous_layout = view_state.cached_layout_state.clone();
            let layout_state = match client.attach_layout(view_state.attached_id).await {
                Ok(state) => state,
                Err(error) if is_attach_stream_closed_error(&error) => {
                    exit_reason = AttachExitReason::StreamClosed;
                    break;
                }
                Err(error) => return Err(map_attach_client_error(error)),
            };
            if view_state.cached_layout_state.as_ref() != Some(&layout_state) {
                frame_needs_render = true;
                let pane_ids = visible_scene_pane_ids(&layout_state.scene);
                for pane_id in pane_ids {
                    view_state.dirty.pane_dirty_ids.insert(pane_id);
                }
                match previous_layout {
                    None => {
                        view_state.dirty.full_pane_redraw = true;
                    }
                    Some(previous) => {
                        if previous.scene != layout_state.scene {
                            view_state.dirty.full_pane_redraw = true;
                        } else if previous.focused_pane_id != layout_state.focused_pane_id {
                            view_state
                                .dirty
                                .pane_dirty_ids
                                .insert(previous.focused_pane_id);
                            view_state
                                .dirty
                                .pane_dirty_ids
                                .insert(layout_state.focused_pane_id);
                        }
                    }
                }
                view_state.mouse.last_focused_pane_id = Some(layout_state.focused_pane_id);
                view_state.cached_layout_state = Some(layout_state);
            }
            view_state.dirty.layout_needs_refresh = false;
        }

        let Some(layout_state) = view_state.cached_layout_state.clone() else {
            continue;
        };

        resize_attach_parsers_for_scene(&mut view_state.pane_buffers, &layout_state.scene);

        let pane_ids = visible_scene_pane_ids(&layout_state.scene);
        view_state
            .pane_buffers
            .retain(|pane_id, _| pane_ids.iter().any(|id| id == pane_id));

        let chunks = match client
            .attach_pane_output_batch(view_state.attached_id, pane_ids.clone(), 8 * 1024)
            .await
        {
            Ok(chunks) => chunks,
            Err(error) if is_attach_stream_closed_error(&error) => {
                exit_reason = AttachExitReason::StreamClosed;
                break;
            }
            Err(error) => return Err(map_attach_client_error(error)),
        };

        for chunk in chunks {
            if chunk.data.is_empty() {
                continue;
            }
            let buffer = view_state.pane_buffers.entry(chunk.pane_id).or_default();
            append_pane_output(buffer, &chunk.data);
            view_state.dirty.pane_dirty_ids.insert(chunk.pane_id);
            frame_needs_render = true;
        }

        if !frame_needs_render {
            continue;
        }

        let help_scroll = view_state.help_overlay_scroll;
        render_attach_frame(
            &mut client,
            &mut view_state,
            &layout_state,
            follow_target_id,
            global,
            &attach_keymap,
            &attach_help_lines,
            help_scroll,
            display_capture.as_mut(),
        )
        .await?;
    }

    drop(raw_mode_guard);
    restore_terminal_after_attach_ui()?;

    let _ = client.detach().await;
    if follow_target_id.is_some() {
        let _ = client.unfollow().await;
    }
    if let Some(message) = attach_exit_message(exit_reason) {
        println!("{message}");
    }
    if let Some(capture) = display_capture.as_mut() {
        let _ = capture.record_stream_closed();
        let _ = capture.flush();
    }
    Ok(0)
}

pub(crate) async fn handle_attach_runtime_action(
    client: &mut BmuxClient,
    action: RuntimeAction,
    view_state: &mut AttachViewState,
) -> std::result::Result<(), ClientError> {
    match action {
        RuntimeAction::NewWindow | RuntimeAction::NewSession => {
            let context = client
                .create_context(None, std::collections::BTreeMap::new())
                .await?;
            let attach_info = open_attach_for_context(client, context.id).await?;
            view_state.attached_id = attach_info.session_id;
            view_state.attached_context_id = attach_info.context_id.or(Some(context.id));
            view_state.can_write = attach_info.can_write;
            update_attach_viewport(client, view_state.attached_id).await?;
            hydrate_attach_state_from_snapshot(client, view_state).await?;
            let status = attach_context_status(
                client,
                view_state.attached_context_id,
                view_state.attached_id,
            )
            .await?;
            set_attach_context_status(
                view_state,
                status,
                Instant::now(),
                ATTACH_WELCOME_STATUS_TTL,
            );
            if !view_state.can_write {
                println!("read-only attach: input disabled");
            }
        }
        _ => {}
    }

    Ok(())
}

pub(crate) async fn apply_plugin_command_outcome(
    client: &mut BmuxClient,
    view_state: &mut AttachViewState,
    outcome: PluginCommandOutcome,
) -> std::result::Result<bool, ClientError> {
    let mut applied = false;
    trace!(
        effect_count = outcome.effects.len(),
        attached_context_id = ?view_state.attached_context_id,
        attached_session_id = %view_state.attached_id,
        "attach.plugin_outcome.received"
    );
    for effect in outcome.effects {
        match effect {
            PluginCommandEffect::SelectContext { context_id } => {
                debug!(
                    target_context_id = %context_id,
                    attached_context_id = ?view_state.attached_context_id,
                    attached_session_id = %view_state.attached_id,
                    "attach.plugin_outcome.select_context"
                );
                retarget_attach_to_context(client, view_state, context_id).await?;
                applied = true;
            }
        }
    }
    Ok(applied)
}

pub(crate) async fn retarget_attach_to_context(
    client: &mut BmuxClient,
    view_state: &mut AttachViewState,
    context_id: Uuid,
) -> std::result::Result<(), ClientError> {
    let started_at = Instant::now();
    debug!(
        from_context_id = ?view_state.attached_context_id,
        from_session_id = %view_state.attached_id,
        to_context_id = %context_id,
        "attach.retarget.start"
    );
    let _ = client
        .select_context(ContextSelector::ById(context_id))
        .await?;
    let attach_info = open_attach_for_context(client, context_id).await?;
    view_state.attached_id = attach_info.session_id;
    view_state.attached_context_id = attach_info.context_id.or(Some(context_id));
    view_state.can_write = attach_info.can_write;
    update_attach_viewport(client, view_state.attached_id).await?;
    hydrate_attach_state_from_snapshot(client, view_state).await?;
    view_state.ui_mode = AttachUiMode::Normal;
    let status = attach_context_status(
        client,
        view_state.attached_context_id,
        view_state.attached_id,
    )
    .await?;
    set_attach_context_status(
        view_state,
        status,
        Instant::now(),
        ATTACH_TRANSIENT_STATUS_TTL,
    );
    debug!(
        to_context_id = ?view_state.attached_context_id,
        to_session_id = %view_state.attached_id,
        can_write = view_state.can_write,
        elapsed_ms = started_at.elapsed().as_millis(),
        "attach.retarget.done"
    );
    Ok(())
}

pub(crate) fn plugin_fallback_retarget_context_id(
    before_context_id: Option<Uuid>,
    after_context_id: Option<Uuid>,
    attached_context_id: Option<Uuid>,
    outcome_applied: bool,
) -> Option<Uuid> {
    if outcome_applied {
        return None;
    }
    after_context_id
        .filter(|after| Some(*after) != before_context_id && Some(*after) != attached_context_id)
}

pub(crate) fn plugin_fallback_new_context_id(
    before_context_ids: Option<&std::collections::BTreeSet<Uuid>>,
    after_context_ids: Option<&std::collections::BTreeSet<Uuid>>,
    attached_context_id: Option<Uuid>,
    after_context_id: Option<Uuid>,
    outcome_applied: bool,
) -> Option<Uuid> {
    if outcome_applied {
        return None;
    }
    let (Some(before), Some(after)) = (before_context_ids, after_context_ids) else {
        return None;
    };

    let mut new_context_ids = after
        .difference(before)
        .copied()
        .filter(|context_id| Some(*context_id) != attached_context_id)
        .collect::<Vec<_>>();

    if new_context_ids.is_empty() {
        return None;
    }
    if new_context_ids.len() == 1 {
        return new_context_ids.pop();
    }

    after_context_id.filter(|context_id| new_context_ids.contains(context_id))
}

pub(crate) async fn handle_attach_plugin_command_action(
    client: &mut BmuxClient,
    plugin_id: &str,
    command_name: &str,
    view_state: &mut AttachViewState,
) -> std::result::Result<(), ClientError> {
    let before_context_id = match client.current_context().await {
        Ok(context) => context.map(|entry| entry.id),
        Err(_) => None,
    };
    let before_context_ids = client.list_contexts().await.ok().map(|contexts| {
        contexts
            .into_iter()
            .map(|context| context.id)
            .collect::<std::collections::BTreeSet<_>>()
    });
    debug!(
        plugin_id = %plugin_id,
        command_name = %command_name,
        before_context_id = ?before_context_id,
        attached_context_id = ?view_state.attached_context_id,
        attached_session_id = %view_state.attached_id,
        "attach.plugin_command.start"
    );
    match run_plugin_keybinding_command(plugin_id, command_name, &[]) {
        Err(error) => {
            warn!(
                plugin_id = %plugin_id,
                command_name = %command_name,
                error = %error,
                "attach.plugin_command.run_failed"
            );
            view_state.set_transient_status(
                format!("plugin action failed: {error}"),
                Instant::now(),
                ATTACH_TRANSIENT_STATUS_TTL,
            );
        }
        Ok(execution) => {
            let status = execution.status;
            let effect_count = execution.outcome.effects.len();
            if status != 0 {
                warn!(
                    plugin_id = %plugin_id,
                    command_name = %command_name,
                    status,
                    effect_count,
                    before_context_id = ?before_context_id,
                    attached_context_id = ?view_state.attached_context_id,
                    attached_session_id = %view_state.attached_id,
                    "attach.plugin_command.nonzero_status"
                );
                view_state.set_transient_status(
                    format!("plugin action failed ({plugin_id}:{command_name}) exit {status}"),
                    Instant::now(),
                    ATTACH_TRANSIENT_STATUS_TTL,
                );
                return Ok(());
            }

            let outcome_applied =
                match apply_plugin_command_outcome(client, view_state, execution.outcome).await {
                    Ok(applied) => applied,
                    Err(error) => {
                        view_state.set_transient_status(
                            format!(
                                "plugin outcome apply failed: {}",
                                map_attach_client_error(error)
                            ),
                            Instant::now(),
                            ATTACH_TRANSIENT_STATUS_TTL,
                        );
                        return Ok(());
                    }
                };

            let after_context_id = match client.current_context().await {
                Ok(context) => context.map(|entry| entry.id),
                Err(_) => None,
            };
            let after_context_ids = client.list_contexts().await.ok().map(|contexts| {
                contexts
                    .into_iter()
                    .map(|context| context.id)
                    .collect::<std::collections::BTreeSet<_>>()
            });
            debug!(
                plugin_id = %plugin_id,
                command_name = %command_name,
                effect_count,
                outcome_applied,
                before_context_id = ?before_context_id,
                after_context_id = ?after_context_id,
                attached_context_id = ?view_state.attached_context_id,
                attached_session_id = %view_state.attached_id,
                "attach.plugin_command.outcome"
            );

            if let Some(fallback_context_id) = plugin_fallback_retarget_context_id(
                before_context_id,
                after_context_id,
                view_state.attached_context_id,
                outcome_applied,
            ) {
                debug!(
                    plugin_id = %plugin_id,
                    command_name = %command_name,
                    fallback_context_id = %fallback_context_id,
                    "attach.plugin_command.fallback_retarget"
                );
                if let Err(error) =
                    retarget_attach_to_context(client, view_state, fallback_context_id).await
                {
                    warn!(
                        plugin_id = %plugin_id,
                        command_name = %command_name,
                        fallback_context_id = %fallback_context_id,
                        error = %error,
                        "attach.plugin_command.fallback_retarget_failed"
                    );
                    view_state.set_transient_status(
                        format!(
                            "plugin fallback retarget failed: {}",
                            map_attach_client_error(error)
                        ),
                        Instant::now(),
                        ATTACH_TRANSIENT_STATUS_TTL,
                    );
                    return Ok(());
                }
                view_state.set_transient_status(
                    format!("plugin action: {plugin_id}:{command_name} (fallback retarget)"),
                    Instant::now(),
                    ATTACH_TRANSIENT_STATUS_TTL,
                );
                view_state.dirty.layout_needs_refresh = true;
                view_state.dirty.full_pane_redraw = true;
                return Ok(());
            }

            if let Some(fallback_context_id) = plugin_fallback_new_context_id(
                before_context_ids.as_ref(),
                after_context_ids.as_ref(),
                view_state.attached_context_id,
                after_context_id,
                outcome_applied,
            ) {
                debug!(
                    plugin_id = %plugin_id,
                    command_name = %command_name,
                    fallback_context_id = %fallback_context_id,
                    "attach.plugin_command.new_context_fallback_retarget"
                );
                if let Err(error) =
                    retarget_attach_to_context(client, view_state, fallback_context_id).await
                {
                    warn!(
                        plugin_id = %plugin_id,
                        command_name = %command_name,
                        fallback_context_id = %fallback_context_id,
                        error = %error,
                        "attach.plugin_command.new_context_fallback_retarget_failed"
                    );
                    view_state.set_transient_status(
                        format!(
                            "plugin new-context fallback failed: {}",
                            map_attach_client_error(error)
                        ),
                        Instant::now(),
                        ATTACH_TRANSIENT_STATUS_TTL,
                    );
                    return Ok(());
                }
                view_state.set_transient_status(
                    format!("plugin action: {plugin_id}:{command_name} (new context retarget)"),
                    Instant::now(),
                    ATTACH_TRANSIENT_STATUS_TTL,
                );
                view_state.dirty.layout_needs_refresh = true;
                view_state.dirty.full_pane_redraw = true;
                return Ok(());
            }

            view_state.set_transient_status(
                format!("plugin action: {plugin_id}:{command_name}"),
                Instant::now(),
                ATTACH_TRANSIENT_STATUS_TTL,
            );
            view_state.dirty.layout_needs_refresh = true;
            view_state.dirty.full_pane_redraw = true;
        }
    }

    Ok(())
}

pub(crate) async fn handle_attach_ui_action(
    client: &mut BmuxClient,
    action: RuntimeAction,
    view_state: &mut AttachViewState,
) -> std::result::Result<(), ClientError> {
    match action {
        RuntimeAction::EnterWindowMode => {
            view_state.set_transient_status(
                "workspace mode unavailable in core baseline",
                Instant::now(),
                ATTACH_TRANSIENT_STATUS_TTL,
            );
        }
        RuntimeAction::ExitMode => {
            let _ = view_state;
        }
        RuntimeAction::EnterScrollMode => {
            if enter_attach_scrollback(view_state) {
            } else {
                view_state.set_transient_status(
                    ATTACH_SCROLLBACK_UNAVAILABLE_STATUS,
                    Instant::now(),
                    ATTACH_TRANSIENT_STATUS_TTL,
                );
            }
        }
        RuntimeAction::ExitScrollMode => {
            if view_state.selection_active() {
                clear_attach_selection(view_state, true);
            } else {
                view_state.exit_scrollback();
            }
        }
        RuntimeAction::ScrollUpLine => {
            step_attach_scrollback(view_state, -1);
        }
        RuntimeAction::ScrollDownLine => {
            step_attach_scrollback(view_state, 1);
        }
        RuntimeAction::ScrollUpPage => {
            step_attach_scrollback(
                view_state,
                -(attach_scrollback_page_size(view_state) as isize),
            );
        }
        RuntimeAction::ScrollDownPage => {
            step_attach_scrollback(view_state, attach_scrollback_page_size(view_state) as isize);
        }
        RuntimeAction::ScrollTop => {
            if view_state.scrollback_active {
                view_state.scrollback_offset = max_attach_scrollback(view_state);
                clamp_attach_scrollback_cursor(view_state);
            }
        }
        RuntimeAction::ScrollBottom => {
            if view_state.scrollback_active {
                view_state.scrollback_offset = 0;
                clamp_attach_scrollback_cursor(view_state);
            }
        }
        RuntimeAction::MoveCursorLeft => {
            move_attach_scrollback_cursor_horizontal(view_state, -1);
        }
        RuntimeAction::MoveCursorRight => {
            move_attach_scrollback_cursor_horizontal(view_state, 1);
        }
        RuntimeAction::MoveCursorUp => {
            move_attach_scrollback_cursor_vertical(view_state, -1);
        }
        RuntimeAction::MoveCursorDown => {
            move_attach_scrollback_cursor_vertical(view_state, 1);
        }
        RuntimeAction::BeginSelection => {
            if begin_attach_selection(view_state) {
                view_state.set_transient_status(
                    ATTACH_SELECTION_STARTED_STATUS,
                    Instant::now(),
                    ATTACH_TRANSIENT_STATUS_TTL,
                );
            }
        }
        RuntimeAction::CopyScrollback => {
            copy_attach_selection(view_state, false);
        }
        RuntimeAction::ConfirmScrollback => {
            confirm_attach_scrollback(view_state);
        }
        RuntimeAction::SessionPrev => {
            view_state.exit_scrollback();
            switch_attach_session_relative(client, view_state, -1).await?;
            let status = attach_context_status(
                client,
                view_state.attached_context_id,
                view_state.attached_id,
            )
            .await?;
            set_attach_context_status(
                view_state,
                status,
                Instant::now(),
                ATTACH_TRANSIENT_STATUS_TTL,
            );
        }
        RuntimeAction::SessionNext => {
            view_state.exit_scrollback();
            switch_attach_session_relative(client, view_state, 1).await?;
            let status = attach_context_status(
                client,
                view_state.attached_context_id,
                view_state.attached_id,
            )
            .await?;
            set_attach_context_status(
                view_state,
                status,
                Instant::now(),
                ATTACH_TRANSIENT_STATUS_TTL,
            );
        }
        RuntimeAction::WindowPrev => {
            view_state.exit_scrollback();
        }
        RuntimeAction::WindowNext => {
            view_state.exit_scrollback();
        }
        RuntimeAction::WindowGoto1 => {
            view_state.exit_scrollback();
        }
        RuntimeAction::WindowGoto2 => {
            view_state.exit_scrollback();
        }
        RuntimeAction::WindowGoto3 => {
            view_state.exit_scrollback();
        }
        RuntimeAction::WindowGoto4 => {
            view_state.exit_scrollback();
        }
        RuntimeAction::WindowGoto5 => {
            view_state.exit_scrollback();
        }
        RuntimeAction::WindowGoto6 => {
            view_state.exit_scrollback();
        }
        RuntimeAction::WindowGoto7 => {
            view_state.exit_scrollback();
        }
        RuntimeAction::WindowGoto8 => {
            view_state.exit_scrollback();
        }
        RuntimeAction::WindowGoto9 => {
            view_state.exit_scrollback();
        }
        RuntimeAction::WindowClose => {
            view_state.exit_scrollback();
        }
        RuntimeAction::SplitFocusedVertical => {
            let selector = attached_session_selector(client, view_state).await?;
            let _ = client
                .split_pane(Some(selector), PaneSplitDirection::Vertical)
                .await?;
        }
        RuntimeAction::SplitFocusedHorizontal => {
            let selector = attached_session_selector(client, view_state).await?;
            let _ = client
                .split_pane(Some(selector), PaneSplitDirection::Horizontal)
                .await?;
        }
        RuntimeAction::FocusNext
        | RuntimeAction::FocusLeft
        | RuntimeAction::FocusRight
        | RuntimeAction::FocusUp
        | RuntimeAction::FocusDown => {
            let direction = if matches!(action, RuntimeAction::FocusLeft | RuntimeAction::FocusUp) {
                PaneFocusDirection::Prev
            } else {
                PaneFocusDirection::Next
            };
            let selector = attached_session_selector(client, view_state).await?;
            let _ = client.focus_pane(Some(selector), direction).await?;
        }
        RuntimeAction::IncreaseSplit
        | RuntimeAction::DecreaseSplit
        | RuntimeAction::ResizeLeft
        | RuntimeAction::ResizeRight
        | RuntimeAction::ResizeUp
        | RuntimeAction::ResizeDown => {
            let delta = if matches!(
                action,
                RuntimeAction::IncreaseSplit
                    | RuntimeAction::ResizeRight
                    | RuntimeAction::ResizeDown
            ) {
                1
            } else {
                -1
            };
            let selector = attached_session_selector(client, view_state).await?;
            client.resize_pane(Some(selector), delta).await?;
        }
        RuntimeAction::CloseFocusedPane => {
            let selector = attached_session_selector(client, view_state).await?;
            client.close_pane(Some(selector)).await?;
        }
        RuntimeAction::NewWindow | RuntimeAction::NewSession => {
            handle_attach_runtime_action(client, action, view_state).await?;
        }
        _ => {}
    }

    Ok(())
}

pub(crate) fn enter_attach_scrollback(view_state: &mut AttachViewState) -> bool {
    let Some((inner_w, inner_h)) = focused_attach_pane_inner_size(view_state) else {
        return false;
    };
    let Some(buffer) = focused_attach_pane_buffer(view_state) else {
        return false;
    };
    let (row, col) = buffer.parser.screen().cursor_position();
    view_state.scrollback_active = true;
    view_state.scrollback_offset = 0;
    view_state.scrollback_cursor = Some(AttachScrollbackCursor {
        row: usize::from(row).min(inner_h.saturating_sub(1)),
        col: usize::from(col).min(inner_w.saturating_sub(1)),
    });
    view_state.selection_anchor = None;
    true
}

pub(crate) fn begin_attach_selection(view_state: &mut AttachViewState) -> bool {
    if !view_state.scrollback_active {
        return false;
    }
    view_state.selection_anchor = attach_scrollback_cursor_absolute_position(view_state);
    view_state.selection_anchor.is_some()
}

pub(crate) fn clear_attach_selection(view_state: &mut AttachViewState, show_status: bool) {
    view_state.selection_anchor = None;
    if show_status {
        view_state.set_transient_status(
            ATTACH_SELECTION_CLEARED_STATUS,
            Instant::now(),
            ATTACH_TRANSIENT_STATUS_TTL,
        );
    }
}

pub(crate) fn attach_scrollback_cursor_absolute_position(
    view_state: &AttachViewState,
) -> Option<AttachScrollbackPosition> {
    let cursor = view_state.scrollback_cursor?;
    Some(AttachScrollbackPosition {
        row: view_state.scrollback_offset.saturating_add(cursor.row),
        col: cursor.col,
    })
}

pub(crate) fn attach_selection_bounds(
    view_state: &AttachViewState,
) -> Option<(AttachScrollbackPosition, AttachScrollbackPosition)> {
    let anchor = view_state.selection_anchor?;
    let head = attach_scrollback_cursor_absolute_position(view_state)?;
    Some(if anchor <= head {
        (anchor, head)
    } else {
        (head, anchor)
    })
}

pub(crate) fn step_attach_scrollback(view_state: &mut AttachViewState, delta: isize) {
    if !view_state.scrollback_active {
        return;
    }
    let max_offset = max_attach_scrollback(view_state);
    view_state.scrollback_offset =
        adjust_attach_scrollback_offset(view_state.scrollback_offset, delta, max_offset);
    clamp_attach_scrollback_cursor(view_state);
}

pub(crate) fn move_attach_scrollback_cursor_horizontal(
    view_state: &mut AttachViewState,
    delta: isize,
) {
    if !view_state.scrollback_active {
        return;
    }
    let Some((inner_w, _)) = focused_attach_pane_inner_size(view_state) else {
        return;
    };
    let Some(cursor) = view_state.scrollback_cursor.as_mut() else {
        return;
    };
    cursor.col = adjust_scrollback_cursor_component(cursor.col, delta, inner_w.saturating_sub(1));
}

pub(crate) fn move_attach_scrollback_cursor_vertical(
    view_state: &mut AttachViewState,
    delta: isize,
) {
    if !view_state.scrollback_active || delta == 0 {
        return;
    }
    let Some((_, inner_h)) = focused_attach_pane_inner_size(view_state) else {
        return;
    };
    let max_offset = max_attach_scrollback(view_state);
    let Some(cursor) = view_state.scrollback_cursor.as_mut() else {
        return;
    };

    if delta < 0 {
        for _ in 0..delta.unsigned_abs() {
            if cursor.row > 0 {
                cursor.row -= 1;
            } else if view_state.scrollback_offset < max_offset {
                view_state.scrollback_offset += 1;
            }
        }
    } else {
        for _ in 0..(delta as usize) {
            if cursor.row + 1 < inner_h {
                cursor.row += 1;
            } else if view_state.scrollback_offset > 0 {
                view_state.scrollback_offset -= 1;
            }
        }
    }

    clamp_attach_scrollback_cursor(view_state);
}

pub(crate) fn adjust_scrollback_cursor_component(
    current: usize,
    delta: isize,
    max_value: usize,
) -> usize {
    if delta < 0 {
        current.saturating_sub(delta.unsigned_abs())
    } else {
        current.saturating_add(delta as usize).min(max_value)
    }
}

pub(crate) fn copy_attach_selection(view_state: &mut AttachViewState, exit_after_copy: bool) {
    let Some(text) = selected_attach_text(view_state) else {
        if exit_after_copy {
            view_state.exit_scrollback();
        } else {
            view_state.set_transient_status(
                ATTACH_SELECTION_EMPTY_STATUS,
                Instant::now(),
                ATTACH_TRANSIENT_STATUS_TTL,
            );
        }
        return;
    };

    match copy_text_with_clipboard_plugin(&text) {
        Ok(()) => {
            view_state.set_transient_status(
                ATTACH_SELECTION_COPIED_STATUS,
                Instant::now(),
                ATTACH_TRANSIENT_STATUS_TTL,
            );
            if exit_after_copy {
                view_state.exit_scrollback();
            }
        }
        Err(error) => {
            view_state.set_transient_status(
                format_clipboard_service_error(&error),
                Instant::now(),
                ATTACH_TRANSIENT_STATUS_TTL,
            );
        }
    }
}

pub(crate) fn confirm_attach_scrollback(view_state: &mut AttachViewState) {
    copy_attach_selection(view_state, true);
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct ClipboardWriteRequest {
    text: String,
}

pub(crate) fn copy_text_with_clipboard_plugin(text: &str) -> Result<()> {
    let config = BmuxConfig::load()?;
    let paths = ConfigPaths::default();
    let registry = scan_available_plugins(&config, &paths)?;
    let services = available_service_descriptors(&config, &registry)?;
    let capability = HostScope::new("bmux.clipboard.write")?;
    let service = services
        .into_iter()
        .find(|entry| {
            entry.capability == capability
                && entry.kind == ServiceKind::Command
                && entry.interface_id == "clipboard-write/v1"
        })
        .context("clipboard service unavailable; ensure a provider is enabled and discoverable")?;

    let provider_plugin_id = match &service.provider {
        bmux_plugin_sdk::ProviderId::Plugin(plugin_id) => plugin_id,
        bmux_plugin_sdk::ProviderId::Host => {
            anyhow::bail!("clipboard service provider must be plugin-owned")
        }
    };
    let provider = registry.get(provider_plugin_id).with_context(|| {
        format!("clipboard service provider '{provider_plugin_id}' was not found")
    })?;

    let payload = bmux_plugin_sdk::encode_service_message(&ClipboardWriteRequest {
        text: text.to_string(),
    })?;
    let enabled_plugins = effective_enabled_plugins(&config, &registry);
    let available_capabilities = available_capability_providers(&config, &registry)?
        .into_keys()
        .map(|entry| entry.to_string())
        .collect::<Vec<_>>();
    let plugin_search_roots = resolve_plugin_search_paths(&config, &paths)?
        .into_iter()
        .map(|path| path.to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    let loaded = load_plugin(
        provider,
        &plugin_host_metadata(),
        &available_capability_providers(&config, &registry)?,
    )
    .with_context(|| format!("failed loading clipboard service provider '{provider_plugin_id}'"))?;

    let connection = bmux_plugin_sdk::HostConnectionInfo {
        config_dir: paths.config_dir.to_string_lossy().into_owned(),
        runtime_dir: paths.runtime_dir.to_string_lossy().into_owned(),
        data_dir: paths.data_dir.to_string_lossy().into_owned(),
        state_dir: paths.state_dir.to_string_lossy().into_owned(),
    };
    let _host_kernel_connection_guard = enter_host_kernel_connection(connection.clone());
    let response = loaded.invoke_service(&bmux_plugin_sdk::NativeServiceContext {
        plugin_id: provider_plugin_id.clone(),
        request: ServiceRequest {
            caller_plugin_id: "bmux.core".to_string(),
            service,
            operation: "copy_text".to_string(),
            payload,
        },
        required_capabilities: provider
            .declaration
            .required_capabilities
            .iter()
            .map(ToString::to_string)
            .collect(),
        provided_capabilities: provider
            .declaration
            .provided_capabilities
            .iter()
            .map(ToString::to_string)
            .collect(),
        services: available_service_descriptors(&config, &registry)?,
        available_capabilities,
        enabled_plugins,
        plugin_search_roots,
        host: plugin_host_metadata(),
        connection,
        settings: None,
        plugin_settings_map: std::collections::BTreeMap::new(),
        host_kernel_bridge: Some(bmux_plugin_sdk::HostKernelBridge::from_fn(
            host_kernel_bridge,
        )),
    })?;
    if let Some(error) = response.error {
        anyhow::bail!(error.message);
    }

    let _: () = bmux_plugin_sdk::decode_service_message(&response.payload)
        .context("failed decoding clipboard service response payload")?;
    Ok(())
}

pub(crate) fn format_clipboard_service_error(error: &anyhow::Error) -> String {
    let message = error.to_string();
    if message.contains("clipboard backend unavailable") {
        return "clipboard backend unavailable".to_string();
    }
    if message.starts_with("clipboard copy failed:") {
        return message;
    }
    format!("clipboard copy failed: {message}")
}

pub(crate) fn selected_attach_text(view_state: &mut AttachViewState) -> Option<String> {
    let (start, end) = attach_selection_bounds(view_state)?;
    extract_attach_text(view_state, start, end)
}

pub(crate) fn extract_attach_text(
    view_state: &mut AttachViewState,
    start: AttachScrollbackPosition,
    end: AttachScrollbackPosition,
) -> Option<String> {
    let buffer = focused_attach_pane_buffer(view_state)?;
    let original_scrollback = buffer.parser.screen().scrollback();
    buffer.parser.screen_mut().set_scrollback(start.row);
    let text = buffer.parser.screen().contents_between(
        0,
        start.col as u16,
        end.row.saturating_sub(start.row) as u16,
        end.col.saturating_add(1) as u16,
    );
    buffer
        .parser
        .screen_mut()
        .set_scrollback(original_scrollback);
    Some(text)
}

pub(crate) fn adjust_attach_scrollback_offset(
    current: usize,
    delta: isize,
    max_offset: usize,
) -> usize {
    if delta < 0 {
        current.saturating_add(delta.unsigned_abs()).min(max_offset)
    } else {
        current.saturating_sub(delta as usize)
    }
}

pub(crate) fn max_attach_scrollback(view_state: &mut AttachViewState) -> usize {
    let Some(buffer) = focused_attach_pane_buffer(view_state) else {
        return 0;
    };
    let previous = buffer.parser.screen().scrollback();
    buffer.parser.screen_mut().set_scrollback(usize::MAX);
    let max_offset = buffer.parser.screen().scrollback();
    buffer.parser.screen_mut().set_scrollback(previous);
    max_offset
}

pub(crate) fn clamp_attach_scrollback_cursor(view_state: &mut AttachViewState) {
    let Some((inner_w, inner_h)) = focused_attach_pane_inner_size(view_state) else {
        view_state.scrollback_cursor = None;
        return;
    };
    let Some(cursor) = view_state.scrollback_cursor.as_mut() else {
        return;
    };
    cursor.row = cursor.row.min(inner_h.saturating_sub(1));
    cursor.col = cursor.col.min(inner_w.saturating_sub(1));
}

pub(crate) fn attach_scrollback_page_size(view_state: &AttachViewState) -> usize {
    focused_attach_pane_inner_size(view_state).map_or(10, |(_, inner_h)| inner_h)
}

pub(crate) fn focused_attach_pane_buffer(
    view_state: &mut AttachViewState,
) -> Option<&mut attach::state::PaneRenderBuffer> {
    let focused_pane_id = view_state.cached_layout_state.as_ref()?.focused_pane_id;
    view_state.pane_buffers.get_mut(&focused_pane_id)
}

pub(crate) fn focused_attach_pane_inner_size(
    view_state: &AttachViewState,
) -> Option<(usize, usize)> {
    let layout_state = view_state.cached_layout_state.as_ref()?;
    layout_state
        .scene
        .surfaces
        .iter()
        .find(|surface| surface.visible && surface.pane_id == Some(layout_state.focused_pane_id))
        .map(|surface| {
            (
                usize::from(surface.rect.w.saturating_sub(2).max(1)),
                usize::from(surface.rect.h.saturating_sub(2).max(1)),
            )
        })
}

pub(crate) async fn switch_attach_session_relative(
    client: &mut BmuxClient,
    view_state: &mut AttachViewState,
    step: isize,
) -> std::result::Result<(), ClientError> {
    if let Some(current_context_id) = view_state.attached_context_id {
        let contexts = client.list_contexts().await?;
        if let Some(target_context_id) = relative_context_id(&contexts, current_context_id, step) {
            let _ = client
                .select_context(ContextSelector::ById(target_context_id))
                .await?;
            let attach_info = open_attach_for_context(client, target_context_id).await?;
            view_state.attached_id = attach_info.session_id;
            view_state.attached_context_id = attach_info.context_id.or(Some(target_context_id));
            view_state.can_write = attach_info.can_write;
            update_attach_viewport(client, view_state.attached_id).await?;
            hydrate_attach_state_from_snapshot(client, view_state).await?;
            return Ok(());
        }
    }

    let sessions = client.list_sessions().await?;
    let Some(target_session_id) = relative_session_id(&sessions, view_state.attached_id, step)
    else {
        return Ok(());
    };

    let attach_info = open_attach_for_session(client, target_session_id).await?;
    view_state.attached_id = attach_info.session_id;
    view_state.attached_context_id = attach_info.context_id;
    view_state.can_write = attach_info.can_write;
    update_attach_viewport(client, view_state.attached_id).await?;
    hydrate_attach_state_from_snapshot(client, view_state).await?;
    Ok(())
}

pub(crate) fn relative_session_id(
    sessions: &[SessionSummary],
    current_session_id: Uuid,
    step: isize,
) -> Option<Uuid> {
    if sessions.is_empty() {
        return None;
    }

    let current_index = sessions
        .iter()
        .position(|session| session.id == current_session_id)
        .unwrap_or(0);
    let len = sessions.len() as isize;
    let mut target_index = current_index as isize + step;
    while target_index < 0 {
        target_index += len;
    }
    target_index %= len;
    sessions
        .get(target_index as usize)
        .map(|session| session.id)
}

pub(crate) fn relative_context_id(
    contexts: &[ContextSummary],
    current_context_id: Uuid,
    step: isize,
) -> Option<Uuid> {
    if contexts.is_empty() {
        return None;
    }

    let current_index = contexts
        .iter()
        .position(|context| context.id == current_context_id)
        .unwrap_or(0);
    let len = contexts.len() as isize;
    let mut target_index = current_index as isize + step;
    while target_index < 0 {
        target_index += len;
    }
    target_index %= len;
    contexts
        .get(target_index as usize)
        .map(|context| context.id)
}

pub(crate) async fn build_attach_status_line_for_draw(
    client: &mut BmuxClient,
    context_id: Option<Uuid>,
    session_id: Uuid,
    can_write: bool,
    ui_mode: AttachUiMode,
    scrollback_active: bool,
    follow_target_id: Option<Uuid>,
    follow_global: bool,
    quit_confirmation_pending: bool,
    help_overlay_open: bool,
    transient_status: Option<&str>,
    keymap: &Keymap,
) -> std::result::Result<String, ClientError> {
    let (cols, _) = terminal::size().unwrap_or((0, 0));
    if cols == 0 {
        return Ok(String::new());
    }

    let tabs = build_attach_tabs(client, context_id, session_id).await?;
    let session_label = resolve_attach_session_label(client, session_id).await?;
    let current_context_label =
        resolve_attach_context_label(client, context_id, session_id).await?;
    let mode_label = if help_overlay_open {
        "HELP"
    } else if scrollback_active {
        "SCROLL"
    } else {
        let _ = ui_mode;
        "NORMAL"
    };
    let role_label = if can_write { "write" } else { "read-only" };
    let follow_label = follow_target_id.map(|id| {
        if follow_global {
            format!("following {} (global)", short_uuid(id))
        } else {
            format!("following {}", short_uuid(id))
        }
    });
    let hint = if quit_confirmation_pending {
        "Quit session and all panes? [y/N]".to_string()
    } else if help_overlay_open {
        "Help overlay open | ? toggles | Esc/Enter close".to_string()
    } else if let Some(status) = transient_status {
        status.to_string()
    } else if scrollback_active {
        attach_scrollback_hint(keymap)
    } else {
        attach_mode_hint(ui_mode, keymap)
    };

    let status_line = build_attach_status_line(
        &session_label,
        &current_context_label,
        &tabs,
        mode_label,
        role_label,
        follow_label.as_deref(),
        &hint,
    );

    Ok(format_status_line_for_width(&status_line, cols))
}

pub(crate) fn format_status_line_for_width(status_line: &str, cols: u16) -> String {
    let width = usize::from(cols);
    let mut rendered = status_line.to_string();
    if rendered.len() > width {
        rendered.truncate(width);
    } else {
        rendered.push_str(&" ".repeat(width - rendered.len()));
    }
    rendered
}

pub(crate) fn attach_mode_hint(_ui_mode: AttachUiMode, keymap: &Keymap) -> String {
    let detach = key_hint_or_unbound(keymap, RuntimeAction::Detach);
    let quit = key_hint_or_unbound(keymap, RuntimeAction::Quit);
    let help = key_hint_or_unbound(keymap, RuntimeAction::ShowHelp);
    format!("{detach} detach | {quit} quit | {help} help")
}

pub(crate) fn initial_attach_status(keymap: &Keymap, can_write: bool) -> String {
    let help = key_hint_or_unbound(keymap, RuntimeAction::ShowHelp);
    if can_write {
        format!("{help} help | typing goes to pane")
    } else {
        format!("read-only attach | {help} help")
    }
}

pub(crate) const fn attach_exit_message(reason: AttachExitReason) -> Option<&'static str> {
    match reason {
        AttachExitReason::Detached | AttachExitReason::Quit => None,
        AttachExitReason::StreamClosed => Some("attach ended unexpectedly: server stream closed"),
    }
}

pub(crate) fn attach_scrollback_hint(keymap: &Keymap) -> String {
    let exit = scroll_key_hint_or_unbound(keymap, RuntimeAction::ExitScrollMode);
    let confirm = scroll_key_hint_or_unbound(keymap, RuntimeAction::ConfirmScrollback);
    let left = scroll_key_hint_or_unbound(keymap, RuntimeAction::MoveCursorLeft);
    let right = scroll_key_hint_or_unbound(keymap, RuntimeAction::MoveCursorRight);
    let up = scroll_key_hint_or_unbound(keymap, RuntimeAction::MoveCursorUp);
    let down = scroll_key_hint_or_unbound(keymap, RuntimeAction::MoveCursorDown);
    let page_up = scroll_key_hint_or_unbound(keymap, RuntimeAction::ScrollUpPage);
    let page_down = scroll_key_hint_or_unbound(keymap, RuntimeAction::ScrollDownPage);
    let top = scroll_key_hint_or_unbound(keymap, RuntimeAction::ScrollTop);
    let bottom = scroll_key_hint_or_unbound(keymap, RuntimeAction::ScrollBottom);
    let select = scroll_key_hint_or_unbound(keymap, RuntimeAction::BeginSelection);
    let copy = scroll_key_hint_or_unbound(keymap, RuntimeAction::CopyScrollback);
    format!(
        "{up}/{down} line | {left}/{right} col | {page_up}/{page_down} page | {top}/{bottom} top/bottom | {select} select | {copy} copy | {confirm} copy+exit | {exit} cancel/exit scroll"
    )
}

pub(crate) fn scroll_key_hint_or_unbound(keymap: &Keymap, action: RuntimeAction) -> String {
    keymap
        .primary_scroll_binding_for_action(&action)
        .unwrap_or_else(|| "unbound".to_string())
}

pub(crate) fn key_hint_or_unbound(keymap: &Keymap, action: RuntimeAction) -> String {
    keymap
        .primary_binding_for_action(&action)
        .unwrap_or_else(|| "unbound".to_string())
}

pub(crate) fn queue_attach_status_line(stdout: &mut impl Write, status_line: &str) -> Result<()> {
    let (cols, rows) = terminal::size().unwrap_or((0, 0));
    if cols == 0 || rows == 0 {
        return Ok(());
    }
    let rendered = format_status_line_for_width(status_line, cols);
    queue!(
        stdout,
        MoveTo(0, 0),
        Print("\x1b[7m"),
        Print(rendered),
        Print("\x1b[0m")
    )
    .context("failed queuing attach status line")
}

pub(crate) fn help_overlay_visible_rows(lines: &[String]) -> usize {
    let (_cols, rows) = terminal::size().unwrap_or((0, 0));
    let max_content_rows = (rows as usize).saturating_sub(6);
    let content_rows = lines.len().min(max_content_rows);
    let height = (content_rows + 4).min((rows as usize).saturating_sub(2));
    height.saturating_sub(4).max(1)
}

pub(crate) fn adjust_help_overlay_scroll(
    current: usize,
    delta: isize,
    total_lines: usize,
    visible_rows: usize,
) -> usize {
    if total_lines == 0 {
        return 0;
    }
    let max_scroll = total_lines.saturating_sub(visible_rows.max(1));
    let next = if delta.is_negative() {
        current.saturating_sub(delta.unsigned_abs())
    } else {
        current.saturating_add(delta as usize)
    };
    next.min(max_scroll)
}

pub(crate) const fn help_overlay_accepts_key_kind(kind: KeyEventKind) -> bool {
    matches!(kind, KeyEventKind::Press | KeyEventKind::Repeat)
}

pub(crate) fn handle_help_overlay_key_event(
    key: &KeyEvent,
    help_lines: &[String],
    view_state: &mut AttachViewState,
) -> bool {
    if !help_overlay_accepts_key_kind(key.kind) {
        return false;
    }

    match key.code {
        KeyCode::Esc | KeyCode::Enter => {
            view_state.help_overlay_open = false;
            view_state.help_overlay_scroll = 0;
            view_state.dirty.status_needs_redraw = true;
            view_state.dirty.full_pane_redraw = true;
            true
        }
        KeyCode::Up | KeyCode::Char('k') => {
            view_state.help_overlay_scroll = adjust_help_overlay_scroll(
                view_state.help_overlay_scroll,
                -1,
                help_lines.len(),
                help_overlay_visible_rows(help_lines),
            );
            view_state.dirty.full_pane_redraw = true;
            true
        }
        KeyCode::Down | KeyCode::Char('j') => {
            view_state.help_overlay_scroll = adjust_help_overlay_scroll(
                view_state.help_overlay_scroll,
                1,
                help_lines.len(),
                help_overlay_visible_rows(help_lines),
            );
            view_state.dirty.full_pane_redraw = true;
            true
        }
        KeyCode::PageUp => {
            let page = help_overlay_visible_rows(help_lines) as isize;
            view_state.help_overlay_scroll = adjust_help_overlay_scroll(
                view_state.help_overlay_scroll,
                -page,
                help_lines.len(),
                help_overlay_visible_rows(help_lines),
            );
            view_state.dirty.full_pane_redraw = true;
            true
        }
        KeyCode::PageDown => {
            let page = help_overlay_visible_rows(help_lines) as isize;
            view_state.help_overlay_scroll = adjust_help_overlay_scroll(
                view_state.help_overlay_scroll,
                page,
                help_lines.len(),
                help_overlay_visible_rows(help_lines),
            );
            view_state.dirty.full_pane_redraw = true;
            true
        }
        KeyCode::Home => {
            view_state.help_overlay_scroll = 0;
            view_state.dirty.full_pane_redraw = true;
            true
        }
        KeyCode::End => {
            let visible = help_overlay_visible_rows(help_lines);
            view_state.help_overlay_scroll = help_lines.len().saturating_sub(visible);
            view_state.dirty.full_pane_redraw = true;
            true
        }
        _ => false,
    }
}

pub(crate) fn help_overlay_surface(lines: &[String]) -> Option<bmux_ipc::AttachSurface> {
    let (cols, rows) = terminal::size().unwrap_or((0, 0));
    if cols < 20 || rows < 6 {
        return None;
    }

    let content_width = lines
        .iter()
        .map(std::string::String::len)
        .max()
        .unwrap_or(0)
        .min(80);
    let width = (content_width + 4)
        .max(36)
        .min((cols as usize).saturating_sub(2));
    let max_content_rows = (rows as usize).saturating_sub(6);
    let content_rows = lines.len().min(max_content_rows);
    let height = (content_rows + 4).min((rows as usize).saturating_sub(2));
    let x = ((cols as usize).saturating_sub(width)) / 2;
    let y = ((rows as usize).saturating_sub(height)) / 2;

    Some(bmux_ipc::AttachSurface {
        id: HELP_OVERLAY_SURFACE_ID,
        kind: bmux_ipc::AttachSurfaceKind::Overlay,
        layer: bmux_ipc::AttachLayer::Overlay,
        z: i32::MAX,
        rect: bmux_ipc::AttachRect {
            x: x as u16,
            y: y as u16,
            w: width as u16,
            h: height as u16,
        },
        opaque: true,
        visible: true,
        accepts_input: true,
        cursor_owner: false,
        pane_id: None,
    })
}

pub(crate) fn queue_attach_help_overlay(
    stdout: &mut impl Write,
    surface_meta: &bmux_ipc::AttachSurface,
    lines: &[String],
    scroll: usize,
) -> Result<()> {
    let width = usize::from(surface_meta.rect.w);
    let height = usize::from(surface_meta.rect.h);
    let x = usize::from(surface_meta.rect.x);
    let y = usize::from(surface_meta.rect.y);
    let body_rows = height.saturating_sub(4).max(1);
    let surface = AttachLayerSurface::new(
        PaneRect {
            x: surface_meta.rect.x,
            y: surface_meta.rect.y,
            w: surface_meta.rect.w,
            h: surface_meta.rect.h,
        },
        AttachLayer::Overlay,
        true,
    );
    let text_width = width.saturating_sub(4);

    let top = format!("+{}+", "-".repeat(width.saturating_sub(2)));
    queue!(stdout, MoveTo(x as u16, y as u16), Print(&top))
        .context("failed drawing help overlay top")?;

    let title = " bmux help ";
    let title_x = x + ((width.saturating_sub(title.len())) / 2);
    queue!(stdout, MoveTo(title_x as u16, y as u16), Print(title))
        .context("failed drawing help overlay title")?;

    for row in 1..height.saturating_sub(1) {
        let y_row = (y + row) as u16;
        queue!(
            stdout,
            MoveTo(x as u16, y_row),
            Print("|"),
            MoveTo((x + width - 1) as u16, y_row),
            Print("|")
        )
        .context("failed drawing help overlay border")?;
    }

    queue_layer_fill(stdout, surface).context("failed filling help overlay body")?;

    queue!(
        stdout,
        MoveTo(x as u16, (y + height - 1) as u16),
        Print(&top)
    )
    .context("failed drawing help overlay bottom")?;

    let header = "scope    chord                action";
    let header_rendered = opaque_row_text(header, text_width);
    queue!(
        stdout,
        MoveTo((x + 2) as u16, (y + 1) as u16),
        Print(header_rendered)
    )
    .context("failed drawing help overlay header")?;

    let start = scroll.min(lines.len().saturating_sub(body_rows));
    let end = (start + body_rows).min(lines.len());
    for (idx, line) in lines.iter().skip(start).take(body_rows).enumerate() {
        let rendered = opaque_row_text(line, text_width);
        let row = y + 2 + idx;
        if row >= y + height - 1 {
            break;
        }
        queue!(stdout, MoveTo((x + 2) as u16, row as u16), Print(rendered))
            .context("failed drawing help overlay entry")?;
    }

    let footer = format!(
        "j/k or ↑/↓ scroll | PgUp/PgDn | Esc close | {}-{} / {}",
        if lines.is_empty() { 0 } else { start + 1 },
        end,
        lines.len()
    );
    let footer_rendered = opaque_row_text(&footer, text_width);
    queue!(
        stdout,
        MoveTo((x + 2) as u16, (y + height - 2) as u16),
        Print(footer_rendered)
    )
    .context("failed drawing help overlay footer")?;

    Ok(())
}

pub(crate) async fn render_attach_frame(
    client: &mut BmuxClient,
    view_state: &mut AttachViewState,
    layout_state: &AttachLayoutState,
    follow_target_id: Option<Uuid>,
    follow_global: bool,
    keymap: &crate::input::Keymap,
    help_lines: &[String],
    help_scroll: usize,
    display_capture: Option<&mut recording::DisplayCaptureWriter>,
) -> Result<()> {
    if view_state.dirty.status_needs_redraw {
        let now = Instant::now();
        view_state.cached_status_line = Some(
            build_attach_status_line_for_draw(
                client,
                view_state.attached_context_id,
                view_state.attached_id,
                view_state.can_write,
                view_state.ui_mode,
                view_state.scrollback_active,
                follow_target_id,
                follow_global,
                view_state.quit_confirmation_pending,
                view_state.help_overlay_open,
                view_state.transient_status_text(now),
                keymap,
            )
            .await
            .map_err(map_attach_client_error)?,
        );
        view_state.dirty.status_needs_redraw = false;
    }

    let mut frame_bytes = Vec::new();
    queue!(frame_bytes, SavePosition).context("failed queuing cursor save for attach frame")?;
    if let Some(status_line) = view_state.cached_status_line.as_deref() {
        queue_attach_status_line(&mut frame_bytes, status_line)?;
    }
    let cursor_state = render_attach_scene(
        &mut frame_bytes,
        &layout_state.scene,
        &mut view_state.pane_buffers,
        &view_state.dirty.pane_dirty_ids,
        view_state.dirty.full_pane_redraw,
        view_state.scrollback_active,
        view_state.scrollback_offset,
        view_state.scrollback_cursor,
        view_state.selection_anchor,
    )?;
    if view_state.help_overlay_open {
        if let Some(help_surface) = help_overlay_surface(help_lines) {
            queue_attach_help_overlay(&mut frame_bytes, &help_surface, help_lines, help_scroll)?;
        }
        apply_attach_cursor_state(&mut frame_bytes, None, &mut view_state.last_cursor_state)?;
    } else {
        apply_attach_cursor_state(
            &mut frame_bytes,
            cursor_state,
            &mut view_state.last_cursor_state,
        )?;
    }

    if let Some(capture) = display_capture {
        let _ = capture.record_frame_bytes(&frame_bytes);
    }

    let mut stdout = io::stdout();
    stdout
        .write_all(&frame_bytes)
        .context("failed writing attach frame")?;
    stdout.flush().context("failed flushing attach frame")?;
    view_state.dirty.full_pane_redraw = false;
    view_state.dirty.pane_dirty_ids.clear();
    Ok(())
}

pub(crate) async fn build_attach_tabs(
    client: &mut BmuxClient,
    context_id: Option<Uuid>,
    session_id: Uuid,
) -> std::result::Result<Vec<AttachTab>, ClientError> {
    let contexts = client.list_contexts().await?;
    if contexts.is_empty() {
        return Ok(vec![AttachTab {
            label: "terminal".to_string(),
            active: true,
        }]);
    }

    let current_context_id = context_id.or_else(|| {
        contexts
            .iter()
            .find(|context| {
                context
                    .attributes
                    .get("bmux.session_id")
                    .is_some_and(|value| value == &session_id.to_string())
            })
            .map(|context| context.id)
    });

    let tabs = contexts
        .into_iter()
        .take(6)
        .map(|context| AttachTab {
            label: context_summary_label(&context),
            active: current_context_id == Some(context.id),
        })
        .collect();
    Ok(tabs)
}

pub(crate) async fn resolve_attach_context_label(
    client: &mut BmuxClient,
    context_id: Option<Uuid>,
    session_id: Uuid,
) -> std::result::Result<String, ClientError> {
    let contexts = client.list_contexts().await?;
    if let Some(context_id) = context_id
        && let Some(context) = contexts.iter().find(|context| context.id == context_id)
    {
        return Ok(context_summary_label(context));
    }

    if let Some(context) = contexts.iter().find(|context| {
        context
            .attributes
            .get("bmux.session_id")
            .is_some_and(|value| value == &session_id.to_string())
    }) {
        return Ok(context_summary_label(context));
    }

    Ok("terminal".to_string())
}

pub(crate) fn context_summary_label(context: &ContextSummary) -> String {
    context
        .name
        .as_deref()
        .filter(|name| !name.trim().is_empty())
        .map_or_else(
            || format!("context-{}", short_uuid(context.id)),
            ToString::to_string,
        )
}

pub(crate) async fn resolve_attach_session_label(
    client: &mut BmuxClient,
    session_id: Uuid,
) -> std::result::Result<String, ClientError> {
    let sessions = client.list_sessions().await?;
    Ok(sessions
        .into_iter()
        .find(|session| session.id == session_id)
        .map_or_else(
            || format!("session-{}", short_uuid(session_id)),
            |session| session_summary_label(&session),
        ))
}

pub(crate) fn session_summary_label(session: &bmux_ipc::SessionSummary) -> String {
    session
        .name
        .clone()
        .unwrap_or_else(|| format!("session-{}", short_uuid(session.id)))
}

pub(crate) async fn attach_context_status(
    client: &mut BmuxClient,
    context_id: Option<Uuid>,
    session_id: Uuid,
) -> std::result::Result<String, ClientError> {
    let session_label = resolve_attach_session_label(client, session_id).await?;
    let context_label = resolve_attach_context_label(client, context_id, session_id).await?;
    Ok(format!(
        "session: {session_label} | context: {context_label}"
    ))
}

pub(crate) fn set_attach_context_status(
    view_state: &mut AttachViewState,
    status: String,
    now: Instant,
    ttl: Duration,
) {
    view_state.set_transient_status(status, now, ttl);
}

pub(crate) fn short_uuid(id: Uuid) -> String {
    id.to_string().chars().take(8).collect()
}

pub(crate) async fn resolve_follow_target_context(
    client: &mut BmuxClient,
    leader_client_id: Uuid,
) -> std::result::Result<Uuid, ClientError> {
    let clients = client.list_clients().await?;
    let leader = clients
        .into_iter()
        .find(|entry| entry.id == leader_client_id)
        .ok_or(ClientError::UnexpectedResponse("follow target not found"))?;

    if let Some(context_id) = leader.selected_context_id {
        return Ok(context_id);
    }

    if let Some(session_id) = leader.selected_session_id {
        let contexts = client.list_contexts().await?;
        if let Some(context) = contexts.into_iter().find(|context| {
            context
                .attributes
                .get("bmux.session_id")
                .is_some_and(|value| value == &session_id.to_string())
        }) {
            return Ok(context.id);
        }
    }

    Err(ClientError::UnexpectedResponse(
        "follow target has no selected context",
    ))
}

pub(crate) async fn open_attach_for_session(
    client: &mut BmuxClient,
    session_id: Uuid,
) -> std::result::Result<bmux_client::AttachOpenInfo, ClientError> {
    let grant = client
        .attach_grant(SessionSelector::ById(session_id))
        .await?;
    client.open_attach_stream_info(&grant).await
}

pub(crate) async fn open_attach_for_context(
    client: &mut BmuxClient,
    context_id: Uuid,
) -> std::result::Result<bmux_client::AttachOpenInfo, ClientError> {
    let grant = client
        .attach_context_grant(ContextSelector::ById(context_id))
        .await?;
    client.open_attach_stream_info(&grant).await
}

pub(crate) async fn attached_session_selector(
    client: &mut BmuxClient,
    view_state: &mut AttachViewState,
) -> std::result::Result<SessionSelector, ClientError> {
    refresh_attached_session_from_context(client, view_state).await?;
    Ok(SessionSelector::ById(view_state.attached_id))
}

pub(crate) async fn refresh_attached_session_from_context(
    client: &mut BmuxClient,
    view_state: &mut AttachViewState,
) -> std::result::Result<(), ClientError> {
    if let Some(context_id) = view_state.attached_context_id {
        trace!(
            context_id = %context_id,
            current_session_id = %view_state.attached_id,
            "attach.context_refresh.start"
        );
        let started_at = Instant::now();
        let grant = client
            .attach_context_grant(ContextSelector::ById(context_id))
            .await?;
        let previous_session_id = view_state.attached_id;
        view_state.attached_id = grant.session_id;
        view_state.attached_context_id = grant.context_id.or(Some(context_id));
        trace!(
            context_id = ?view_state.attached_context_id,
            previous_session_id = %previous_session_id,
            refreshed_session_id = %view_state.attached_id,
            elapsed_ms = started_at.elapsed().as_millis(),
            "attach.context_refresh.done"
        );
    }
    Ok(())
}

pub(crate) fn attach_keymap_from_config(config: &BmuxConfig) -> crate::input::Keymap {
    let (runtime_bindings, global_bindings, scroll_bindings) = filtered_attach_keybindings(config);
    let timeout_ms = config
        .keybindings
        .resolve_timeout()
        .map(|timeout| timeout.timeout_ms())
        .unwrap_or(None);
    match crate::input::Keymap::from_parts_with_scroll(
        &config.keybindings.prefix,
        timeout_ms,
        &runtime_bindings,
        &global_bindings,
        &scroll_bindings,
    ) {
        Ok(keymap) => keymap,
        Err(error) => {
            eprintln!("bmux warning: invalid attach keymap config, using defaults ({error})");
            default_attach_keymap()
        }
    }
}

pub(crate) fn filtered_attach_keybindings(
    config: &BmuxConfig,
) -> (
    std::collections::BTreeMap<String, String>,
    std::collections::BTreeMap<String, String>,
    std::collections::BTreeMap<String, String>,
) {
    let (runtime, global, scroll) = merged_runtime_keybindings(config);
    let runtime = normalize_attach_keybindings(runtime, "runtime");
    let mut global = normalize_attach_keybindings(global, "global");
    let scroll = normalize_attach_keybindings(scroll, "scroll");

    inject_attach_global_defaults(&mut global);
    (runtime, global, scroll)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AttachKeybindingScope {
    Runtime,
    Global,
}

impl AttachKeybindingScope {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Runtime => "runtime",
            Self::Global => "global",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AttachKeybindingEntry {
    pub(crate) scope: AttachKeybindingScope,
    pub(crate) chord: String,
    pub(crate) action: RuntimeAction,
    pub(crate) action_name: String,
}

pub(crate) fn effective_attach_keybindings(config: &BmuxConfig) -> Vec<AttachKeybindingEntry> {
    let (runtime, global, _) = filtered_attach_keybindings(config);
    let mut entries = Vec::new();

    for (chord, action_name) in runtime {
        if let Ok(action) = crate::input::parse_runtime_action_name(&action_name) {
            entries.push(AttachKeybindingEntry {
                scope: AttachKeybindingScope::Runtime,
                chord,
                action,
                action_name,
            });
        }
    }
    for (chord, action_name) in global {
        if let Ok(action) = crate::input::parse_runtime_action_name(&action_name) {
            entries.push(AttachKeybindingEntry {
                scope: AttachKeybindingScope::Global,
                chord,
                action,
                action_name,
            });
        }
    }

    entries.sort_by(|left, right| {
        left.scope
            .as_str()
            .cmp(right.scope.as_str())
            .then_with(|| left.chord.cmp(&right.chord))
    });
    entries
}

pub(crate) fn build_attach_help_lines(config: &BmuxConfig) -> Vec<String> {
    let keymap = attach_keymap_from_config(config);
    let help = key_hint_or_unbound(&keymap, RuntimeAction::ShowHelp);
    let detach = key_hint_or_unbound(&keymap, RuntimeAction::Detach);
    let scroll = key_hint_or_unbound(&keymap, RuntimeAction::EnterScrollMode);
    let mut groups: Vec<(&str, Vec<AttachKeybindingEntry>)> = vec![
        ("Session", Vec::new()),
        ("Pane", Vec::new()),
        ("Mode", Vec::new()),
        ("Other", Vec::new()),
    ];

    for entry in effective_attach_keybindings(config) {
        let category = match entry.action {
            RuntimeAction::NewSession
            | RuntimeAction::SessionPrev
            | RuntimeAction::SessionNext
            | RuntimeAction::Detach
            | RuntimeAction::Quit => "Session",
            RuntimeAction::NewWindow
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
            | RuntimeAction::WindowClose => "Other",
            RuntimeAction::SplitFocusedVertical
            | RuntimeAction::SplitFocusedHorizontal
            | RuntimeAction::FocusNext
            | RuntimeAction::FocusLeft
            | RuntimeAction::FocusRight
            | RuntimeAction::FocusUp
            | RuntimeAction::FocusDown
            | RuntimeAction::IncreaseSplit
            | RuntimeAction::DecreaseSplit
            | RuntimeAction::ResizeLeft
            | RuntimeAction::ResizeRight
            | RuntimeAction::ResizeUp
            | RuntimeAction::ResizeDown
            | RuntimeAction::CloseFocusedPane => "Pane",
            RuntimeAction::EnterWindowMode
            | RuntimeAction::ExitMode
            | RuntimeAction::EnterScrollMode
            | RuntimeAction::ExitScrollMode
            | RuntimeAction::ScrollUpLine
            | RuntimeAction::ScrollDownLine
            | RuntimeAction::ScrollUpPage
            | RuntimeAction::ScrollDownPage
            | RuntimeAction::ScrollTop
            | RuntimeAction::ScrollBottom
            | RuntimeAction::BeginSelection
            | RuntimeAction::CopyScrollback
            | RuntimeAction::ConfirmScrollback
            | RuntimeAction::ShowHelp => "Mode",
            _ => "Other",
        };

        if let Some((_, entries)) = groups.iter_mut().find(|(name, _)| *name == category) {
            entries.push(entry);
        }
    }

    let mut lines = Vec::new();
    lines.push("Attach Help".to_string());
    lines.push(format!(
        "Normal mode sends typing to the pane. Use {scroll} for scrollback, {detach} to detach, and {help} to toggle help."
    ));
    lines.push(String::new());
    for (category, mut entries) in groups {
        if entries.is_empty() {
            continue;
        }
        entries.sort_by(|left, right| {
            left.scope
                .as_str()
                .cmp(right.scope.as_str())
                .then_with(|| left.chord.cmp(&right.chord))
        });
        lines.push(format!("-- {category} --"));
        for entry in entries {
            lines.push(format!(
                "[{:<7}] {:<20} {}",
                entry.scope.as_str(),
                entry.chord,
                entry.action_name
            ));
        }
        lines.push(String::new());
    }

    if lines.last().is_some_and(String::is_empty) {
        let _ = lines.pop();
    }
    lines
}

pub(crate) fn normalize_attach_keybindings(
    bindings: std::collections::BTreeMap<String, String>,
    scope: &str,
) -> std::collections::BTreeMap<String, String> {
    bindings
        .into_iter()
        .filter_map(
            |(chord, action_name)| match crate::input::parse_runtime_action_name(&action_name) {
                Ok(action) if is_attach_runtime_action(&action) => {
                    Some((chord, action_to_config_name(&action)))
                }
                Ok(_) => None,
                Err(error) => {
                    eprintln!(
                        "bmux warning: dropping invalid {scope} keybinding '{chord}' -> '{action_name}' ({error})"
                    );
                    None
                }
            },
        )
        .collect()
}

pub(crate) fn inject_attach_global_defaults(
    global: &mut std::collections::BTreeMap<String, String>,
) {
    let defaults = [
        ("alt+h", RuntimeAction::SessionPrev),
        ("alt+l", RuntimeAction::SessionNext),
    ];

    for (key, action) in defaults {
        global
            .entry(key.to_string())
            .or_insert_with(|| action_to_config_name(&action));
    }
}

pub(crate) const fn is_attach_runtime_action(action: &RuntimeAction) -> bool {
    matches!(
        action,
        RuntimeAction::Detach
            | RuntimeAction::Quit
            | RuntimeAction::NewWindow
            | RuntimeAction::NewSession
            | RuntimeAction::SessionPrev
            | RuntimeAction::SessionNext
            | RuntimeAction::EnterWindowMode
            | RuntimeAction::ExitMode
            | RuntimeAction::EnterScrollMode
            | RuntimeAction::ExitScrollMode
            | RuntimeAction::ScrollUpLine
            | RuntimeAction::ScrollDownLine
            | RuntimeAction::ScrollUpPage
            | RuntimeAction::ScrollDownPage
            | RuntimeAction::ScrollTop
            | RuntimeAction::ScrollBottom
            | RuntimeAction::BeginSelection
            | RuntimeAction::CopyScrollback
            | RuntimeAction::ConfirmScrollback
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
            | RuntimeAction::WindowClose
            | RuntimeAction::PluginCommand { .. }
            | RuntimeAction::SplitFocusedVertical
            | RuntimeAction::SplitFocusedHorizontal
            | RuntimeAction::FocusNext
            | RuntimeAction::FocusLeft
            | RuntimeAction::FocusRight
            | RuntimeAction::FocusUp
            | RuntimeAction::FocusDown
            | RuntimeAction::IncreaseSplit
            | RuntimeAction::DecreaseSplit
            | RuntimeAction::ResizeLeft
            | RuntimeAction::ResizeRight
            | RuntimeAction::ResizeUp
            | RuntimeAction::ResizeDown
            | RuntimeAction::CloseFocusedPane
            | RuntimeAction::ShowHelp
    )
}

pub(crate) fn default_attach_keymap() -> crate::input::Keymap {
    let defaults = BmuxConfig::default();
    let (runtime_bindings, global_bindings, scroll_bindings) =
        filtered_attach_keybindings(&defaults);
    let timeout_ms = defaults
        .keybindings
        .resolve_timeout()
        .expect("default timeout config must be valid")
        .timeout_ms();
    crate::input::Keymap::from_parts_with_scroll(
        &defaults.keybindings.prefix,
        timeout_ms,
        &runtime_bindings,
        &global_bindings,
        &scroll_bindings,
    )
    .expect("default attach keymap must be valid")
}

pub(crate) fn describe_timeout(timeout: &ResolvedTimeout) -> String {
    match timeout {
        ResolvedTimeout::Indefinite => "indefinite".to_string(),
        ResolvedTimeout::Exact(ms) => format!("exact ({ms}ms)"),
        ResolvedTimeout::Profile { name, ms } => format!("profile:{name} ({ms}ms)"),
    }
}

pub(crate) struct RawModeGuard {
    keyboard_enhanced: bool,
    mouse_capture_enabled: bool,
}

impl RawModeGuard {
    fn enable(kitty_keyboard_enabled: bool, mouse_capture_enabled: bool) -> Result<Self> {
        enable_raw_mode().context("failed enabling raw mode")?;

        #[cfg(feature = "kitty-keyboard")]
        let keyboard_enhanced = kitty_keyboard_enabled
            && crossterm::terminal::supports_keyboard_enhancement().unwrap_or(false);
        #[cfg(not(feature = "kitty-keyboard"))]
        let keyboard_enhanced = false;

        let _ = kitty_keyboard_enabled; // suppress unused warning when feature is disabled

        let mut stdout = io::stdout();
        if keyboard_enhanced {
            use crossterm::event::{KeyboardEnhancementFlags, PushKeyboardEnhancementFlags};
            queue!(
                stdout,
                PushKeyboardEnhancementFlags(
                    KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                        | KeyboardEnhancementFlags::REPORT_EVENT_TYPES
                )
            )
            .context("failed to push keyboard enhancement flags")?;
            stdout
                .flush()
                .context("failed to flush after pushing keyboard flags")?;
        }

        if mouse_capture_enabled {
            queue!(stdout, EnableMouseCapture).context("failed to enable mouse capture")?;
            stdout
                .flush()
                .context("failed to flush after enabling mouse capture")?;
        }

        Ok(Self {
            keyboard_enhanced,
            mouse_capture_enabled,
        })
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        if self.mouse_capture_enabled {
            let mut stdout = io::stdout();
            let _ = queue!(stdout, DisableMouseCapture);
            let _ = stdout.flush();
        }
        if self.keyboard_enhanced {
            use crossterm::event::PopKeyboardEnhancementFlags;
            let mut stdout = io::stdout();
            let _ = queue!(stdout, PopKeyboardEnhancementFlags);
            let _ = stdout.flush();
        }
        let _ = disable_raw_mode();
    }
}

pub(crate) async fn poll_attach_terminal_event(timeout: Duration) -> Result<Option<Event>> {
    tokio::task::spawn_blocking(move || {
        if event::poll(timeout).context("failed polling terminal events")? {
            let event = event::read().context("failed reading terminal event")?;
            return Ok(Some(event));
        }

        Ok(None)
    })
    .await
    .context("failed to join terminal event task")?
}

pub(crate) async fn update_attach_viewport(
    client: &mut BmuxClient,
    session_id: Uuid,
) -> std::result::Result<(), ClientError> {
    let (cols, rows) = terminal::size().unwrap_or((0, 0));
    if cols == 0 || rows == 0 {
        return Ok(());
    }
    client.attach_set_viewport(session_id, cols, rows).await?;
    Ok(())
}

pub(crate) async fn hydrate_attach_state_from_snapshot(
    client: &mut BmuxClient,
    view_state: &mut AttachViewState,
) -> std::result::Result<(), ClientError> {
    let AttachSnapshotState {
        context_id: _,
        session_id,
        focused_pane_id,
        panes,
        layout_root,
        scene,
        chunks,
    } = client
        .attach_snapshot(view_state.attached_id, ATTACH_SNAPSHOT_MAX_BYTES_PER_PANE)
        .await?;

    view_state.cached_layout_state = Some(AttachLayoutState {
        context_id: None,
        session_id,
        focused_pane_id,
        panes,
        layout_root,
        scene,
    });
    view_state.mouse.last_focused_pane_id = Some(focused_pane_id);
    view_state.pane_buffers.clear();
    if let Some(layout_state) = view_state.cached_layout_state.as_ref() {
        resize_attach_parsers_for_scene(&mut view_state.pane_buffers, &layout_state.scene);
    }
    for chunk in chunks {
        if chunk.data.is_empty() {
            continue;
        }
        let buffer = view_state.pane_buffers.entry(chunk.pane_id).or_default();
        append_pane_output(buffer, &chunk.data);
        view_state.dirty.pane_dirty_ids.insert(chunk.pane_id);
    }
    view_state.dirty.layout_needs_refresh = false;
    view_state.dirty.full_pane_redraw = true;
    view_state.dirty.status_needs_redraw = true;
    Ok(())
}

pub(crate) fn resize_attach_parsers_for_scene(
    pane_buffers: &mut std::collections::BTreeMap<Uuid, attach::state::PaneRenderBuffer>,
    scene: &bmux_ipc::AttachScene,
) {
    let (cols, rows) = terminal::size().unwrap_or((0, 0));
    resize_attach_parsers_for_scene_with_size(pane_buffers, scene, cols, rows);
}

pub(crate) fn resize_attach_parsers_for_scene_with_size(
    pane_buffers: &mut std::collections::BTreeMap<Uuid, attach::state::PaneRenderBuffer>,
    scene: &bmux_ipc::AttachScene,
    cols: u16,
    rows: u16,
) {
    if cols == 0 || rows <= 1 {
        return;
    }

    for surface in &scene.surfaces {
        let Some(pane_id) = surface.pane_id else {
            continue;
        };
        if !surface.visible {
            continue;
        }
        let rect = PaneRect {
            x: surface.rect.x.min(cols.saturating_sub(1)),
            y: surface.rect.y.min(rows.saturating_sub(1)),
            w: surface.rect.w.min(cols),
            h: surface
                .rect
                .h
                .min(rows.saturating_sub(surface.rect.y.min(rows.saturating_sub(1)))),
        };
        if rect.w < 2 || rect.h < 2 {
            continue;
        }
        let inner_w = rect.w.saturating_sub(2).max(1);
        let inner_h = rect.h.saturating_sub(2).max(1);
        let buffer = pane_buffers.entry(pane_id).or_default();
        buffer.parser.screen_mut().set_size(inner_h, inner_w);
    }
}

pub(crate) async fn handle_attach_loop_event(
    event: AttachLoopEvent,
    client: &mut BmuxClient,
    attach_input_processor: &mut InputProcessor,
    follow_target_id: Option<Uuid>,
    self_client_id: Option<Uuid>,
    global: bool,
    help_lines: &[String],
    view_state: &mut AttachViewState,
) -> Result<AttachLoopControl> {
    match event {
        AttachLoopEvent::Server(server_event) => {
            handle_attach_server_event(
                client,
                server_event,
                follow_target_id,
                self_client_id,
                global,
                view_state,
            )
            .await
        }
        AttachLoopEvent::Terminal(terminal_event) => {
            handle_attach_terminal_event(
                client,
                terminal_event,
                attach_input_processor,
                help_lines,
                view_state,
            )
            .await
        }
    }
}

pub(crate) async fn handle_attach_server_event(
    client: &mut BmuxClient,
    server_event: bmux_client::ServerEvent,
    follow_target_id: Option<Uuid>,
    self_client_id: Option<Uuid>,
    _global: bool,
    view_state: &mut AttachViewState,
) -> Result<AttachLoopControl> {
    if is_attach_terminal_server_exit_event(&server_event, view_state.attached_id) {
        return Ok(AttachLoopControl::Break(AttachExitReason::StreamClosed));
    }

    match server_event {
        bmux_client::ServerEvent::FollowTargetChanged {
            follower_client_id,
            leader_client_id,
            context_id,
            session_id,
        } => {
            if Some(leader_client_id) != follow_target_id
                || Some(follower_client_id) != self_client_id
            {
                return Ok(AttachLoopControl::Continue);
            }
            let attach_info = if let Some(context_id) = context_id {
                open_attach_for_context(client, context_id)
                    .await
                    .map_err(map_attach_client_error)?
            } else if view_state.attached_context_id.is_none() {
                open_attach_for_session(client, session_id)
                    .await
                    .map_err(map_attach_client_error)?
            } else {
                return Ok(AttachLoopControl::Continue);
            };
            view_state.attached_id = attach_info.session_id;
            view_state.attached_context_id = attach_info.context_id.or(context_id);
            view_state.can_write = attach_info.can_write;
            update_attach_viewport(client, view_state.attached_id).await?;
            hydrate_attach_state_from_snapshot(client, view_state)
                .await
                .map_err(map_attach_client_error)?;
            view_state.ui_mode = AttachUiMode::Normal;
            let status = attach_context_status(
                client,
                view_state.attached_context_id,
                view_state.attached_id,
            )
            .await
            .map_err(map_attach_client_error)?;
            set_attach_context_status(
                view_state,
                status,
                Instant::now(),
                ATTACH_TRANSIENT_STATUS_TTL,
            );
            if !view_state.can_write {
                println!("read-only attach: input disabled");
            }
        }
        bmux_client::ServerEvent::FollowTargetGone {
            former_leader_client_id,
            ..
        } if Some(former_leader_client_id) == follow_target_id => {
            println!("follow target disconnected; staying on current session");
        }
        bmux_client::ServerEvent::AttachViewChanged {
            context_id,
            session_id,
            components,
            ..
        } if attach_view_event_matches_target(view_state, context_id, session_id) => {
            apply_attach_view_change_components(&components, view_state);
        }
        _ => {}
    }

    Ok(AttachLoopControl::Continue)
}

pub(crate) fn apply_attach_view_change_components(
    components: &[AttachViewComponent],
    view_state: &mut AttachViewState,
) {
    // Components are applied sequentially in server-provided order so future
    // fine-grained refresh behavior can build on earlier invalidation steps
    // without re-sorting or undoing prior effects.
    for component in components {
        match component {
            AttachViewComponent::Scene => {
                view_state.dirty.layout_needs_refresh = true;
                view_state.dirty.full_pane_redraw = true;
                view_state.dirty.status_needs_redraw = true;
            }
            AttachViewComponent::SurfaceContent => {
                view_state.dirty.layout_needs_refresh = true;
                view_state.dirty.full_pane_redraw = true;
            }
            AttachViewComponent::Layout => {
                view_state.dirty.layout_needs_refresh = true;
                view_state.dirty.full_pane_redraw = true;
                view_state.dirty.status_needs_redraw = true;
            }
            AttachViewComponent::Status => {
                view_state.dirty.status_needs_redraw = true;
            }
        }
    }
}

pub(crate) fn is_attach_terminal_server_exit_event(
    event: &bmux_client::ServerEvent,
    attached_id: Uuid,
) -> bool {
    matches!(event, bmux_client::ServerEvent::SessionRemoved { id } if *id == attached_id)
}

pub(crate) fn attach_view_event_matches_target(
    view_state: &AttachViewState,
    event_context_id: Option<Uuid>,
    event_session_id: Uuid,
) -> bool {
    if let Some(attached_context_id) = view_state.attached_context_id {
        return event_context_id == Some(attached_context_id);
    }
    event_session_id == view_state.attached_id
}

pub(crate) async fn handle_attach_terminal_event(
    client: &mut BmuxClient,
    terminal_event: Event,
    attach_input_processor: &mut InputProcessor,
    help_lines: &[String],
    view_state: &mut AttachViewState,
) -> Result<AttachLoopControl> {
    if matches!(terminal_event, Event::Resize(_, _)) {
        if let Err(error) = refresh_attached_session_from_context(client, view_state).await {
            view_state.set_transient_status(
                format!(
                    "context refresh delayed: {}",
                    map_attach_client_error(error)
                ),
                Instant::now(),
                ATTACH_TRANSIENT_STATUS_TTL,
            );
        }
        update_attach_viewport(client, view_state.attached_id).await?;
    }

    let mut skip_attach_key_actions = false;
    if view_state.quit_confirmation_pending
        && let Event::Key(key) = &terminal_event
        && key.kind == KeyEventKind::Press
    {
        match key.code {
            KeyCode::Char('y' | 'Y') => {
                let selector = attached_session_selector(client, view_state).await?;
                match client.kill_session(selector).await {
                    Ok(_) => return Ok(AttachLoopControl::Break(AttachExitReason::Quit)),
                    Err(error) => {
                        let status = attach_quit_failure_status(&error);
                        view_state.set_transient_status(
                            status,
                            Instant::now(),
                            ATTACH_TRANSIENT_STATUS_TTL,
                        );
                    }
                }
                view_state.quit_confirmation_pending = false;
                view_state.dirty.status_needs_redraw = true;
                skip_attach_key_actions = true;
            }
            KeyCode::Char('n' | 'N') | KeyCode::Esc | KeyCode::Enter => {
                view_state.quit_confirmation_pending = false;
                view_state.dirty.status_needs_redraw = true;
                skip_attach_key_actions = true;
            }
            _ => {
                skip_attach_key_actions = true;
            }
        }
    }

    if skip_attach_key_actions {
        return Ok(AttachLoopControl::Continue);
    }

    if view_state.help_overlay_open
        && let Event::Key(key) = &terminal_event
        && handle_help_overlay_key_event(key, help_lines, view_state)
    {
        return Ok(AttachLoopControl::Continue);
    }

    for attach_action in
        attach_event_actions(&terminal_event, attach_input_processor, view_state.ui_mode)?
    {
        match attach_action {
            AttachEventAction::Detach => {
                return Ok(AttachLoopControl::Break(AttachExitReason::Detached));
            }
            AttachEventAction::Send(bytes) => {
                if view_state.help_overlay_open {
                    continue;
                }
                if view_state.can_write {
                    if let Err(error) =
                        refresh_attached_session_from_context(client, view_state).await
                    {
                        view_state.set_transient_status(
                            format!(
                                "context refresh delayed: {}",
                                map_attach_client_error(error)
                            ),
                            Instant::now(),
                            ATTACH_TRANSIENT_STATUS_TTL,
                        );
                    }
                    match client.attach_input(view_state.attached_id, bytes).await {
                        Ok(_) => {}
                        Err(error) if is_attach_stream_closed_error(&error) => {
                            return Ok(AttachLoopControl::Break(AttachExitReason::StreamClosed));
                        }
                        Err(error) => return Err(map_attach_client_error(error)),
                    }
                }
            }
            AttachEventAction::Runtime(action) => {
                if view_state.help_overlay_open {
                    continue;
                }
                if let Err(error) = handle_attach_runtime_action(client, action, view_state).await {
                    println!("attach action failed: {}", map_attach_client_error(error));
                } else {
                    view_state.dirty.status_needs_redraw = true;
                    view_state.dirty.layout_needs_refresh = true;
                    view_state.dirty.full_pane_redraw = true;
                }
                attach_input_processor.set_scroll_mode(view_state.scrollback_active);
            }
            AttachEventAction::PluginCommand {
                plugin_id,
                command_name,
            } => {
                if view_state.help_overlay_open {
                    continue;
                }
                if let Err(error) = handle_attach_plugin_command_action(
                    client,
                    &plugin_id,
                    &command_name,
                    view_state,
                )
                .await
                {
                    view_state.set_transient_status(
                        format!("plugin action failed: {}", map_attach_client_error(error)),
                        Instant::now(),
                        ATTACH_TRANSIENT_STATUS_TTL,
                    );
                }
                attach_input_processor.set_scroll_mode(view_state.scrollback_active);
            }
            AttachEventAction::Mouse(mouse_event) => {
                if let Err(error) = handle_attach_mouse_event(client, mouse_event, view_state).await
                {
                    view_state.set_transient_status(
                        format!("mouse action failed: {}", map_attach_client_error(error)),
                        Instant::now(),
                        ATTACH_TRANSIENT_STATUS_TTL,
                    );
                }
                attach_input_processor.set_scroll_mode(view_state.scrollback_active);
            }
            AttachEventAction::Ui(action) => {
                if matches!(action, RuntimeAction::ShowHelp) {
                    view_state.help_overlay_open = !view_state.help_overlay_open;
                    if !view_state.help_overlay_open {
                        view_state.help_overlay_scroll = 0;
                    }
                    view_state.dirty.status_needs_redraw = true;
                    view_state.dirty.full_pane_redraw = true;
                    continue;
                }
                if view_state.help_overlay_open {
                    if matches!(action, RuntimeAction::ExitMode)
                        || matches!(action, RuntimeAction::ForwardToPane(_))
                    {
                        view_state.help_overlay_open = false;
                        view_state.help_overlay_scroll = 0;
                        view_state.dirty.status_needs_redraw = true;
                        view_state.dirty.full_pane_redraw = true;
                    }
                    continue;
                }
                if matches!(action, RuntimeAction::Quit) {
                    view_state.quit_confirmation_pending = true;
                    view_state.dirty.status_needs_redraw = true;
                    continue;
                }
                if let Err(error) = handle_attach_ui_action(client, action, view_state).await {
                    println!("attach action failed: {}", map_attach_client_error(error));
                } else {
                    view_state.dirty.layout_needs_refresh = true;
                    view_state.dirty.full_pane_redraw = true;
                }
                attach_input_processor.set_scroll_mode(view_state.scrollback_active);
                view_state.dirty.status_needs_redraw = true;
            }
            AttachEventAction::Redraw => {
                view_state.dirty.status_needs_redraw = true;
                view_state.dirty.layout_needs_refresh = true;
                view_state.dirty.full_pane_redraw = true;
            }
            AttachEventAction::Ignore => {}
        }
    }

    Ok(AttachLoopControl::Continue)
}

pub(crate) fn record_attach_mouse_event(mouse_event: MouseEvent, view_state: &mut AttachViewState) {
    view_state.mouse.last_position = Some((mouse_event.column, mouse_event.row));
    view_state.mouse.last_event_at = Some(Instant::now());
}

pub(crate) async fn handle_attach_mouse_event(
    client: &mut BmuxClient,
    mouse_event: MouseEvent,
    view_state: &mut AttachViewState,
) -> std::result::Result<(), ClientError> {
    record_attach_mouse_event(mouse_event, view_state);

    if !view_state.mouse.config.enabled || !view_state.can_write {
        return Ok(());
    }
    if view_state.help_overlay_open || view_state.quit_confirmation_pending {
        return Ok(());
    }

    if matches!(mouse_event.kind, MouseEventKind::ScrollUp)
        && handle_attach_mouse_gesture_action(client, view_state, "scroll_up").await?
    {
        return Ok(());
    }
    if matches!(mouse_event.kind, MouseEventKind::ScrollDown)
        && handle_attach_mouse_gesture_action(client, view_state, "scroll_down").await?
    {
        return Ok(());
    }

    if handle_attach_mouse_scrollback(view_state, mouse_event.kind) {
        return Ok(());
    }

    match mouse_event.kind {
        MouseEventKind::Down(MouseButton::Left) if view_state.mouse.config.focus_on_click => {
            let target = attach_scene_pane_at(view_state, mouse_event.column, mouse_event.row);
            view_state.mouse.hovered_pane_id = target;
            view_state.mouse.hover_started_at = Some(Instant::now());
            if let Some(pane_id) = target {
                if !handle_attach_mouse_gesture_action(client, view_state, "click_left").await? {
                    focus_attach_pane(client, view_state, pane_id).await?;
                }
            }
        }
        MouseEventKind::Moved if view_state.mouse.config.focus_on_hover => {
            let now = Instant::now();
            let target = attach_scene_pane_at(view_state, mouse_event.column, mouse_event.row);
            if target != view_state.mouse.hovered_pane_id {
                view_state.mouse.hovered_pane_id = target;
                view_state.mouse.hover_started_at = Some(now);
                return Ok(());
            }

            let Some(pane_id) = target else {
                view_state.mouse.hover_started_at = None;
                return Ok(());
            };

            if view_state.mouse.last_focused_pane_id == Some(pane_id) {
                return Ok(());
            }

            let Some(hover_started_at) = view_state.mouse.hover_started_at else {
                view_state.mouse.hover_started_at = Some(now);
                return Ok(());
            };

            if now.duration_since(hover_started_at)
                >= Duration::from_millis(view_state.mouse.config.hover_delay_ms)
            {
                if !handle_attach_mouse_gesture_action(client, view_state, "hover_focus").await? {
                    focus_attach_pane(client, view_state, pane_id).await?;
                }
                view_state.mouse.hover_started_at = Some(now);
            }
        }
        _ => {}
    }

    Ok(())
}

pub(crate) async fn handle_attach_mouse_gesture_action(
    client: &mut BmuxClient,
    view_state: &mut AttachViewState,
    gesture: &str,
) -> std::result::Result<bool, ClientError> {
    let Some(attach_action) = resolve_mouse_gesture_action(view_state, gesture) else {
        return Ok(false);
    };

    match attach_action {
        AttachEventAction::PluginCommand {
            plugin_id,
            command_name,
        } => {
            handle_attach_plugin_command_action(client, &plugin_id, &command_name, view_state)
                .await?;
            Ok(true)
        }
        AttachEventAction::Runtime(action) => {
            if let Err(error) = handle_attach_runtime_action(client, action, view_state).await {
                view_state.set_transient_status(
                    format!("mouse action failed: {}", map_attach_client_error(error)),
                    Instant::now(),
                    ATTACH_TRANSIENT_STATUS_TTL,
                );
            } else {
                view_state.dirty.status_needs_redraw = true;
                view_state.dirty.layout_needs_refresh = true;
                view_state.dirty.full_pane_redraw = true;
            }
            Ok(true)
        }
        AttachEventAction::Ui(action) => {
            if let Err(error) = handle_attach_ui_action(client, action, view_state).await {
                view_state.set_transient_status(
                    format!("mouse action failed: {}", map_attach_client_error(error)),
                    Instant::now(),
                    ATTACH_TRANSIENT_STATUS_TTL,
                );
            } else {
                view_state.dirty.status_needs_redraw = true;
                view_state.dirty.layout_needs_refresh = true;
                view_state.dirty.full_pane_redraw = true;
            }
            Ok(true)
        }
        AttachEventAction::Ignore => Ok(true),
        AttachEventAction::Detach
        | AttachEventAction::Send(_)
        | AttachEventAction::Mouse(_)
        | AttachEventAction::Redraw => Ok(false),
    }
}

pub(crate) fn resolve_mouse_gesture_action(
    view_state: &AttachViewState,
    gesture: &str,
) -> Option<AttachEventAction> {
    let action_name = view_state.mouse.config.gesture_actions.get(gesture)?;
    match crate::input::parse_runtime_action_name(action_name) {
        Ok(action) => Some(runtime_action_to_attach_event_action(action)),
        Err(error) => {
            warn!(
                gesture = %gesture,
                action_name = %action_name,
                error = %error,
                "attach.mouse_gesture.invalid_action"
            );
            None
        }
    }
}

pub(crate) fn handle_attach_mouse_scrollback(
    view_state: &mut AttachViewState,
    kind: MouseEventKind,
) -> bool {
    if !view_state.mouse.config.scroll_scrollback {
        return false;
    }

    let lines = view_state.mouse.config.scroll_lines_per_tick.max(1) as isize;
    match kind {
        MouseEventKind::ScrollUp => {
            if !view_state.scrollback_active && !enter_attach_scrollback(view_state) {
                return false;
            }
            step_attach_scrollback(view_state, -lines);
            view_state.dirty.full_pane_redraw = true;
            view_state.dirty.status_needs_redraw = true;
            true
        }
        MouseEventKind::ScrollDown => {
            if !view_state.scrollback_active {
                return false;
            }
            step_attach_scrollback(view_state, lines);
            if view_state.mouse.config.exit_scrollback_on_bottom
                && view_state.scrollback_offset == 0
                && !view_state.selection_active()
            {
                view_state.exit_scrollback();
            }
            view_state.dirty.full_pane_redraw = true;
            view_state.dirty.status_needs_redraw = true;
            true
        }
        _ => false,
    }
}

pub(crate) async fn focus_attach_pane(
    client: &mut BmuxClient,
    view_state: &mut AttachViewState,
    pane_id: Uuid,
) -> std::result::Result<(), ClientError> {
    if view_state.mouse.last_focused_pane_id == Some(pane_id) {
        return Ok(());
    }

    if let Err(error) = refresh_attached_session_from_context(client, view_state).await {
        view_state.set_transient_status(
            format!(
                "context refresh delayed: {}",
                map_attach_client_error(error)
            ),
            Instant::now(),
            ATTACH_TRANSIENT_STATUS_TTL,
        );
    }

    client
        .focus_pane_target(
            Some(SessionSelector::ById(view_state.attached_id)),
            PaneSelector::ById(pane_id),
        )
        .await?;

    view_state.mouse.last_focused_pane_id = Some(pane_id);
    view_state.dirty.layout_needs_refresh = true;
    view_state.dirty.full_pane_redraw = true;
    view_state.dirty.status_needs_redraw = true;

    Ok(())
}

pub(crate) fn attach_scene_pane_at(
    view_state: &AttachViewState,
    column: u16,
    row: u16,
) -> Option<Uuid> {
    let layout_state = view_state.cached_layout_state.as_ref()?;
    let mut best: Option<(bmux_ipc::AttachLayer, i32, usize, Uuid)> = None;
    for (index, surface) in layout_state.scene.surfaces.iter().enumerate() {
        let Some(pane_id) = surface.pane_id else {
            continue;
        };
        if !surface.visible || !surface.accepts_input {
            continue;
        }
        if !attach_rect_contains_point(surface.rect, column, row) {
            continue;
        }
        let candidate = (surface.layer, surface.z, index, pane_id);
        if best.as_ref().is_none_or(|current| candidate > *current) {
            best = Some(candidate);
        }
    }
    best.map(|(_, _, _, pane_id)| pane_id)
}

pub(crate) fn attach_rect_contains_point(rect: AttachRect, column: u16, row: u16) -> bool {
    if rect.w == 0 || rect.h == 0 {
        return false;
    }
    let max_x = rect.x.saturating_add(rect.w.saturating_sub(1));
    let max_y = rect.y.saturating_add(rect.h.saturating_sub(1));
    column >= rect.x && column <= max_x && row >= rect.y && row <= max_y
}

pub(crate) fn restore_terminal_after_attach_ui() -> Result<()> {
    let mut stdout = io::stdout();
    // Safety net: restore terminal input flags in case the drop guard didn't run.
    #[cfg(feature = "kitty-keyboard")]
    let _ = queue!(stdout, crossterm::event::PopKeyboardEnhancementFlags);
    let _ = queue!(stdout, DisableMouseCapture);
    queue!(
        stdout,
        Show,
        Print("\x1b[0m"),
        MoveTo(0, 0),
        Clear(ClearType::All),
        MoveTo(0, 0)
    )
    .context("failed restoring terminal after attach ui")?;
    stdout
        .flush()
        .context("failed flushing terminal restoration")
}

pub(crate) fn attach_event_actions(
    event: &Event,
    attach_input_processor: &mut InputProcessor,
    ui_mode: AttachUiMode,
) -> Result<Vec<AttachEventAction>> {
    match event {
        Event::Key(key) => attach_key_event_actions(key, attach_input_processor, ui_mode),
        Event::Mouse(mouse) => Ok(vec![AttachEventAction::Mouse(*mouse)]),
        Event::Resize(_, _) => Ok(vec![AttachEventAction::Redraw]),
        Event::FocusGained | Event::FocusLost | Event::Paste(_) => {
            Ok(vec![AttachEventAction::Ignore])
        }
    }
}

pub(crate) fn attach_key_event_actions(
    key: &KeyEvent,
    attach_input_processor: &mut InputProcessor,
    _ui_mode: AttachUiMode,
) -> Result<Vec<AttachEventAction>> {
    // Accept Press and Repeat events. Release events are filtered out here
    // and also inside InputProcessor's crossterm adapter as a safety net.
    if key.kind == KeyEventKind::Release {
        return Ok(vec![AttachEventAction::Ignore]);
    }

    let actions = attach_input_processor.process_terminal_event(Event::Key(*key));
    Ok(actions
        .into_iter()
        .map(runtime_action_to_attach_event_action)
        .collect())
}

pub(crate) fn runtime_action_to_attach_event_action(action: RuntimeAction) -> AttachEventAction {
    match action {
        RuntimeAction::Detach => AttachEventAction::Detach,
        RuntimeAction::ForwardToPane(bytes) => AttachEventAction::Send(bytes),
        RuntimeAction::NewWindow | RuntimeAction::NewSession => AttachEventAction::Runtime(action),
        RuntimeAction::PluginCommand {
            plugin_id,
            command_name,
        } => AttachEventAction::PluginCommand {
            plugin_id,
            command_name,
        },
        RuntimeAction::SessionPrev
        | RuntimeAction::SessionNext
        | RuntimeAction::EnterWindowMode
        | RuntimeAction::SplitFocusedVertical
        | RuntimeAction::SplitFocusedHorizontal
        | RuntimeAction::FocusNext
        | RuntimeAction::FocusLeft
        | RuntimeAction::FocusRight
        | RuntimeAction::FocusUp
        | RuntimeAction::FocusDown
        | RuntimeAction::IncreaseSplit
        | RuntimeAction::DecreaseSplit
        | RuntimeAction::ResizeLeft
        | RuntimeAction::ResizeRight
        | RuntimeAction::ResizeUp
        | RuntimeAction::ResizeDown
        | RuntimeAction::CloseFocusedPane
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
        | RuntimeAction::WindowClose
        | RuntimeAction::Quit
        | RuntimeAction::ShowHelp
        | RuntimeAction::ToggleSplitDirection
        | RuntimeAction::RestartFocusedPane
        | RuntimeAction::EnterScrollMode
        | RuntimeAction::ExitScrollMode
        | RuntimeAction::ScrollUpLine
        | RuntimeAction::ScrollDownLine
        | RuntimeAction::ScrollUpPage
        | RuntimeAction::ScrollDownPage
        | RuntimeAction::ScrollTop
        | RuntimeAction::ScrollBottom
        | RuntimeAction::BeginSelection
        | RuntimeAction::MoveCursorLeft
        | RuntimeAction::MoveCursorRight
        | RuntimeAction::MoveCursorUp
        | RuntimeAction::MoveCursorDown
        | RuntimeAction::CopyScrollback
        | RuntimeAction::ConfirmScrollback => AttachEventAction::Ui(action),
    }
}

pub(crate) const fn is_attach_stream_closed_error(error: &ClientError) -> bool {
    matches!(
        error,
        ClientError::ServerError {
            code: bmux_ipc::ErrorCode::NotFound,
            ..
        }
    )
}
