#[test]
fn scoped_prompt_retention_preserves_send_and_sync() {
    fn require_send_sync<T: Send + Sync>() {}
    require_send_sync::<super::ScopedPrompt>();
    require_send_sync::<super::AttachPromptState>();
}

use super::{
    AttachInternalPromptAction, AttachPromptState, PromptKeyDisposition, PromptWidgetState,
    filtered_option_indices, prompt_overlay_layout, step_search_selection,
};
use crate::runtime::attach::input::TerminalGeometry;
use crate::runtime::attach::tui_surface::{component_theme, parse_tui_color};
use crate::runtime::prompt::{
    PromptField, PromptFormField, PromptFormFieldKind, PromptFormSection, PromptFormValue,
    PromptOption, PromptRequest, PromptResponse, PromptSearchMatchMode, PromptValidation,
    PromptValue,
};
use bmux_appearance::RuntimeAppearance;
use bmux_plugin::prompt::PromptHostRequest;
use bmux_terminal::event::{
    KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers, MouseButton, MouseEvent,
    MouseEventKind,
};
use bmux_tui::style::Color;
use tokio::sync::{mpsc, oneshot};
use uuid::Uuid;

fn key_event(code: KeyCode) -> KeyEvent {
    modified_key_event(code, KeyModifiers::NONE)
}

fn modified_key_event(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
    KeyEvent {
        code,
        modifiers,
        kind: KeyEventKind::Press,
        state: KeyEventState::NONE,
    }
}

/// Render the active prompt into a character grid of the whole terminal.
/// Cells the overlay does not paint are spaces; trailing spaces are trimmed.
fn render_grid(
    state: &mut AttachPromptState,
    cols: u16,
    rows: u16,
    extension_chrome: bool,
) -> Vec<String> {
    let render = state
        .attach_prompt_overlay_render(
            TerminalGeometry { cols, rows },
            &RuntimeAppearance::default(),
            extension_chrome,
        )
        .expect("prompt should render");
    let mut grid = vec![vec![' '; usize::from(cols)]; usize::from(rows)];
    let mut put = |x: u16, y: u16, text: &str| {
        for (offset, ch) in text.chars().enumerate() {
            if let Some(cell) = grid
                .get_mut(usize::from(y))
                .and_then(|row| row.get_mut(usize::from(x) + offset))
            {
                *cell = ch;
            }
        }
    };
    for op in &render.ops {
        match op {
            bmux_plugin::RenderOp::TextRun { x, y, text, .. } => put(*x, *y, text),
            bmux_plugin::RenderOp::StyledText { x, y, spans } => {
                let text = spans
                    .iter()
                    .map(|span| span.text.as_str())
                    .collect::<String>();
                put(*x, *y, &text);
            }
            _ => {}
        }
    }
    grid.into_iter()
        .map(|row| row.into_iter().collect::<String>().trim_end().to_owned())
        .collect()
}

/// The overlay's rows only, without terminal margins.
fn overlay_rows(state: &mut AttachPromptState, cols: u16, rows: u16, ext: bool) -> Vec<String> {
    // Render once so the layout reflects the extension-chrome decision.
    render_grid(state, cols, rows, ext);
    let surface = state
        .overlay_surface(TerminalGeometry { cols, rows })
        .expect("overlay surface")
        .rect;
    render_grid(state, cols, rows, ext)
        .into_iter()
        .skip(usize::from(surface.y))
        .take(usize::from(surface.h))
        .map(|row| {
            row.chars()
                .skip(usize::from(surface.x))
                .take(usize::from(surface.w))
                .collect::<String>()
                .trim_end()
                .to_owned()
        })
        .collect()
}

fn open(request: PromptRequest) -> AttachPromptState {
    let mut state = AttachPromptState::default();
    state.enqueue_internal(request, AttachInternalPromptAction::QuitSession);
    state
}

fn cut_recording_form() -> PromptRequest {
    PromptRequest::form(
        "Cut Recording",
        vec![PromptFormSection::new(
            "cut",
            "Cut Recording",
            vec![
                PromptFormField::text("seconds", "Seconds", "")
                    .placeholder("last N seconds")
                    .validation(PromptValidation::PositiveInteger)
                    .required(true)
                    .description("How much of the recording to keep, counting back from now."),
                PromptFormField::text("fps", "Frame rate", "24")
                    .validation(PromptValidation::PositiveInteger),
                PromptFormField::choice(
                    "format",
                    "Format",
                    vec![
                        PromptOption::new("gif", "gif"),
                        PromptOption::new("mp4", "mp4"),
                    ],
                    1,
                ),
                PromptFormField::multi(
                    "effects",
                    "Effects",
                    vec![
                        PromptOption::new("cursor", "cursor"),
                        PromptOption::new("clicks", "clicks"),
                    ],
                    vec![0],
                ),
                PromptFormField::toggle("open", "Open when done", true),
                PromptFormField::new(
                    "quality",
                    "Quality",
                    PromptFormFieldKind::Integer {
                        initial_value: 80,
                        min: Some(1),
                        max: Some(100),
                    },
                ),
            ],
        )],
    )
    .message("Export the last N seconds of the recording.")
    .submit_label("Export")
}

fn form_values(state: &AttachPromptState) -> std::collections::BTreeMap<String, PromptFormValue> {
    let Some(super::ActivePrompt {
        state: PromptWidgetState::Form(form),
        ..
    }) = state.active.as_ref()
    else {
        panic!("form prompt should be active");
    };
    form.values.clone()
}

fn form_focus(state: &AttachPromptState) -> super::form::FormFocus {
    let Some(super::ActivePrompt {
        state: PromptWidgetState::Form(form),
        ..
    }) = state.active.as_ref()
    else {
        panic!("form prompt should be active");
    };
    form.focus
}

fn type_text(state: &mut AttachPromptState, text: &str) {
    for ch in text.chars() {
        state.handle_key_event(&key_event(KeyCode::Char(ch)));
    }
}

fn submitted_form(
    disposition: PromptKeyDisposition,
) -> std::collections::BTreeMap<String, PromptFormValue> {
    let PromptKeyDisposition::Completed(completion) = disposition else {
        panic!("expected the form to submit");
    };
    let PromptResponse::Submitted(PromptValue::Form(values)) = completion.response else {
        panic!("expected form values, got {:?}", completion.response);
    };
    values
}

#[test]
fn text_prompt_input_box_sits_above_footer() {
    // Regression: the footer used to overwrite the input's bottom border.
    let mut state =
        open(PromptRequest::text_input("Cut Recording").input_placeholder("last N seconds"));
    assert_eq!(
        overlay_rows(&mut state, 120, 40, false),
        [
            "╭ Cut Recording ───────────────────────────────────╮",
            "│ ┌──────────────────────────────────────────────┐ │",
            "│ │ last N seconds                               │ │",
            "│ └──────────────────────────────────────────────┘ │",
            "│                                                  │",
            "│ Enter submit · Esc cancel  [ Submit ] [ Cancel ] │",
            "╰──────────────────────────────────────────────────╯",
        ]
    );
}

#[test]
fn text_prompt_shows_message_and_validation_error() {
    let mut state = open(
        PromptRequest::text_input("Rename")
            .message("Enter a new name for the window")
            .input_required(true),
    );
    state.handle_key_event(&key_event(KeyCode::Enter));
    let rows = overlay_rows(&mut state, 120, 40, false);
    assert!(
        rows.iter()
            .any(|row| row.contains("Enter a new name for the window"))
    );
    assert!(rows.iter().any(|row| row.contains("✕ value is required")));
}

#[test]
fn compact_form_renders_every_field_with_labels_and_actions() {
    let mut state = open(cut_recording_form());
    assert_eq!(
        overlay_rows(&mut state, 120, 40, false),
        [
            "╭ Cut Recording ──────────────────────────────────────────────╮",
            "│ Export the last N seconds of the recording.                 │",
            "│                                                             │",
            "│ ▎      Seconds *  last N seconds                            │",
            "│                   How much of the recording to keep,        │",
            "│                   counting back from now.                   │",
            "│       Frame rate  24                                        │",
            "│           Format  ( ) gif  (•) mp4                          │",
            "│          Effects  [x] cursor  [ ] clicks                    │",
            "│   Open when done  [x]                                       │",
            "│          Quality  80                                        │",
            "│                                                             │",
            "│ Tab next · Enter export · Esc cancel  [ Export ] [ Cancel ] │",
            "╰─────────────────────────────────────────────────────────────╯",
        ]
    );
}

#[test]
fn extension_chrome_keeps_title_inside_the_border() {
    let mut state = open(cut_recording_form());
    let rows = overlay_rows(&mut state, 120, 40, true);
    assert_eq!(rows[0], "");
    assert_eq!(rows[1], "  Cut Recording");
    assert_eq!(rows.last().map(String::as_str), Some(""));
    assert!(
        rows.iter()
            .all(|row| !row.contains('╭') && !row.contains('│'))
    );
}

#[test]
fn stacked_layout_setting_overrides_compact_default() {
    let mut state = open(cut_recording_form());
    state.set_form_layout(bmux_config::PromptFormLayoutSetting::Stacked);
    let rows = overlay_rows(&mut state, 120, 40, false);
    assert!(
        rows.iter()
            .any(|row| row.trim_end_matches('│').trim_end() == "│ ▎ Seconds *")
    );
    assert!(rows.iter().any(|row| row.contains("│   ┌")));
    // Help text is always visible when stacked, even for unfocused fields.
    let mut hinted =
        open(cut_recording_form().form_layout(crate::runtime::prompt::PromptFormLayout::Stacked));
    assert_eq!(overlay_rows(&mut hinted, 120, 40, false), rows);
}

#[test]
fn small_terminal_scrolls_form_body_and_keeps_footer() {
    let mut state = open(cut_recording_form());
    let rows = overlay_rows(&mut state, 60, 16, false);
    assert!(
        rows.iter().any(|row| row.ends_with("█ │")),
        "scrollbar thumb"
    );
    assert!(rows.iter().any(|row| row.contains("[ Export ] [ Cancel ]")));
    assert!(!rows.iter().any(|row| row.contains("Quality")));
    for _ in 0..5 {
        state.handle_key_event(&key_event(KeyCode::Tab));
    }
    let rows = overlay_rows(&mut state, 60, 16, false);
    assert!(rows.iter().any(|row| row.contains("▎        Quality")));
}

#[test]
fn form_text_fields_accept_navigation_letters_and_spaces() {
    let mut state = open(cut_recording_form().form_resettable(true));
    // Focus Frame rate (a text field) and replace its value.
    state.handle_key_event(&key_event(KeyCode::Tab));
    state.handle_key_event(&modified_key_event(
        KeyCode::Char('u'),
        KeyModifiers::CONTROL,
    ));
    type_text(&mut state, "jk r");
    assert_eq!(
        form_values(&state).get("fps"),
        Some(&PromptFormValue::Text("jk r".to_owned()))
    );
}

#[test]
fn enter_submits_valid_forms_and_focuses_first_invalid_field() {
    let mut state = open(cut_recording_form());
    state.handle_key_event(&key_event(KeyCode::Tab));
    assert!(matches!(
        state.handle_key_event(&key_event(KeyCode::Enter)),
        PromptKeyDisposition::Consumed
    ));
    assert_eq!(form_focus(&state), super::form::FormFocus::Field(0));
    let rows = overlay_rows(&mut state, 120, 40, false);
    assert!(rows.iter().any(|row| row.contains("✕ required")));

    type_text(&mut state, "30");
    let values = submitted_form(state.handle_key_event(&key_event(KeyCode::Enter)));
    assert_eq!(
        values.get("seconds"),
        Some(&PromptFormValue::Text("30".to_owned()))
    );
    assert_eq!(
        values.get("format"),
        Some(&PromptFormValue::Single("mp4".to_owned()))
    );
    assert_eq!(values.get("quality"), Some(&PromptFormValue::Integer(80)));
}

#[test]
fn tab_cycles_through_fields_then_actions() {
    use super::form::FormFocus;
    let mut state = open(cut_recording_form());
    let mut seen = vec![form_focus(&state)];
    for _ in 0..8 {
        state.handle_key_event(&key_event(KeyCode::Tab));
        seen.push(form_focus(&state));
    }
    assert_eq!(
        seen,
        [
            FormFocus::Field(0),
            FormFocus::Field(1),
            FormFocus::Field(2),
            FormFocus::Field(3),
            FormFocus::Field(4),
            FormFocus::Field(5),
            FormFocus::Action(super::form::SUBMIT_ACTION),
            FormFocus::Action(super::form::CANCEL_ACTION),
            FormFocus::Field(0),
        ]
    );
    state.handle_key_event(&key_event(KeyCode::BackTab));
    let disposition = state.handle_key_event(&key_event(KeyCode::Enter));
    let PromptKeyDisposition::Completed(completion) = disposition else {
        panic!("Enter on Cancel should complete");
    };
    assert_eq!(completion.response, PromptResponse::Cancelled);
}

#[test]
fn choice_and_toggle_fields_respond_to_arrows_and_space() {
    let mut state = open(cut_recording_form());
    type_text(&mut state, "5");
    for _ in 0..2 {
        state.handle_key_event(&key_event(KeyCode::Tab));
    }
    state.handle_key_event(&key_event(KeyCode::Left));
    assert_eq!(
        form_values(&state).get("format"),
        Some(&PromptFormValue::Single("gif".to_owned()))
    );
    state.handle_key_event(&key_event(KeyCode::Tab));
    state.handle_key_event(&key_event(KeyCode::Right));
    state.handle_key_event(&key_event(KeyCode::Char(' ')));
    assert_eq!(
        form_values(&state).get("effects"),
        Some(&PromptFormValue::Multi(vec![
            "cursor".to_owned(),
            "clicks".to_owned()
        ]))
    );
    state.handle_key_event(&key_event(KeyCode::Tab));
    state.handle_key_event(&key_event(KeyCode::Char(' ')));
    assert_eq!(
        form_values(&state).get("open"),
        Some(&PromptFormValue::Bool(false))
    );
}

#[test]
fn integer_fields_step_filter_and_validate() {
    let mut state = open(cut_recording_form());
    type_text(&mut state, "5");
    for _ in 0..5 {
        state.handle_key_event(&key_event(KeyCode::Tab));
    }
    state.handle_key_event(&modified_key_event(KeyCode::Right, KeyModifiers::SHIFT));
    assert_eq!(
        form_values(&state).get("quality"),
        Some(&PromptFormValue::Integer(85))
    );
    state.handle_key_event(&modified_key_event(KeyCode::Right, KeyModifiers::CONTROL));
    assert_eq!(
        form_values(&state).get("quality"),
        Some(&PromptFormValue::Integer(95))
    );
    state.handle_key_event(&modified_key_event(KeyCode::Right, KeyModifiers::CONTROL));
    assert_eq!(
        form_values(&state).get("quality"),
        Some(&PromptFormValue::Integer(100)),
        "clamped to max"
    );
    state.handle_key_event(&modified_key_event(
        KeyCode::Char('u'),
        KeyModifiers::CONTROL,
    ));
    type_text(&mut state, "x");
    state.handle_key_event(&key_event(KeyCode::Enter));
    let rows = overlay_rows(&mut state, 120, 40, false);
    assert!(
        rows.iter()
            .any(|row| row.contains("✕ enter a whole number"))
    );
    type_text(&mut state, "250");
    state.handle_key_event(&key_event(KeyCode::Enter));
    let rows = overlay_rows(&mut state, 120, 40, false);
    assert!(
        rows.iter()
            .any(|row| row.contains("✕ must be between 1 and 100"))
    );
}

#[test]
fn form_mouse_clicks_options_and_actions_and_wheel_scrolls() {
    let mut state = open(cut_recording_form());
    let geometry = TerminalGeometry { cols: 60, rows: 16 };
    let grid = render_grid(&mut state, 60, 16, false);
    let (row, col) = grid
        .iter()
        .enumerate()
        .find_map(|(row, line)| {
            line.find("( ) gif")
                .map(|byte| (row, line[..byte].chars().count()))
        })
        .expect("gif option rendered");
    let click = |column: usize, row: usize, kind: MouseEventKind| MouseEvent {
        kind,
        column: u16::try_from(column).unwrap(),
        row: u16::try_from(row).unwrap(),
        modifiers: KeyModifiers::NONE,
    };
    state.handle_mouse_event(
        click(col, row, MouseEventKind::Down(MouseButton::Left)),
        geometry,
    );
    assert_eq!(
        form_values(&state).get("format"),
        Some(&PromptFormValue::Single("gif".to_owned()))
    );

    // A short terminal keeps the body overflowing so the wheel can scroll it.
    let mut scrolled = open(cut_recording_form());
    let short = TerminalGeometry { cols: 60, rows: 12 };
    let grid = render_grid(&mut scrolled, 60, 12, false);
    let body_row = grid
        .iter()
        .position(|line| line.contains("Seconds"))
        .expect("first field rendered");
    scrolled.handle_mouse_event(click(col, body_row, MouseEventKind::ScrollDown), short);
    let offset = |state: &AttachPromptState| {
        let Some(super::ActivePrompt {
            state: PromptWidgetState::Form(form),
            ..
        }) = state.active.as_ref()
        else {
            panic!("form active");
        };
        form.scroll_offset()
    };
    assert!(offset(&scrolled) > 0);

    let grid = render_grid(&mut state, 60, 16, false);
    let (row, col) = grid
        .iter()
        .enumerate()
        .find_map(|(row, line)| {
            line.find("[ Cancel ]")
                .map(|byte| (row, line[..byte].chars().count() + 2))
        })
        .expect("cancel rendered");
    let PromptKeyDisposition::Completed(completion) = state.handle_mouse_event(
        click(col, row, MouseEventKind::Down(MouseButton::Left)),
        geometry,
    ) else {
        panic!("cancel click completes");
    };
    assert_eq!(completion.response, PromptResponse::Cancelled);
}

#[test]
fn hidden_form_swallows_keys_until_shown() {
    let mut state = open(cut_recording_form());
    state.handle_key_event(&key_event(KeyCode::F(6)));
    assert!(
        state
            .overlay_surface(TerminalGeometry { cols: 80, rows: 24 })
            .is_none()
    );
    type_text(&mut state, "9");
    assert_eq!(
        form_values(&state).get("seconds"),
        Some(&PromptFormValue::Text(String::new()))
    );
    state.handle_key_event(&key_event(KeyCode::F(6)));
    type_text(&mut state, "9");
    assert_eq!(
        form_values(&state).get("seconds"),
        Some(&PromptFormValue::Text("9".to_owned()))
    );
}

#[test]
fn live_form_emits_only_valid_changes() {
    let (response_tx, _response_rx) = oneshot::channel();
    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    let mut state = AttachPromptState::default();
    state.enqueue_external(PromptHostRequest {
        request: cut_recording_form().form_live_preview(true),
        response_tx,
        event_tx: Some(event_tx),
    });
    type_text(&mut state, "x");
    assert!(
        event_rx.try_recv().is_err(),
        "invalid edits are not previewed"
    );
    state.handle_key_event(&key_event(KeyCode::Backspace));
    // Clearing a required field is still invalid.
    assert!(event_rx.try_recv().is_err());
    type_text(&mut state, "12");
    let mut last = None;
    while let Ok(event) = event_rx.try_recv() {
        last = Some(event);
    }
    let Some(crate::runtime::prompt::PromptEvent::FormChanged {
        field_id, values, ..
    }) = last
    else {
        panic!("expected a form change event");
    };
    assert_eq!(field_id, "seconds");
    assert_eq!(
        values.get("seconds"),
        Some(&PromptFormValue::Text("12".to_owned()))
    );
}

#[test]
fn plugin_supplied_field_error_is_shown_and_focused() {
    let mut request = cut_recording_form();
    let PromptField::Form { sections, .. } = &mut request.field else {
        unreachable!();
    };
    sections[0].fields[1] = sections[0].fields[1].clone().error("too fast for GIF");
    let mut state = open(request);
    assert_eq!(form_focus(&state), super::form::FormFocus::Field(1));
    let rows = overlay_rows(&mut state, 120, 40, false);
    assert!(rows.iter().any(|row| row.contains("✕ too fast for GIF")));
    // Fixing the value clears the error immediately.
    state.handle_key_event(&modified_key_event(
        KeyCode::Char('u'),
        KeyModifiers::CONTROL,
    ));
    type_text(&mut state, "12");
    let rows = overlay_rows(&mut state, 120, 40, false);
    assert!(!rows.iter().any(|row| row.contains('✕')));
}

#[test]
fn lone_unlabeled_field_uses_the_full_row() {
    let mut state = open(
        PromptRequest::form(
            "Cut Recording",
            vec![PromptFormSection::new(
                "values",
                "",
                vec![
                    PromptFormField::text("seconds", "", "")
                        .placeholder("last N seconds")
                        .required(true),
                ],
            )],
        )
        .submit_label("Run"),
    );
    assert_eq!(
        overlay_rows(&mut state, 120, 40, false),
        [
            "╭ Cut Recording ─────────────────────────────╮",
            "│ ▎ last N seconds                           │",
            "│                                            │",
            "│ Enter run · Esc cancel  [ Run ] [ Cancel ] │",
            "╰────────────────────────────────────────────╯",
        ]
    );
}

#[test]
fn form_focus_is_marked_without_color() {
    let mut state = open(cut_recording_form());
    let rows = overlay_rows(&mut state, 120, 40, false);
    assert!(rows[3].contains("▎      Seconds"));
    type_text(&mut state, "1");
    state.handle_key_event(&key_event(KeyCode::Tab));
    let rows = overlay_rows(&mut state, 120, 40, false);
    assert!(!rows[3].contains('▎'));
    assert!(rows.iter().any(|row| row.contains("▎     Frame rate")));
}

#[test]
fn bounded_prompt_layout_adapts_to_terminal_and_entry_count() {
    let request = PromptRequest::search_select(
        "Search",
        (0..100)
            .map(|index| PromptOption::new(index.to_string(), "Short"))
            .collect(),
    )
    .width_range(90, 90)
    .max_height(30);
    for (cols, rows, width, height) in [
        (200, 60, 90, 30),
        (120, 40, 90, 30),
        (80, 24, 78, 22),
        (60, 18, 58, 16),
        (200, 18, 90, 16),
        (60, 60, 58, 30),
        (20, 6, 20, 6),
    ] {
        let layout =
            prompt_overlay_layout(Some(&request), TerminalGeometry { cols, rows }).expect("layout");
        let rect = layout.surface.rect;
        assert_eq!((rect.w, rect.h), (width, height));
        assert_eq!((rect.x, rect.y), ((cols - width) / 2, (rows - height) / 2));
    }
    let short = PromptRequest::search_select("Search", vec![PromptOption::new("one", "One")])
        .width_range(90, 90)
        .max_height(30);
    let layout = prompt_overlay_layout(
        Some(&short),
        TerminalGeometry {
            cols: 200,
            rows: 60,
        },
    )
    .expect("short layout");
    // Interior title and the search/list separator each need their own row.
    assert_eq!((layout.surface.rect.w, layout.surface.rect.h), (90, 8));
}

#[test]
fn command_palette_query_cursor_tracks_caller_owned_edits() {
    let mut state = AttachPromptState::default();
    state.enqueue_internal(
        PromptRequest::search_select(
            "Command Palette",
            vec![PromptOption::new("commit", "Commit")],
        )
        .modal_id("command-palette"),
        AttachInternalPromptAction::QuitSession,
    );
    let geometry = TerminalGeometry { cols: 80, rows: 24 };

    let empty = state
        .attach_prompt_overlay_render(geometry, &RuntimeAppearance::default(), false)
        .expect("empty palette should render");
    let empty_cursor = empty.cursor_state.expect("empty query cursor");

    for ch in "co".chars() {
        let _ = state.handle_key_event(&key_event(KeyCode::Char(ch)));
    }
    let edited = state
        .attach_prompt_overlay_render(geometry, &RuntimeAppearance::default(), false)
        .expect("edited palette should render");
    let edited_cursor = edited.cursor_state.expect("edited query cursor");

    assert!(empty_cursor.visible);
    assert!(edited_cursor.visible);
    assert_eq!(edited_cursor.y, empty_cursor.y);
    assert_eq!(edited_cursor.x, empty_cursor.x.saturating_add(2));
}

#[test]
fn command_palette_degrades_to_full_viewport_on_small_terminal() {
    let mut state = AttachPromptState::default();
    state.enqueue_internal(
        PromptRequest::search_select("Command Palette", vec![PromptOption::new("one", "One")])
            .modal_id("command-palette"),
        AttachInternalPromptAction::QuitSession,
    );
    let geometry = TerminalGeometry { cols: 20, rows: 6 };

    let render = state
        .attach_prompt_overlay_render(geometry, &RuntimeAppearance::default(), false)
        .expect("compact palette should render");

    assert_eq!(render.surface.rect.x, 0);
    assert_eq!(render.surface.rect.y, 0);
    assert_eq!(render.surface.rect.w, 20);
    assert_eq!(render.surface.rect.h, 6);
}

#[test]
fn prompt_highlight_tracks_arrow_navigation() {
    for mode in 0..3 {
        let options = vec![
            PromptOption::new("one", "First label")
                .detail("First detail")
                .key_hint("F1"),
            PromptOption::new("two", "Second label")
                .detail("Second detail")
                .key_hint("F2"),
        ];
        let request = match mode {
            0 => PromptRequest::search_select("Search", options),
            1 => PromptRequest::search_select("Search", options).modal_id("command-palette"),
            _ => PromptRequest::single_select("Select", options),
        };
        let mut state = AttachPromptState::default();
        state.enqueue_internal(request, AttachInternalPromptAction::QuitSession);
        let geometry = TerminalGeometry { cols: 80, rows: 24 };
        let appearance = RuntimeAppearance::default();
        let before = state
            .attach_prompt_overlay_render(geometry, &appearance, false)
            .unwrap();
        state.handle_key_event(&key_event(KeyCode::Down));
        let after = state
            .attach_prompt_overlay_render(geometry, &appearance, false)
            .unwrap();
        let style_for = |render: &super::AttachPromptOverlayRender, needle: &str| {
            render
                .ops
                .iter()
                .find_map(|op| match op {
                    bmux_plugin::RenderOp::TextRun { text, style, .. } if text.contains(needle) => {
                        Some(*style)
                    }
                    _ => None,
                })
                .expect("rendered text")
        };
        for (first, second) in [
            ("First label", "Second label"),
            ("First detail", "Second detail"),
        ] {
            assert_ne!(style_for(&before, first), style_for(&after, first));
            assert_eq!(style_for(&before, first), style_for(&after, second));
        }
        if mode == 1 {
            assert!(style_for(&before, "F1").reverse);
            assert!(!style_for(&after, "F1").reverse);
            assert!(style_for(&after, "F2").reverse);
        }
        state.handle_key_event(&key_event(KeyCode::Up));
        let restored = state
            .attach_prompt_overlay_render(geometry, &appearance, false)
            .unwrap();
        assert_eq!(
            style_for(&before, "First label"),
            style_for(&restored, "First label")
        );
    }
}

fn modal_focus_cases() -> Vec<(PromptRequest, KeyCode)> {
    vec![
        (
            PromptRequest::confirm("Confirm").confirm_labels("First", "Second"),
            KeyCode::Tab,
        ),
        (
            PromptRequest::multi_toggle(
                "Toggle",
                vec![
                    PromptOption::new("first", "First"),
                    PromptOption::new("second", "Second"),
                ],
            ),
            KeyCode::Down,
        ),
    ]
}

#[test]
fn modal_controls_keep_focus_visible_without_color_contrast() {
    for (request, key) in modal_focus_cases() {
        let first_label = "First";
        let mut state = AttachPromptState::default();
        state.enqueue_internal(request, AttachInternalPromptAction::QuitSession);
        let appearance = RuntimeAppearance {
            foreground: "#ffffff".to_owned(),
            cursor: "#ffffff".to_owned(),
            ..RuntimeAppearance::default()
        };
        let geometry = TerminalGeometry { cols: 80, rows: 24 };
        let before = state
            .attach_prompt_overlay_render(geometry, &appearance, false)
            .unwrap();
        state.handle_key_event(&key_event(key));
        let after = state
            .attach_prompt_overlay_render(geometry, &appearance, false)
            .unwrap();
        let focused = |render: &super::AttachPromptOverlayRender, label: &str| {
            render.ops.iter().any(|op| {
                matches!(op,
                bmux_plugin::RenderOp::TextRun { text, style, .. }
                if text.contains(label) && style.bold && style.underline)
            })
        };
        // Confirm defaults to No; other controls start at the first item.
        let initially_first = focused(&before, first_label);
        assert_ne!(initially_first, focused(&before, "Second"));
        assert_ne!(initially_first, focused(&after, first_label));
        assert_eq!(initially_first, focused(&after, "Second"));
    }
}

#[test]
fn command_palette_renders_scrollbar_for_long_results() {
    let mut state = AttachPromptState::default();
    let options = (0..40)
        .map(|index| PromptOption::new(index.to_string(), format!("Command {index}")))
        .collect();
    state.enqueue_internal(
        PromptRequest::search_select("Command Palette", options).modal_id("command-palette"),
        AttachInternalPromptAction::QuitSession,
    );

    let render = state
        .attach_prompt_overlay_render(
            TerminalGeometry { cols: 80, rows: 24 },
            &RuntimeAppearance::default(),
            false,
        )
        .expect("palette should render");

    assert!(render.ops.iter().any(|op| {
        matches!(op, bmux_plugin::RenderOp::TextRun { text, .. } if text.contains('█'))
    }));
}

#[test]
fn search_select_pages_by_measured_rows() {
    use super::{
        LayoutCx, Line, Text, TextBlock, VirtualList, VirtualListState, search_select_page_target,
    };
    let list = VirtualList::new("paging")
        .component(
            10,
            TextBlock::new(Text::from_lines([Line::raw("abcdefgh")])).id("first"),
        )
        .component(
            20,
            TextBlock::new(Text::from_lines([Line::raw("ijklmnop")])).id("second"),
        )
        .component(
            30,
            TextBlock::new(Text::from_lines([Line::raw("qrstuvwx")])).id("third"),
        );
    let mut state = VirtualListState::new(0);
    list.sync(2, &mut state, &mut LayoutCx::new());
    assert_eq!(state.total_height(), 12);
    assert_eq!(state.item_height(&20), Some(4));
    assert_eq!(state.item_height(&99), None);
    let filtered = [10, 20, 30];
    // No sync between filtering/reordering and paging: offsets are stale,
    // but each retained keyed height remains usable.
    assert_eq!(
        search_select_page_target(&state, &[30, 10, 20], 0, 8, true),
        2
    );
    assert_eq!(
        search_select_page_target(&state, &[30, 10, 20], 2, 8, false),
        0
    );
    assert_eq!(search_select_page_target(&state, &filtered, 0, 4, true), 1);
    assert_eq!(search_select_page_target(&state, &filtered, 2, 4, false), 1);
    assert_eq!(search_select_page_target(&state, &filtered, 0, 1, true), 1);
    assert_eq!(search_select_page_target(&state, &filtered, 2, 4, true), 2);
    assert_eq!(search_select_page_target(&state, &filtered, 0, 4, false), 0);
    assert_eq!(search_select_page_target(&state, &[], 0, 4, true), 0);
    assert_eq!(search_select_page_target(&state, &[40, 50], 0, 4, true), 1);
}

#[test]
fn search_select_reuses_gutter_and_full_width_measurements() {
    let mut state = AttachPromptState::default();
    state.enqueue_internal(
        PromptRequest::search_select(
            "Search",
            vec![PromptOption::new("long", "wrapped label ".repeat(100))],
        ),
        AttachInternalPromptAction::QuitSession,
    );
    let geometry = TerminalGeometry { cols: 40, rows: 16 };
    let misses = |state: &AttachPromptState| {
        let PromptWidgetState::SearchSelect {
            list, width_probe, ..
        } = &state.active.as_ref().unwrap().state
        else {
            panic!("search select");
        };
        (
            list.layout_cache().stats().misses,
            width_probe.layout_cache().stats().misses,
        )
    };
    state
        .attach_prompt_overlay_render(geometry, &RuntimeAppearance::default(), false)
        .unwrap();
    let initial = misses(&state);
    assert!(initial.0 > 0 && initial.1 > 0);
    for _ in 0..3 {
        state
            .attach_prompt_overlay_render(geometry, &RuntimeAppearance::default(), false)
            .unwrap();
        assert_eq!(misses(&state), initial);
    }
}

#[test]
fn command_palette_scrollbar_uses_wrapped_row_extent() {
    let mut state = AttachPromptState::default();
    state.enqueue_internal(
        PromptRequest::search_select(
            "Command Palette",
            vec![PromptOption::new("long", "wrapped label ".repeat(100))],
        )
        .modal_id("command-palette"),
        AttachInternalPromptAction::QuitSession,
    );
    let render = state
        .attach_prompt_overlay_render(
            TerminalGeometry { cols: 40, rows: 16 },
            &RuntimeAppearance::default(),
            false,
        )
        .expect("palette should render");
    let active = state.active.as_ref().unwrap();
    let PromptWidgetState::SearchSelect {
        list,
        viewport_height,
        ..
    } = &active.state
    else {
        panic!("expected search select");
    };
    assert!(*viewport_height > 1);
    assert!(list.total_height() > *viewport_height);
    assert!(render.ops.iter().any(|op| {
        matches!(op, bmux_plugin::RenderOp::TextRun { text, .. } if text.contains('█'))
    }));
}

#[test]
fn pasted_search_reveals_first_match_after_manual_scroll() {
    let mut state = AttachPromptState::default();
    state.enqueue_internal(
        PromptRequest::search_select(
            "Search",
            (0..40)
                .map(|i| PromptOption::new(i.to_string(), format!("Item {i:02}")))
                .collect(),
        ),
        AttachInternalPromptAction::QuitSession,
    );
    let geometry = TerminalGeometry { cols: 40, rows: 16 };
    state.handle_key_event(&KeyEvent::new(KeyCode::End, KeyModifiers::NONE));
    state
        .attach_prompt_overlay_render(geometry, &RuntimeAppearance::default(), false)
        .unwrap();
    let area = state.active.as_ref().unwrap().list_viewport;
    state.handle_mouse_event(
        MouseEvent {
            kind: MouseEventKind::ScrollUp,
            column: area.x,
            row: area.y,
            modifiers: KeyModifiers::NONE,
        },
        geometry,
    );
    assert!(state.active.as_ref().unwrap().manual_list_scroll);
    state.handle_paste("Item 1");
    assert!(!state.active.as_ref().unwrap().manual_list_scroll);
    for _ in 0..2 {
        state
            .attach_prompt_overlay_render(geometry, &RuntimeAppearance::default(), false)
            .unwrap();
        let PromptWidgetState::SearchSelect { selected, list, .. } =
            &state.active.as_ref().unwrap().state
        else {
            panic!("search select");
        };
        assert_eq!(*selected, 0);
        assert_eq!(list.scroll.vertical_offset(), 0);
        assert_eq!(list.key_at_offset(0), Some(&10));
    }
}

#[test]
fn manual_select_scroll_preserves_key_and_row_on_resize() {
    for searchable in [false, true] {
        let options = (0..40)
            .map(|i| {
                PromptOption::new(
                    i.to_string(),
                    format!("Item {i:02} {}", "wrapped ".repeat(10)),
                )
            })
            .collect();
        let request = if searchable {
            PromptRequest::search_select("Search", options)
        } else {
            PromptRequest::single_select("Select", options)
        };
        let mut state = AttachPromptState::default();
        state.enqueue_internal(request, AttachInternalPromptAction::QuitSession);
        let narrow = TerminalGeometry { cols: 40, rows: 16 };
        state
            .attach_prompt_overlay_render(narrow, &RuntimeAppearance::default(), false)
            .unwrap();
        let area = state.active.as_ref().unwrap().list_viewport;
        for _ in 0..5 {
            state.handle_mouse_event(
                MouseEvent {
                    kind: MouseEventKind::ScrollDown,
                    column: area.x,
                    row: area.y,
                    modifiers: KeyModifiers::NONE,
                },
                narrow,
            );
        }
        let anchor = |state: &AttachPromptState| {
            let (PromptWidgetState::SingleSelect { list, .. }
            | PromptWidgetState::SearchSelect { list, .. }) = &state.active.as_ref().unwrap().state
            else {
                panic!("select");
            };
            let offset = list.scroll.vertical_offset();
            let key = *list.key_at_offset(offset).unwrap();
            (
                key,
                offset - list.item_offset(&key).unwrap(),
                list.item_height(&key).unwrap(),
            )
        };
        let before = anchor(&state);
        state
            .attach_prompt_overlay_render(
                TerminalGeometry { cols: 80, rows: 24 },
                &RuntimeAppearance::default(),
                false,
            )
            .unwrap();
        let after = anchor(&state);
        assert_eq!(after.0, before.0);
        assert_eq!(after.1, before.1.min(after.2 - 1));
    }
}

#[test]
fn single_select_wheel_clamps_and_survives_repaint() {
    let mut state = AttachPromptState::default();
    state.enqueue_internal(
        PromptRequest::single_select(
            "Select",
            (0..40)
                .map(|i| PromptOption::new(i.to_string(), format!("Item {i}")))
                .collect(),
        ),
        AttachInternalPromptAction::QuitSession,
    );
    let geometry = TerminalGeometry { cols: 80, rows: 16 };
    state
        .attach_prompt_overlay_render(geometry, &RuntimeAppearance::default(), false)
        .unwrap();
    let area = state.active.as_ref().unwrap().list_viewport;
    for _ in 0..50 {
        state.handle_mouse_event(
            MouseEvent {
                kind: MouseEventKind::ScrollDown,
                column: area.x,
                row: area.y,
                modifiers: KeyModifiers::NONE,
            },
            geometry,
        );
    }
    state
        .attach_prompt_overlay_render(geometry, &RuntimeAppearance::default(), false)
        .unwrap();
    let PromptWidgetState::SingleSelect { list, selected } = &state.active.as_ref().unwrap().state
    else {
        panic!("select");
    };
    assert_eq!(*selected, 0);
    assert_eq!(
        list.scroll.vertical_offset(),
        list.total_height() - u64::from(area.height)
    );
    state.handle_key_event(&key_event(KeyCode::Down));
    state
        .attach_prompt_overlay_render(geometry, &RuntimeAppearance::default(), false)
        .unwrap();
    let PromptWidgetState::SingleSelect { list, selected } = &state.active.as_ref().unwrap().state
    else {
        panic!("select");
    };
    assert_eq!(*selected, 1);
    assert_eq!(list.scroll.vertical_offset(), 1);
}

#[test]
fn search_select_wheel_survives_render_and_keys_restore_reveal() {
    let mut state = AttachPromptState::default();
    state.enqueue_internal(
        PromptRequest::search_select(
            "Search",
            (0..40)
                .map(|i| PromptOption::new(i.to_string(), format!("Result {i:02}")))
                .collect(),
        ),
        AttachInternalPromptAction::QuitSession,
    );
    let geometry = TerminalGeometry { cols: 80, rows: 24 };
    state
        .attach_prompt_overlay_render(geometry, &RuntimeAppearance::default(), false)
        .unwrap();
    let area = state.active.as_ref().unwrap().list_viewport;
    state.handle_mouse_event(
        MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: area.x,
            row: area.y,
            modifiers: KeyModifiers::NONE,
        },
        geometry,
    );
    state
        .attach_prompt_overlay_render(geometry, &RuntimeAppearance::default(), false)
        .unwrap();
    let PromptWidgetState::SearchSelect { list, selected, .. } =
        &state.active.as_ref().unwrap().state
    else {
        panic!("search");
    };
    assert_eq!(*selected, 0);
    assert_eq!(list.scroll.vertical_offset(), 3);
    state.handle_key_event(&key_event(KeyCode::Home));
    state
        .attach_prompt_overlay_render(geometry, &RuntimeAppearance::default(), false)
        .unwrap();
    let PromptWidgetState::SearchSelect { list, .. } = &state.active.as_ref().unwrap().state else {
        panic!("search");
    };
    assert_eq!(list.scroll.vertical_offset(), 0);
}

#[test]
fn search_select_non_selection_mouse_events_preserve_selection() {
    let mut state = AttachPromptState::default();
    state.enqueue_internal(
        PromptRequest::search_select(
            "Search",
            vec![
                PromptOption::new("one", "One"),
                PromptOption::new("two", "Two"),
            ],
        ),
        AttachInternalPromptAction::QuitSession,
    );
    let geometry = TerminalGeometry { cols: 80, rows: 24 };
    state
        .attach_prompt_overlay_render(geometry, &RuntimeAppearance::default(), false)
        .unwrap();
    let hit = state
        .active
        .as_ref()
        .unwrap()
        .hits
        .regions()
        .iter()
        .find(|hit| hit.id.as_str() == "command-list.item.1")
        .unwrap()
        .area;
    for kind in [
        MouseEventKind::ScrollDown,
        MouseEventKind::ScrollUp,
        MouseEventKind::Down(MouseButton::Right),
        MouseEventKind::Up(MouseButton::Left),
    ] {
        state.handle_mouse_event(
            MouseEvent {
                kind,
                column: hit.x,
                row: hit.y,
                modifiers: KeyModifiers::NONE,
            },
            geometry,
        );
        let PromptWidgetState::SearchSelect { selected, .. } =
            &state.active.as_ref().unwrap().state
        else {
            panic!("search select");
        };
        assert_eq!(*selected, 0, "{kind:?} must not select the hovered item");
    }
}

#[test]
fn command_palette_mouse_uses_component_hit_map() {
    let mut state = AttachPromptState::default();
    state.enqueue_internal(
        PromptRequest::search_select(
            "Command Palette",
            vec![
                PromptOption::new("one", "One"),
                PromptOption::new("two", "Two"),
            ],
        )
        .modal_id("command-palette"),
        AttachInternalPromptAction::QuitSession,
    );
    let geometry = TerminalGeometry { cols: 80, rows: 24 };
    let _ = state
        .attach_prompt_overlay_render(geometry, &RuntimeAppearance::default(), false)
        .expect("palette should render");
    let active = state.active.as_ref().expect("active prompt");
    let hit = active
        .hits
        .regions()
        .iter()
        .find(|hit| hit.id.as_str() == "command-list.item.1")
        .expect("second option hit");
    let mouse = MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: hit.area.x,
        row: hit.area.y,
        modifiers: KeyModifiers::NONE,
    };

    let PromptKeyDisposition::Completed(completion) = state.handle_mouse_event(mouse, geometry)
    else {
        panic!("palette click should complete");
    };

    assert_eq!(
        completion.response,
        PromptResponse::Submitted(PromptValue::Single("two".to_owned()))
    );
}

#[test]
fn command_palette_uses_upper_third_placement() {
    let mut state = AttachPromptState::default();
    state.enqueue_internal(
        PromptRequest::search_select("Command Palette", vec![PromptOption::new("one", "One")])
            .modal_id("command-palette"),
        AttachInternalPromptAction::QuitSession,
    );

    let render = state
        .attach_prompt_overlay_render(
            TerminalGeometry {
                cols: 120,
                rows: 60,
            },
            &RuntimeAppearance::default(),
            false,
        )
        .expect("palette should render");
    let centered_y = (60_u16.saturating_sub(render.surface.rect.h)) / 2;

    assert!(render.surface.rect.y < centered_y);
    assert_eq!(
        render.surface.rect.y,
        (60_u16.saturating_sub(render.surface.rect.h)) / 3
    );
}

#[test]
fn extension_owned_finder_title_and_results_remain_inside_80_by_24_modal() {
    let mut state = AttachPromptState::default();
    state.enqueue_internal(
        PromptRequest::search_select(
            "Find Tab",
            vec![
                PromptOption::new("logs", "operations/logs"),
                PromptOption::new("build", "default/build"),
                PromptOption::new("api", "default/api"),
            ],
        )
        .message("Search tabs across all workspaces"),
        AttachInternalPromptAction::QuitSession,
    );
    for _ in 0..2 {
        let render = state
            .attach_prompt_overlay_render(
                TerminalGeometry { cols: 80, rows: 24 },
                &RuntimeAppearance::default(),
                true,
            )
            .expect("finder render");
        let rect = render.surface.rect;
        for expected in [
            "Find Tab",
            "Search tabs across all workspaces",
            "default/api",
        ] {
            assert!(
                render.ops.iter().any(|op| {
                    matches!(op, bmux_plugin::RenderOp::TextRun { x, y, text, .. }
                        if text.contains(expected)
                            && *x > rect.x && *x < rect.x + rect.w - 1
                            && *y > rect.y && *y < rect.y + rect.h - 1)
                }),
                "{expected} must survive extension border repaint"
            );
        }
        let active = state.active.as_ref().expect("finder active");
        assert!(active.list_viewport.y > rect.y + 1);
        assert!(active.hits.regions().iter().any(|hit| {
            hit.id.as_str() == "command-list.item.2"
                && hit.area.y > rect.y
                && hit.area.bottom() < rect.y + rect.h
        }));
    }
}

#[test]
fn extension_owned_prompt_chrome_suppresses_fallback_border() {
    let mut state = AttachPromptState::default();
    state.enqueue_internal(
        PromptRequest::text_input("Name"),
        AttachInternalPromptAction::QuitSession,
    );

    let render = state
        .attach_prompt_overlay_render(
            TerminalGeometry { cols: 80, rows: 24 },
            &RuntimeAppearance::default(),
            true,
        )
        .expect("prompt should render");
    let top_y = render.surface.rect.y;
    let top = render.ops.iter().find_map(|op| match op {
        bmux_plugin::RenderOp::TextRun { y, text, .. } if *y == top_y => Some(text.as_str()),
        _ => None,
    });

    assert!(top.is_some_and(|line| !line.contains('╭') && !line.contains('╮')));
}

#[test]
fn prompt_overlay_uses_rounded_tui_chrome_and_opaque_rows() {
    let mut state = AttachPromptState::default();
    state.enqueue_internal(
        PromptRequest::text_input("Name"),
        AttachInternalPromptAction::QuitSession,
    );

    let render = state
        .attach_prompt_overlay_render(
            TerminalGeometry { cols: 80, rows: 24 },
            &RuntimeAppearance::default(),
            false,
        )
        .expect("prompt should render");
    let top_y = render.surface.rect.y;
    let top_x = render.surface.rect.x;
    let top_runs = render
        .ops
        .iter()
        .filter_map(|op| match op {
            bmux_plugin::RenderOp::TextRun { x, y, text, .. } if *y == top_y => {
                Some((*x, text.as_str()))
            }
            _ => None,
        })
        .collect::<Vec<_>>();

    assert!(
        top_runs
            .first()
            .is_some_and(|(x, text)| *x == top_x && text.starts_with('╭'))
            && top_runs.last().is_some_and(|(_, text)| text.ends_with('╮'))
    );
    assert!(
        !render
            .ops
            .iter()
            .any(|op| matches!(op, bmux_plugin::RenderOp::Border { .. }))
    );
    assert!(
        (top_y..top_y.saturating_add(render.surface.rect.h)).all(|row| {
            render
                .ops
                .iter()
                .any(|op| matches!(op, bmux_plugin::RenderOp::TextRun { y, .. } if *y == row))
        })
    );
}

#[test]
fn runtime_appearance_maps_to_opaque_component_theme() {
    let appearance = RuntimeAppearance {
        foreground: "#112233".to_owned(),
        background: "#040506".to_owned(),
        cursor: "#aabbcc".to_owned(),
        selection_background: "#778899".to_owned(),
        ..RuntimeAppearance::default()
    };

    let theme = component_theme(&appearance);

    assert_eq!(theme.text.fg, Some(Color::Rgb(0x11, 0x22, 0x33)));
    assert_eq!(theme.surfaces.overlay.bg, Some(Color::Rgb(4, 5, 6)));
    assert_eq!(theme.focused.fg, Some(Color::Rgb(0xaa, 0xbb, 0xcc)));
    assert_eq!(theme.selected.bg, Some(Color::Rgb(0x77, 0x88, 0x99)));
    assert!(theme.surfaces.scrim.is_none());
}

#[test]
fn invalid_runtime_colors_use_safe_fallbacks() {
    assert_eq!(parse_tui_color("nope"), None);
    assert_eq!(parse_tui_color("#123"), None);
    let appearance = RuntimeAppearance {
        foreground: "invalid".to_owned(),
        background: "invalid".to_owned(),
        cursor: "invalid".to_owned(),
        selection_background: "invalid".to_owned(),
        ..RuntimeAppearance::default()
    };

    let theme = component_theme(&appearance);

    assert_eq!(theme.text.fg, Some(Color::BrightWhite));
    assert_eq!(theme.surfaces.overlay.bg, Some(Color::Black));
    assert_eq!(theme.focused.fg, Some(Color::BrightCyan));
    assert_eq!(theme.selected.bg, Some(Color::Cyan));
}

#[test]
fn confirm_prompt_submits_on_enter() {
    let mut state = AttachPromptState::default();
    state.enqueue_internal(
        PromptRequest::confirm("Quit?").confirm_default(true),
        AttachInternalPromptAction::QuitSession,
    );

    let outcome = state.handle_key_event(&key_event(KeyCode::Enter));
    let PromptKeyDisposition::Completed(completion) = outcome else {
        panic!("expected prompt completion");
    };
    assert_eq!(
        completion.response,
        PromptResponse::Submitted(PromptValue::Confirm(true))
    );
}

#[test]
fn text_input_prompt_accepts_multiline_unicode_paste() {
    let mut state = AttachPromptState::default();
    state.enqueue_internal(
        PromptRequest::text_input("Value"),
        AttachInternalPromptAction::QuitSession,
    );

    assert!(matches!(
        state.handle_paste("hello\r\n世界"),
        PromptKeyDisposition::Consumed
    ));
    let outcome = state.handle_key_event(&key_event(KeyCode::Enter));
    let PromptKeyDisposition::Completed(completion) = outcome else {
        panic!("expected prompt completion");
    };
    assert_eq!(
        completion.response,
        PromptResponse::Submitted(PromptValue::Text("hello\n世界".to_string()))
    );
}

#[test]
fn confirm_prompt_consumes_paste_without_changing_selection() {
    let mut state = AttachPromptState::default();
    state.enqueue_internal(
        PromptRequest::confirm("Continue?").confirm_default(true),
        AttachInternalPromptAction::QuitSession,
    );

    assert!(matches!(
        state.handle_paste("n"),
        PromptKeyDisposition::Consumed
    ));
    let outcome = state.handle_key_event(&key_event(KeyCode::Enter));
    let PromptKeyDisposition::Completed(completion) = outcome else {
        panic!("expected prompt completion");
    };
    assert_eq!(
        completion.response,
        PromptResponse::Submitted(PromptValue::Confirm(true))
    );
}

#[test]
fn text_input_prompt_accepts_typing_and_backspace() {
    let mut state = AttachPromptState::default();
    state.enqueue_internal(
        PromptRequest::text_input("Name").input_required(true),
        AttachInternalPromptAction::ClosePane {
            pane_id: Uuid::new_v4(),
        },
    );

    let _ = state.handle_key_event(&key_event(KeyCode::Char('h')));
    let _ = state.handle_key_event(&key_event(KeyCode::Char('i')));
    let _ = state.handle_key_event(&key_event(KeyCode::Backspace));
    let outcome = state.handle_key_event(&key_event(KeyCode::Enter));

    let PromptKeyDisposition::Completed(completion) = outcome else {
        panic!("expected prompt completion");
    };
    assert_eq!(
        completion.response,
        PromptResponse::Submitted(PromptValue::Text("h".to_string()))
    );
}

#[test]
fn optional_text_input_prompt_allows_blank_value_with_validation() {
    let mut state = AttachPromptState::default();
    state.enqueue_internal(
        PromptRequest::text_input("Count").input_validation(PromptValidation::Integer),
        AttachInternalPromptAction::QuitSession,
    );

    let outcome = state.handle_key_event(&key_event(KeyCode::Enter));

    let PromptKeyDisposition::Completed(completion) = outcome else {
        panic!("expected prompt completion");
    };
    assert_eq!(
        completion.response,
        PromptResponse::Submitted(PromptValue::Text(String::new()))
    );
}

#[test]
fn confirm_prompt_renders_dialog_actions() {
    let mut state = AttachPromptState::default();
    state.enqueue_internal(
        PromptRequest::confirm("Prompt Showcase")
            .confirm_default(true)
            .confirm_labels("Continue", "Stop"),
        AttachInternalPromptAction::QuitSession,
    );

    let render = state
        .attach_prompt_overlay_render(
            TerminalGeometry { cols: 80, rows: 24 },
            &RuntimeAppearance::default(),
            false,
        )
        .expect("confirm should render");
    let text = render
        .ops
        .iter()
        .filter_map(|op| match op {
            bmux_plugin::RenderOp::TextRun { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("");
    assert!(text.contains("Continue"), "rendered: {text:?}");
    assert!(text.contains("Stop"));
    assert!(!text.contains("[x]"));
    assert!(!text.contains("[ ]"));
}

#[test]
fn ordered_search_preserves_ranks_and_group_before_relevance() {
    use bmux_plugin_sdk::prompt::{PromptOrderedMatchMode, PromptSearchOrder};
    let mut options = vec![
        PromptOption::new("recent", "e-x"),
        PromptOption::new("exact", "ex"),
        PromptOption::new("current", "example"),
    ];
    for (index, option) in options.iter_mut().enumerate() {
        option.search_order = Some(PromptSearchOrder {
            group: u8::from(index == 2),
            initial: index,
            filtered: index,
        });
    }
    let mode = |matching, relevance| PromptSearchMatchMode::OrderedV1 {
        matching,
        relevance,
    };
    assert_eq!(
        filtered_option_indices(&options, "", mode(PromptOrderedMatchMode::Fuzzy, false)),
        vec![0, 1, 2]
    );
    assert_eq!(
        filtered_option_indices(&options, "ex", mode(PromptOrderedMatchMode::Fuzzy, false)),
        vec![0, 1, 2]
    );
    assert_eq!(
        filtered_option_indices(&options, "ex", mode(PromptOrderedMatchMode::Fuzzy, true)),
        vec![1, 0, 2]
    );
    for matching in [
        PromptOrderedMatchMode::Prefix,
        PromptOrderedMatchMode::Substring,
    ] {
        assert_eq!(
            filtered_option_indices(&options, "e", mode(matching, false)),
            vec![0, 1, 2]
        );
        assert_eq!(
            filtered_option_indices(&options, "missing", mode(matching, false)),
            [] as [usize; 0]
        );
    }
}

#[test]
fn search_selection_wraps_only_when_enabled() {
    assert_eq!(step_search_selection(0, 3, false, true), 2);
    assert_eq!(step_search_selection(2, 3, true, true), 0);
    assert_eq!(step_search_selection(0, 3, false, false), 0);
    assert_eq!(step_search_selection(2, 3, true, false), 2);
    assert_eq!(step_search_selection(1, 3, true, true), 2);
    assert_eq!(step_search_selection(1, 3, false, true), 0);
    for len in [0, 1] {
        for wrap in [false, true] {
            for forward in [false, true] {
                assert_eq!(step_search_selection(0, len, forward, wrap), 0);
            }
        }
    }
    let options = vec![
        PromptOption::new("a", "match"),
        PromptOption::new("b", "other"),
        PromptOption::new("c", "match"),
    ];
    let filtered = filtered_option_indices(&options, "match", PromptSearchMatchMode::Substring);
    assert_eq!(
        filtered[step_search_selection(0, filtered.len(), false, true)],
        2
    );
    assert_eq!(
        filtered[step_search_selection(1, filtered.len(), true, true)],
        0
    );
}

#[test]
fn command_palette_page_navigation_uses_rendered_virtual_viewport() {
    let mut state = AttachPromptState::default();
    state.enqueue_internal(
        PromptRequest::search_select(
            "Command Palette",
            (0..40)
                .map(|index| {
                    PromptOption::new(format!("command-{index}"), format!("Command {index}"))
                })
                .collect(),
        )
        .modal_id("command-palette"),
        AttachInternalPromptAction::QuitSession,
    );
    let geometry = TerminalGeometry { cols: 80, rows: 16 };
    let _ = state
        .attach_prompt_overlay_render(geometry, &RuntimeAppearance::default(), false)
        .expect("palette should render");

    let _ = state.handle_key_event(&key_event(KeyCode::PageDown));
    let _ = state.handle_key_event(&key_event(KeyCode::PageDown));
    // Selection is revealed against measured item geometry during rendering.
    let _ = state
        .attach_prompt_overlay_render(geometry, &RuntimeAppearance::default(), false)
        .expect("palette should render updated selection");
    let active = state.active.as_ref().expect("active prompt");
    let expected_index = match &active.state {
        PromptWidgetState::SearchSelect {
            selected,
            list,
            viewport_height,
            ..
        } => {
            assert!(*viewport_height > 0);
            assert!(list.scroll.vertical_offset() > 0);
            let PromptField::SearchSelect {
                options,
                match_mode,
                ..
            } = &active.envelope.request.field
            else {
                panic!("expected search-select field");
            };
            filtered_option_indices(options, "", *match_mode)[*selected]
        }
        _ => panic!("expected search-select state"),
    };
    let outcome = state.handle_key_event(&key_event(KeyCode::Enter));

    let PromptKeyDisposition::Completed(completion) = outcome else {
        panic!("expected prompt completion");
    };
    assert_eq!(
        completion.response,
        PromptResponse::Submitted(PromptValue::Single(format!("command-{expected_index}")))
    );
}

#[test]
fn search_select_overflow_resize_preserves_pointer_activation() {
    let mut state = AttachPromptState::default();
    state.enqueue_internal(
        PromptRequest::search_select(
            "Search",
            (0..30)
                .map(|index| {
                    PromptOption::new(
                        index.to_string(),
                        format!("Result {index:02} {}", "wrapped ".repeat(8)),
                    )
                })
                .collect(),
        ),
        AttachInternalPromptAction::QuitSession,
    );
    let narrow = TerminalGeometry { cols: 40, rows: 16 };
    state
        .attach_prompt_overlay_render(narrow, &RuntimeAppearance::default(), false)
        .unwrap();
    for _ in 0..40 {
        state.handle_key_event(&key_event(KeyCode::PageDown));
    }
    state
        .attach_prompt_overlay_render(narrow, &RuntimeAppearance::default(), false)
        .unwrap();
    let selected_key = |state: &AttachPromptState| {
        let active = state.active.as_ref().unwrap();
        let PromptWidgetState::SearchSelect {
            selected,
            list,
            viewport_height,
            ..
        } = &active.state
        else {
            panic!("search select");
        };
        assert_eq!(*selected, 29);
        assert!(list.total_height() > *viewport_height);
        assert!(list.scroll.vertical_offset() > 0);
        let PromptField::SearchSelect {
            options,
            match_mode,
            ..
        } = &active.envelope.request.field
        else {
            panic!("search select");
        };
        filtered_option_indices(options, "", *match_mode)[*selected]
    };
    let source = selected_key(&state);
    state.handle_key_event(&key_event(KeyCode::PageDown));
    assert_eq!(selected_key(&state), source);
    let wide = TerminalGeometry { cols: 80, rows: 24 };
    state
        .attach_prompt_overlay_render(wide, &RuntimeAppearance::default(), false)
        .unwrap();
    assert_eq!(selected_key(&state), source);
    let hit = state
        .active
        .as_ref()
        .unwrap()
        .hits
        .regions()
        .iter()
        .find(|hit| hit.id.as_str() == format!("command-list.item.{source}"))
        .expect("selected item remains visible after resize");
    let mouse = MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: hit.area.x,
        row: hit.area.y,
        modifiers: KeyModifiers::NONE,
    };
    let PromptKeyDisposition::Completed(completion) = state.handle_mouse_event(mouse, wide) else {
        panic!("click completes");
    };
    assert_eq!(
        completion.response,
        PromptResponse::Submitted(PromptValue::Single(source.to_string()))
    );
}

#[test]
fn search_select_width_tracks_filter_overflow_transitions() {
    use super::{
        LayoutCx, Line, Text, TextBlock, VirtualList, VirtualListState, sync_search_select_width,
    };
    let mut probe = VirtualListState::new(0);
    let mut painted = VirtualListState::new(0);
    let short = VirtualList::new("command-list").component(
        0,
        TextBlock::new(Text::from_lines([Line::raw("abcde")])).id("label"),
    );
    assert_eq!(
        sync_search_select_width(&short, &mut probe, &mut painted, 5, 2),
        5
    );
    let long = short.component(
        1,
        TextBlock::new(Text::from_lines([Line::raw("abcdefghij")])).id("other"),
    );
    assert_eq!(
        sync_search_select_width(&long, &mut probe, &mut painted, 5, 2),
        4
    );
    assert_eq!(painted.item_height(&0), Some(2));
    assert_eq!(painted.item_height(&1), Some(3));
    let short = VirtualList::new("command-list").component(
        0,
        TextBlock::new(Text::from_lines([Line::raw("abcde")])).id("label"),
    );
    assert_eq!(
        sync_search_select_width(&short, &mut probe, &mut painted, 5, 2),
        5
    );
    assert_eq!(painted.item_height(&0), Some(1));
    assert_eq!(painted.item_height(&1), None);
    // Same resolved width remains a cache hit.
    let misses = painted.layout_cache().stats().misses;
    short.sync(5, &mut painted, &mut LayoutCx::new());
    assert_eq!(painted.layout_cache().stats().misses, misses);
}

#[test]
fn search_select_reintroduced_results_are_measured_before_paging() {
    let mut state = AttachPromptState::default();
    state.enqueue_internal(
        PromptRequest::search_select(
            "Search",
            (0..30)
                .map(|index| PromptOption::new(index.to_string(), format!("Result {index:02}")))
                .collect(),
        ),
        AttachInternalPromptAction::QuitSession,
    );
    state.handle_key_event(&key_event(KeyCode::Char('9')));
    state
        .attach_prompt_overlay_render(
            TerminalGeometry { cols: 80, rows: 24 },
            &RuntimeAppearance::default(),
            false,
        )
        .unwrap();
    state.handle_key_event(&key_event(KeyCode::Backspace));
    state.handle_key_event(&key_event(KeyCode::PageDown));
    let PromptWidgetState::SearchSelect { selected, list, .. } =
        &state.active.as_ref().unwrap().state
    else {
        panic!("search select");
    };
    let PromptWidgetState::SearchSelect {
        viewport_height, ..
    } = &state.active.as_ref().unwrap().state
    else {
        unreachable!()
    };
    assert_eq!(
        *selected,
        usize::try_from((*viewport_height).min(29)).unwrap(),
        "one-row results page by exactly the viewport height"
    );
    assert!(
        list.item_height(&0).is_some(),
        "reintroduced option measured before repaint"
    );
}

#[test]
fn command_palette_enter_submits_literal_pause_not_scattered_letters() {
    let mut state = AttachPromptState::default();
    state.enqueue_internal(
        PromptRequest::search_select(
            "Command Palette",
            vec![
                PromptOption::new("wrong", "p-a-u-s-e"),
                PromptOption::new("prefix", "pauser"),
                PromptOption::new("pause-action", "Output: pause"),
            ],
        )
        .modal_id("command-palette")
        .search_match_mode(PromptSearchMatchMode::OrderedV1 {
            matching: bmux_plugin_sdk::prompt::PromptOrderedMatchMode::Fuzzy,
            relevance: true,
        }),
        AttachInternalPromptAction::QuitSession,
    );
    for ch in "pause".chars() {
        let _ = state.handle_key_event(&key_event(KeyCode::Char(ch)));
    }
    let PromptKeyDisposition::Completed(completion) =
        state.handle_key_event(&key_event(KeyCode::Enter))
    else {
        panic!("expected prompt completion");
    };
    assert_eq!(
        completion.response,
        PromptResponse::Submitted(PromptValue::Single("pause-action".into()))
    );
}

#[test]
fn search_select_query_supports_cursor_editing() {
    let mut state = AttachPromptState::default();
    state.enqueue_internal(
        PromptRequest::search_select(
            "Command",
            vec![
                PromptOption::new("commit", "Commit"),
                PromptOption::new("compact", "Compact"),
                PromptOption::new("connect", "Connect"),
            ],
        ),
        AttachInternalPromptAction::QuitSession,
    );

    for ch in "cmmit".chars() {
        let _ = state.handle_key_event(&key_event(KeyCode::Char(ch)));
    }
    let _ = state.handle_key_event(&modified_key_event(KeyCode::Left, KeyModifiers::ALT));
    let _ = state.handle_key_event(&key_event(KeyCode::Right));
    let _ = state.handle_key_event(&key_event(KeyCode::Char('o')));
    let outcome = state.handle_key_event(&key_event(KeyCode::Enter));

    let PromptKeyDisposition::Completed(completion) = outcome else {
        panic!("expected prompt completion");
    };
    assert_eq!(
        completion.response,
        PromptResponse::Submitted(PromptValue::Single("commit".to_string()))
    );
}

#[test]
fn pasted_text_preserves_component_cursor_across_widths() {
    let mut state = AttachPromptState::default();
    state.enqueue_internal(
        PromptRequest::text_input("Value"),
        AttachInternalPromptAction::QuitSession,
    );
    assert!(matches!(
        state.handle_paste("abcdefghij"),
        PromptKeyDisposition::Consumed
    ));

    let narrow = state
        .attach_prompt_overlay_render(
            TerminalGeometry { cols: 24, rows: 8 },
            &RuntimeAppearance::default(),
            false,
        )
        .expect("narrow input should render");
    let wide = state
        .attach_prompt_overlay_render(
            TerminalGeometry { cols: 80, rows: 24 },
            &RuntimeAppearance::default(),
            false,
        )
        .expect("wide input should render");

    assert!(narrow.cursor_state.is_some());
    assert!(wide.cursor_state.is_some());
    assert!(
        wide.cursor_state.expect("wide cursor").x >= narrow.cursor_state.expect("narrow cursor").x
    );
}

#[test]
fn single_select_wrapped_row_click_uses_retained_item_geometry() {
    let mut state = AttachPromptState::default();
    state.enqueue_internal(
        PromptRequest::single_select(
            "Choose",
            vec![
                PromptOption::new(
                    "first",
                    "A long option label that wraps across several terminal rows",
                ),
                PromptOption::new("second", "Second"),
            ],
        ),
        AttachInternalPromptAction::QuitSession,
    );
    let geometry = TerminalGeometry { cols: 28, rows: 20 };
    let mut previous = None;
    let mut previous_misses = None;
    for _ in 0..2 {
        state
            .attach_prompt_overlay_render(geometry, &RuntimeAppearance::default(), false)
            .expect("render");
        let active = state.active.as_ref().expect("active");
        let region = active
            .hits
            .regions()
            .iter()
            .find(|hit| hit.id.as_str() == "single-select-list.item.0")
            .expect("first item");
        assert!(region.area.height > 1, "fixture must wrap");
        if let Some(area) = previous {
            assert_eq!(region.area, area);
        }
        previous = Some(region.area);
        let PromptWidgetState::SingleSelect { list, .. } = &active.state else {
            panic!("single select");
        };
        assert!(list.total_height() > 2);
        let misses = list.layout_cache().stats().misses;
        assert!(misses > 0);
        if let Some(previous) = previous_misses {
            assert_eq!(
                misses, previous,
                "unchanged render must reuse measured layouts"
            );
        }
        previous_misses = Some(misses);
    }
    let area = previous.expect("painted item");
    let outcome = state.handle_mouse_event(
        MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: area.x,
            row: area.y + 1,
            modifiers: KeyModifiers::NONE,
        },
        geometry,
    );
    let PromptKeyDisposition::Completed(completion) = outcome else {
        panic!("expected completion");
    };
    assert_eq!(
        completion.response,
        PromptResponse::Submitted(PromptValue::Single("first".to_string()))
    );
}

#[test]
fn single_select_prompt_moves_with_arrow_keys() {
    let mut state = AttachPromptState::default();
    state.enqueue_internal(
        PromptRequest::single_select(
            "Layout",
            vec![
                PromptOption::new("tall", "Tall"),
                PromptOption::new("wide", "Wide"),
                PromptOption::new("grid", "Grid"),
            ],
        ),
        AttachInternalPromptAction::ClosePane {
            pane_id: Uuid::new_v4(),
        },
    );

    let _ = state.handle_key_event(&key_event(KeyCode::Down));
    let outcome = state.handle_key_event(&key_event(KeyCode::Enter));

    let PromptKeyDisposition::Completed(completion) = outcome else {
        panic!("expected prompt completion");
    };
    assert_eq!(
        completion.response,
        PromptResponse::Submitted(PromptValue::Single("wide".to_string()))
    );
}

#[test]
fn multi_toggle_reveals_selection_beyond_viewport() {
    let mut state = AttachPromptState::default();
    state.enqueue_internal(
        PromptRequest::multi_toggle(
            "Features",
            (0..30)
                .map(|i| PromptOption::new(format!("option-{i}"), format!("Option {i}")))
                .collect(),
        ),
        AttachInternalPromptAction::QuitSession,
    );
    for _ in 0..29 {
        state.handle_key_event(&key_event(KeyCode::Down));
    }
    for geometry in [
        TerminalGeometry { cols: 80, rows: 16 },
        TerminalGeometry { cols: 40, rows: 10 },
        TerminalGeometry {
            cols: 100,
            rows: 24,
        },
    ] {
        state
            .attach_prompt_overlay_render(geometry, &RuntimeAppearance::default(), false)
            .expect("render");
        let active = state.active.as_ref().expect("active");
        let area = active
            .hits
            .regions()
            .iter()
            .find(|hit| hit.id.as_str() == "prompt.multi-toggle.29")
            .expect("focused checkbox must be visible after resize")
            .area;
        assert!(area.height > 0 && area.width > 0);
        assert!(area.bottom() < geometry.rows);
        let misses = active.scroll_layout.stats().misses;
        assert_eq!(active.scroll_layout.len(), 1);
        state
            .attach_prompt_overlay_render(geometry, &RuntimeAppearance::default(), false)
            .expect("repeat render");
        let cache = &state.active.as_ref().expect("active").scroll_layout;
        assert_eq!(cache.stats().misses, misses);
        assert!(cache.stats().hits > 0);
    }
    assert!(
        state
            .active
            .as_ref()
            .expect("active")
            .scroll_layout
            .stats()
            .released
            >= 2
    );
    let PromptWidgetState::MultiToggle { cursor, scroll, .. } =
        &state.active.as_ref().expect("active").state
    else {
        panic!("multi toggle");
    };
    assert_eq!(*cursor, 29);
    assert!(scroll.vertical_offset() > 0);
    state.handle_key_event(&key_event(KeyCode::Char(' ')));
    let PromptKeyDisposition::Completed(completion) =
        state.handle_key_event(&key_event(KeyCode::Enter))
    else {
        panic!("completion");
    };
    assert_eq!(
        completion.response,
        PromptResponse::Submitted(PromptValue::Multi(vec!["option-29".to_string()]))
    );
}

#[test]
fn multi_toggle_prompt_moves_with_arrow_keys_and_toggles_selection() {
    let mut state = AttachPromptState::default();
    state.enqueue_internal(
        PromptRequest::multi_toggle(
            "Features",
            vec![
                PromptOption::new("line-numbers", "Line numbers"),
                PromptOption::new("timestamps", "Timestamps"),
                PromptOption::new("soft-wrap", "Soft wrap"),
            ],
        )
        .multi_min_selected(1),
        AttachInternalPromptAction::ClosePane {
            pane_id: Uuid::new_v4(),
        },
    );

    let _ = state.handle_key_event(&key_event(KeyCode::Down));
    let _ = state.handle_key_event(&key_event(KeyCode::Char(' ')));
    let outcome = state.handle_key_event(&key_event(KeyCode::Enter));

    let PromptKeyDisposition::Completed(completion) = outcome else {
        panic!("expected prompt completion");
    };
    assert_eq!(
        completion.response,
        PromptResponse::Submitted(PromptValue::Multi(vec!["timestamps".to_string()]))
    );
}
