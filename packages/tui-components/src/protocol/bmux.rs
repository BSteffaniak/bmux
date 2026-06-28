//! Built-in open protocol bindings for components exported by this crate.
//!
//! This module is intentionally adapter glue: protocol props are decoded into
//! `bmux_tui_components` data/state types and rendering/event handling is
//! delegated to the existing components.

use bmux_keyboard::KeyCode;
use bmux_text_edit::TextEditBuffer;
use bmux_tui::event::Event;
use bmux_tui::frame::Frame;
use bmux_tui::geometry::Rect;
use bmux_tui::prelude::Line;
use bmux_tui_component_protocol::event::{ComponentEvent, ComponentEventKind};
use bmux_tui_component_protocol::ids::{ActionId, ComponentTypeId};
use bmux_tui_component_protocol::model::ComponentNode;
use bmux_tui_component_protocol::state::ComponentRuntimeState;
use bmux_tui_component_protocol::value::ComponentValue;

use crate::action_row::{ActionButton, ActionRow, ActionRowOutcome, ActionRowState};
use crate::button::{Button, ButtonState};
use crate::checkbox::{Checkbox, CheckboxOutcome, CheckboxState};
use crate::empty_state::EmptyState;
use crate::form_field::FormField;
use crate::progress_bar::{ProgressBar, ProgressBarValue};
use crate::protocol::{
    ProtocolBindings, ProtocolComponentBinding, ProtocolLocalStateKey, ProtocolRenderContext,
    ProtocolRuntime,
};
use crate::radio_group::{RadioGroup, RadioGroupOutcome, RadioGroupState, RadioOption};
use crate::select_dropdown::{
    SelectDropdown, SelectDropdownOutcome, SelectDropdownState, SelectOption,
};
use crate::text_input::{TextInputPolicy, TextInputState};
use crate::text_input_box::{TextInputBox, TextInputBoxOutcome, TextInputBoxPolicy};
use crate::text_view::{TextView, TextViewState};

/// Open component type id for `action_row`.
pub const ACTION_ROW_TYPE_ID: &str = "bmux.action_row";
/// Open component type id for `badge`.
pub const BADGE_TYPE_ID: &str = "bmux.badge";
/// Open component type id for `bar_chart`.
pub const BAR_CHART_TYPE_ID: &str = "bmux.bar_chart";
/// Open component type id for `breadcrumbs`.
pub const BREADCRUMBS_TYPE_ID: &str = "bmux.breadcrumbs";
/// Open component type id for `button`.
pub const BUTTON_TYPE_ID: &str = "bmux.button";
/// Open component type id for `canvas`.
pub const CANVAS_TYPE_ID: &str = "bmux.canvas";
/// Open component type id for `chart`.
pub const CHART_TYPE_ID: &str = "bmux.chart";
/// Open component type id for `checkbox`.
pub const CHECKBOX_TYPE_ID: &str = "bmux.checkbox";
/// Open component type id for `checkbox_group`.
pub const CHECKBOX_GROUP_TYPE_ID: &str = "bmux.checkbox_group";
/// Open component type id for `dialog`.
pub const DIALOG_TYPE_ID: &str = "bmux.dialog";
/// Open component type id for `empty_state`.
pub const EMPTY_STATE_TYPE_ID: &str = "bmux.empty_state";
/// Open component type id for `filtered_list`.
pub const FILTERED_LIST_TYPE_ID: &str = "bmux.filtered_list";
/// Open component type id for `form`.
pub const FORM_TYPE_ID: &str = "bmux.form";
/// Open component type id for `form_field`.
pub const FORM_FIELD_TYPE_ID: &str = "bmux.form_field";
/// Open component type id for `key_hint_bar`.
pub const KEY_HINT_BAR_TYPE_ID: &str = "bmux.key_hint_bar";
/// Open component type id for `labeled_details`.
pub const LABELED_DETAILS_TYPE_ID: &str = "bmux.labeled_details";
/// Open component type id for `menu`.
pub const MENU_TYPE_ID: &str = "bmux.menu";
/// Open component type id for `modal_frame`.
pub const MODAL_FRAME_TYPE_ID: &str = "bmux.modal_frame";
/// Open component type id for `pane`.
pub const PANE_TYPE_ID: &str = "bmux.pane";
/// Open component type id for `panel_group`.
pub const PANEL_GROUP_TYPE_ID: &str = "bmux.panel_group";
/// Open component type id for `picker_frame`.
pub const PICKER_FRAME_TYPE_ID: &str = "bmux.picker_frame";
/// Open component type id for `progress_bar`.
pub const PROGRESS_BAR_TYPE_ID: &str = "bmux.progress_bar";
/// Open component type id for `radio_group`.
pub const RADIO_GROUP_TYPE_ID: &str = "bmux.radio_group";
/// Open component type id for `scroll_area`.
pub const SCROLL_AREA_TYPE_ID: &str = "bmux.scroll_area";
/// Open component type id for `scrollbar`.
pub const SCROLLBAR_TYPE_ID: &str = "bmux.scrollbar";
/// Open component type id for `scrollbar_layout`.
pub const SCROLLBAR_LAYOUT_TYPE_ID: &str = "bmux.scrollbar_layout";
/// Open component type id for `select_dropdown`.
pub const SELECT_DROPDOWN_TYPE_ID: &str = "bmux.select_dropdown";
/// Open component type id for `selectable_list`.
pub const SELECTABLE_LIST_TYPE_ID: &str = "bmux.selectable_list";
/// Open component type id for `sparkline`.
pub const SPARKLINE_TYPE_ID: &str = "bmux.sparkline";
/// Open component type id for `status_bar`.
pub const STATUS_BAR_TYPE_ID: &str = "bmux.status_bar";
/// Open component type id for `stepper`.
pub const STEPPER_TYPE_ID: &str = "bmux.stepper";
/// Open component type id for `tab_bar`.
pub const TAB_BAR_TYPE_ID: &str = "bmux.tab_bar";
/// Open component type id for `table`.
pub const TABLE_TYPE_ID: &str = "bmux.table";
/// Open component type id for `text_input`.
pub const TEXT_INPUT_TYPE_ID: &str = "bmux.text_input";
/// Open component type id for `text_input_box`.
pub const TEXT_INPUT_BOX_TYPE_ID: &str = "bmux.text_input_box";
/// Open component type id for `text_view`.
pub const TEXT_VIEW_TYPE_ID: &str = "bmux.text_view";
/// Open component type id for `toast_stack`.
pub const TOAST_STACK_TYPE_ID: &str = "bmux.toast_stack";
/// Open component type id for `tree_view`.
pub const TREE_VIEW_TYPE_ID: &str = "bmux.tree_view";

const ALL_TYPE_IDS: &[&str] = &[
    ACTION_ROW_TYPE_ID,
    BADGE_TYPE_ID,
    BAR_CHART_TYPE_ID,
    BREADCRUMBS_TYPE_ID,
    BUTTON_TYPE_ID,
    CANVAS_TYPE_ID,
    CHART_TYPE_ID,
    CHECKBOX_TYPE_ID,
    CHECKBOX_GROUP_TYPE_ID,
    DIALOG_TYPE_ID,
    EMPTY_STATE_TYPE_ID,
    FILTERED_LIST_TYPE_ID,
    FORM_TYPE_ID,
    FORM_FIELD_TYPE_ID,
    KEY_HINT_BAR_TYPE_ID,
    LABELED_DETAILS_TYPE_ID,
    MENU_TYPE_ID,
    MODAL_FRAME_TYPE_ID,
    PANE_TYPE_ID,
    PANEL_GROUP_TYPE_ID,
    PICKER_FRAME_TYPE_ID,
    PROGRESS_BAR_TYPE_ID,
    RADIO_GROUP_TYPE_ID,
    SCROLL_AREA_TYPE_ID,
    SCROLLBAR_TYPE_ID,
    SCROLLBAR_LAYOUT_TYPE_ID,
    SELECT_DROPDOWN_TYPE_ID,
    SELECTABLE_LIST_TYPE_ID,
    SPARKLINE_TYPE_ID,
    STATUS_BAR_TYPE_ID,
    STEPPER_TYPE_ID,
    TAB_BAR_TYPE_ID,
    TABLE_TYPE_ID,
    TEXT_INPUT_TYPE_ID,
    TEXT_INPUT_BOX_TYPE_ID,
    TEXT_VIEW_TYPE_ID,
    TOAST_STACK_TYPE_ID,
    TREE_VIEW_TYPE_ID,
];

/// Register built-in open bindings for every component type id exported by this crate.
///
/// Type ids without a native adapter render an explicit unsupported message rather than
/// reimplementing component drawing in protocol glue.
pub fn register_bmux_components(bindings: &mut ProtocolBindings) {
    for type_id in ALL_TYPE_IDS {
        bindings.register_component(*type_id, BmuxComponentBinding::new(*type_id));
    }
}

/// Return a binding registry populated with built-in BMUX component bindings.
#[must_use]
pub fn bmux_component_bindings() -> ProtocolBindings {
    let mut bindings = ProtocolBindings::new();
    register_bmux_components(&mut bindings);
    bindings
}

#[derive(Debug, Clone)]
struct BmuxComponentBinding {
    type_id: ComponentTypeId,
}

impl BmuxComponentBinding {
    #[must_use]
    fn new(type_id: impl Into<ComponentTypeId>) -> Self {
        Self {
            type_id: type_id.into(),
        }
    }
}

impl ProtocolComponentBinding for BmuxComponentBinding {
    fn render_with_context(
        &self,
        node: &ComponentNode,
        area: Rect,
        context: &mut ProtocolRenderContext<'_, '_>,
    ) {
        let props = component_props(node);
        match self.type_id.as_str() {
            ACTION_ROW_TYPE_ID => {
                let (runtime, frame) = context.runtime_and_frame();
                render_action_row_with_runtime(props, runtime, node, area, frame);
            }
            FORM_FIELD_TYPE_ID => render_form_field_with_child(node, props, area, context),
            FORM_TYPE_ID => render_form(node, area, context),
            RADIO_GROUP_TYPE_ID => {
                let (runtime, frame) = context.runtime_and_frame();
                render_radio_group_with_runtime(props, runtime, node, area, frame);
            }
            SELECT_DROPDOWN_TYPE_ID => {
                let (runtime, frame) = context.runtime_and_frame();
                render_select_dropdown_with_runtime(props, runtime, node, area, frame);
            }
            TEXT_INPUT_TYPE_ID | TEXT_INPUT_BOX_TYPE_ID => {
                let (runtime, frame) = context.runtime_and_frame();
                render_text_input_with_runtime(props, runtime, node, area, frame);
            }
            _ => {
                let state = context.state().clone();
                self.render(node, &state, area, context.frame());
            }
        }
    }

    fn render(
        &self,
        node: &ComponentNode,
        state: &ComponentRuntimeState,
        area: Rect,
        frame: &mut Frame<'_>,
    ) {
        let props = component_props(node);
        match self.type_id.as_str() {
            ACTION_ROW_TYPE_ID => render_action_row(props, area, frame),
            BUTTON_TYPE_ID => render_button(props, area, frame),
            CHECKBOX_TYPE_ID => render_checkbox(props, state, node, area, frame),
            EMPTY_STATE_TYPE_ID => render_empty_state(props, area, frame),
            PROGRESS_BAR_TYPE_ID => render_progress_bar(props, area, frame),
            RADIO_GROUP_TYPE_ID => render_radio_group(props, state, node, area, frame),
            SELECT_DROPDOWN_TYPE_ID => render_select_dropdown(props, state, node, area, frame),
            TEXT_INPUT_TYPE_ID | TEXT_INPUT_BOX_TYPE_ID => {
                render_text_input(props, state, node, area, frame);
            }
            TEXT_VIEW_TYPE_ID => render_text_view(props, area, frame),
            type_id => render_unsupported(type_id, area, frame),
        }
    }

    fn handle_event_with_context(
        &self,
        node: &ComponentNode,
        area: Rect,
        event: &Event,
        context: &mut crate::protocol::ProtocolEventContext<'_>,
    ) -> Vec<ComponentEvent> {
        let props = component_props(node);
        match self.type_id.as_str() {
            ACTION_ROW_TYPE_ID => {
                handle_action_row_with_runtime(props, node, context.runtime(), area, event)
            }
            FORM_FIELD_TYPE_ID => handle_form_field_with_context(node, props, area, event, context),
            FORM_TYPE_ID => handle_form_with_context(node, area, event, context),
            RADIO_GROUP_TYPE_ID => {
                handle_radio_group_with_runtime(props, node, context.runtime(), area, event)
            }
            SELECT_DROPDOWN_TYPE_ID => {
                handle_select_dropdown_with_runtime(props, node, context.runtime(), area, event)
            }
            TEXT_INPUT_TYPE_ID | TEXT_INPUT_BOX_TYPE_ID => {
                handle_text_input_with_runtime(props, node, context.runtime(), area, event)
            }
            _ => self.handle_event(node, context.state(), area, event),
        }
    }

    fn handle_event(
        &self,
        node: &ComponentNode,
        state: &mut ComponentRuntimeState,
        area: Rect,
        event: &Event,
    ) -> Vec<ComponentEvent> {
        let props = component_props(node);
        match self.type_id.as_str() {
            ACTION_ROW_TYPE_ID => handle_action_row(props, node, area, event),
            BUTTON_TYPE_ID => handle_button(props, node, event),
            CHECKBOX_TYPE_ID => handle_checkbox(props, node, state, area, event),
            RADIO_GROUP_TYPE_ID => handle_radio_group(props, node, state, area, event),
            SELECT_DROPDOWN_TYPE_ID => handle_select_dropdown(props, node, state, area, event),
            TEXT_INPUT_TYPE_ID | TEXT_INPUT_BOX_TYPE_ID => {
                handle_text_input(props, node, state, area, event)
            }
            _ => Vec::new(),
        }
    }
}

const fn component_props(node: &ComponentNode) -> &ComponentValue {
    match &node.kind {
        bmux_tui_component_protocol::model::ComponentKind::Component { props, .. } => props,
        bmux_tui_component_protocol::model::ComponentKind::Extension { payload, .. } => payload,
        _ => &ComponentValue::Null,
    }
}

fn render_action_row_with_runtime(
    props: &ComponentValue,
    runtime: &mut ProtocolRuntime,
    node: &ComponentNode,
    area: Rect,
    frame: &mut Frame<'_>,
) {
    let actions = action_buttons(props);
    let state_key = local_state_key(node, ComponentTypeId::new(ACTION_ROW_TYPE_ID));
    let row_state = runtime.local_state_or_insert_with(&state_key, ActionRowState::new);
    if row_state.focused().is_none() && !actions.is_empty() {
        row_state.set_focused(Some(0));
    }
    ActionRow::new(&actions).render_state(area, row_state, frame);
}

fn render_action_row(props: &ComponentValue, area: Rect, frame: &mut Frame<'_>) {
    let actions = action_buttons(props);
    let mut state = ActionRowState::new();
    state.set_focused(Some(0));
    ActionRow::new(&actions).render_state(area, &state, frame);
}

fn render_button(props: &ComponentValue, area: Rect, frame: &mut Frame<'_>) {
    let label = string_prop(props, "label").unwrap_or("button");
    Button::new(label).render(area, &ButtonState::new(), frame);
}

fn render_checkbox(
    props: &ComponentValue,
    state: &ComponentRuntimeState,
    node: &ComponentNode,
    area: Rect,
    frame: &mut Frame<'_>,
) {
    let label = string_prop(props, "label").unwrap_or("checkbox");
    let checked = node_value_bool(state, node)
        .unwrap_or_else(|| bool_prop(props, "checked").unwrap_or(false));
    let mut checkbox_state = CheckboxState::new(checked);
    checkbox_state.set_focused(is_focused(state, node));
    checkbox_state.set_disabled(bool_prop(props, "disabled").unwrap_or(false));
    Checkbox::new(label).render(area, &checkbox_state, frame);
}

fn render_radio_group(
    props: &ComponentValue,
    state: &ComponentRuntimeState,
    node: &ComponentNode,
    area: Rect,
    frame: &mut Frame<'_>,
) {
    let options = radio_options(props);
    let selected = selected_option_index(props, state, node, &options);
    let mut group_state = RadioGroupState::new(selected);
    group_state.set_focused(selected.or(Some(0)));
    group_state.set_disabled(bool_prop(props, "disabled").unwrap_or(false));
    RadioGroup::new(&options).render(area, &group_state, frame);
}

fn render_select_dropdown_with_runtime(
    props: &ComponentValue,
    runtime: &mut ProtocolRuntime,
    node: &ComponentNode,
    area: Rect,
    frame: &mut Frame<'_>,
) {
    let options = select_options(props);
    let selected = selected_select_index(props, runtime.state(), node, &options);
    let state_key = local_state_key(node, ComponentTypeId::new(SELECT_DROPDOWN_TYPE_ID));
    let select_state =
        runtime.local_state_or_insert_with(&state_key, || SelectDropdownState::new(selected));
    if selected != select_state.selected() {
        select_state.set_selected(selected);
    }
    select_state.set_disabled(bool_prop(props, "disabled").unwrap_or(false));
    let select = SelectDropdown::new(&options)
        .placeholder(string_prop(props, "placeholder").unwrap_or("Select..."));
    select.render(area, select_state, frame);
}
fn render_select_dropdown(
    props: &ComponentValue,
    state: &ComponentRuntimeState,
    node: &ComponentNode,
    area: Rect,
    frame: &mut Frame<'_>,
) {
    let options = select_options(props);
    let selected = selected_select_index(props, state, node, &options);
    let mut select_state = SelectDropdownState::new(selected);
    select_state.set_open(bool_prop(props, "open").unwrap_or(false));
    select_state.set_disabled(bool_prop(props, "disabled").unwrap_or(false));
    let select = SelectDropdown::new(&options)
        .placeholder(string_prop(props, "placeholder").unwrap_or("Select..."));
    select.render(area, &select_state, frame);
}
fn render_radio_group_with_runtime(
    props: &ComponentValue,
    runtime: &mut ProtocolRuntime,
    node: &ComponentNode,
    area: Rect,
    frame: &mut Frame<'_>,
) {
    let options = radio_options(props);
    let selected = selected_option_index(props, runtime.state(), node, &options);
    let state_key = local_state_key(node, ComponentTypeId::new(RADIO_GROUP_TYPE_ID));
    let group_state =
        runtime.local_state_or_insert_with(&state_key, || RadioGroupState::new(selected));
    if selected != group_state.selected() {
        group_state.set_selected(selected);
    }
    group_state.set_disabled(bool_prop(props, "disabled").unwrap_or(false));
    RadioGroup::new(&options).render(area, group_state, frame);
}
fn render_empty_state(props: &ComponentValue, area: Rect, frame: &mut Frame<'_>) {
    let title = string_prop(props, "title").unwrap_or("Empty");
    let body = lines_prop(props, "body")
        .or_else(|| string_prop(props, "message").map(|message| vec![Line::from(message)]))
        .unwrap_or_default();
    let actions = lines_prop(props, "actions").unwrap_or_default();
    let mut empty = EmptyState::new(title).body(&body).actions(&actions);
    if let Some(icon) = string_prop(props, "icon") {
        empty = empty.icon(icon);
    }
    empty.render(area, frame);
}

fn render_form_field_with_child(
    node: &ComponentNode,
    props: &ComponentValue,
    area: Rect,
    context: &mut ProtocolRenderContext<'_, '_>,
) {
    let control = render_form_field_chrome(props, area, context.frame());
    if let Some(child) = node.children.first() {
        context.render_child(child, control);
    }
}

fn render_form(node: &ComponentNode, area: Rect, context: &mut ProtocolRenderContext<'_, '_>) {
    let child_count = u16::try_from(node.children.len())
        .unwrap_or(u16::MAX)
        .max(1);
    let row_height = area.height.saturating_div(child_count).max(1);
    for (index, child) in node.children.iter().enumerate() {
        let row = u16::try_from(index).unwrap_or(u16::MAX);
        let y = area.y.saturating_add(row.saturating_mul(row_height));
        let remaining = area.bottom().saturating_sub(y);
        if remaining == 0 {
            break;
        }
        let height = if index + 1 == node.children.len() {
            remaining
        } else {
            row_height.min(remaining)
        };
        context.render_child(child, Rect::new(area.x, y, area.width, height));
    }
}

fn handle_form_with_context(
    node: &ComponentNode,
    area: Rect,
    event: &Event,
    context: &mut crate::protocol::ProtocolEventContext<'_>,
) -> Vec<ComponentEvent> {
    let child_count = u16::try_from(node.children.len())
        .unwrap_or(u16::MAX)
        .max(1);
    let row_height = area.height.saturating_div(child_count).max(1);
    let mut output = Vec::new();
    for (index, child) in node.children.iter().enumerate() {
        let row = u16::try_from(index).unwrap_or(u16::MAX);
        let y = area.y.saturating_add(row.saturating_mul(row_height));
        let remaining = area.bottom().saturating_sub(y);
        if remaining == 0 {
            break;
        }
        let height = if index + 1 == node.children.len() {
            remaining
        } else {
            row_height.min(remaining)
        };
        output.extend(context.handle_child_event(
            child,
            Rect::new(area.x, y, area.width, height),
            event,
        ));
    }
    output
}

fn handle_form_field_with_context(
    node: &ComponentNode,
    props: &ComponentValue,
    area: Rect,
    event: &Event,
    context: &mut crate::protocol::ProtocolEventContext<'_>,
) -> Vec<ComponentEvent> {
    let label = string_prop(props, "label").unwrap_or("Field");
    let mut field = FormField::new(label).required(bool_prop(props, "required").unwrap_or(false));
    if let Some(help) = string_prop(props, "help") {
        field = field.help(help);
    }
    if let Some(error) = string_prop(props, "error") {
        field = field.error(error);
    }
    let control = field.layout(area).control;
    node.children.first().map_or_else(Vec::new, |child| {
        context.handle_child_event(child, control, event)
    })
}

fn render_form_field_chrome(props: &ComponentValue, area: Rect, frame: &mut Frame<'_>) -> Rect {
    let label = string_prop(props, "label").unwrap_or("Field");
    let mut field = FormField::new(label).required(bool_prop(props, "required").unwrap_or(false));
    if let Some(help) = string_prop(props, "help") {
        field = field.help(help);
    }
    if let Some(error) = string_prop(props, "error") {
        field = field.error(error);
    }
    field.render(area, frame)
}

fn render_progress_bar(props: &ComponentValue, area: Rect, frame: &mut Frame<'_>) {
    let value = number_prop(props, "value").unwrap_or(0);
    let total = number_prop(props, "total")
        .or_else(|| number_prop(props, "max"))
        .unwrap_or(100);
    let progress = if bool_prop(props, "indeterminate").unwrap_or(false) {
        ProgressBar::new(ProgressBarValue::indeterminate(
            u16::try_from(value).unwrap_or(u16::MAX),
        ))
    } else {
        ProgressBar::new(ProgressBarValue::determinate(value, total))
    };
    if let Some(label) = string_prop(props, "label") {
        progress.label(label).render(area, frame);
    } else {
        progress.render(area, frame);
    }
}

fn render_text_input_with_runtime(
    props: &ComponentValue,
    runtime: &mut ProtocolRuntime,
    node: &ComponentNode,
    area: Rect,
    frame: &mut Frame<'_>,
) {
    let type_id =
        node_type_id(node).unwrap_or_else(|| ComponentTypeId::new(TEXT_INPUT_BOX_TYPE_ID));
    let value = node_value_string(runtime.state(), node)
        .or_else(|| string_prop(props, "value"))
        .unwrap_or_default()
        .to_owned();
    let state_key = local_state_key(node, type_id);
    let focused = is_focused(runtime.state(), node);
    let input_state = runtime.local_state_or_insert_with(&state_key, || {
        TextInputState::new(TextEditBuffer::from_text(&value))
    });
    text_input_box(props, focused).render(area, input_state, frame);
}

fn render_text_input(
    props: &ComponentValue,
    state: &ComponentRuntimeState,
    node: &ComponentNode,
    area: Rect,
    frame: &mut Frame<'_>,
) {
    let mut runtime = ProtocolRuntime::from_state(state.clone());
    render_text_input_with_runtime(props, &mut runtime, node, area, frame);
}
fn render_text_view(props: &ComponentValue, area: Rect, frame: &mut Frame<'_>) {
    let lines = lines_prop(props, "lines")
        .or_else(|| string_prop(props, "text").map(|text| text.lines().map(Line::from).collect()))
        .unwrap_or_default();
    let mut state = TextViewState::new();
    if let Some(scroll) = number_prop(props, "vertical_scroll") {
        state.set_vertical_scroll(usize::try_from(scroll).unwrap_or(usize::MAX));
    }
    TextView::new(&lines).render(area, &state, frame);
}
fn render_unsupported(type_id: &str, area: Rect, frame: &mut Frame<'_>) {
    frame.write_line(
        area,
        &Line::from(format!("unsupported protocol component: {type_id}")),
    );
}

fn should_handle_focused_event(
    state: &ComponentRuntimeState,
    node: &ComponentNode,
    event: &Event,
) -> bool {
    if !matches!(event, Event::Key(_)) {
        return true;
    }
    state
        .focus
        .focused
        .as_ref()
        .is_none_or(|focused| node.id.as_ref() == Some(focused))
}

fn handle_action_row_with_runtime(
    props: &ComponentValue,
    node: &ComponentNode,
    runtime: &mut ProtocolRuntime,
    area: Rect,
    event: &Event,
) -> Vec<ComponentEvent> {
    if !should_handle_focused_event(runtime.state(), node, event) {
        return Vec::new();
    }
    let actions = action_buttons(props);
    let state_key = local_state_key(node, ComponentTypeId::new(ACTION_ROW_TYPE_ID));
    let row_state = runtime.local_state_or_insert_with(&state_key, ActionRowState::new);
    if row_state.focused().is_none() && !actions.is_empty() {
        row_state.set_focused(Some(0));
    }
    match ActionRow::new(&actions).handle_event(area, row_state, event) {
        ActionRowOutcome::Activated { id, .. } => action_event(node, ActionId::new(id)),
        ActionRowOutcome::Ignored
        | ActionRowOutcome::Handled
        | ActionRowOutcome::Redraw
        | ActionRowOutcome::FocusRequested { .. }
        | ActionRowOutcome::FocusMoved { .. } => Vec::new(),
    }
}

fn handle_action_row(
    props: &ComponentValue,
    node: &ComponentNode,
    area: Rect,
    event: &Event,
) -> Vec<ComponentEvent> {
    let actions = action_buttons(props);
    let mut state = ActionRowState::new();
    state.set_focused(Some(0));
    match ActionRow::new(&actions).handle_event(area, &mut state, event) {
        ActionRowOutcome::Activated { id, .. } => action_event(node, ActionId::new(id)),
        ActionRowOutcome::Ignored
        | ActionRowOutcome::Handled
        | ActionRowOutcome::Redraw
        | ActionRowOutcome::FocusRequested { .. }
        | ActionRowOutcome::FocusMoved { .. } => Vec::new(),
    }
}

fn handle_button(
    props: &ComponentValue,
    node: &ComponentNode,
    event: &Event,
) -> Vec<ComponentEvent> {
    if is_activation(event) {
        action(props).map_or_else(Vec::new, |action| action_event(node, action))
    } else {
        Vec::new()
    }
}

fn handle_checkbox(
    props: &ComponentValue,
    node: &ComponentNode,
    state: &mut ComponentRuntimeState,
    area: Rect,
    event: &Event,
) -> Vec<ComponentEvent> {
    let label = string_prop(props, "label").unwrap_or("checkbox");
    let checked = node_value_bool(state, node)
        .unwrap_or_else(|| bool_prop(props, "checked").unwrap_or(false));
    let mut checkbox_state = CheckboxState::new(checked);
    checkbox_state.set_focused(true);
    checkbox_state.set_disabled(bool_prop(props, "disabled").unwrap_or(false));
    match Checkbox::new(label).handle_event(area, &mut checkbox_state, event) {
        CheckboxOutcome::Toggled(value) => {
            value_changed_event(node, state, ComponentValue::Bool(value))
        }
        CheckboxOutcome::Ignored | CheckboxOutcome::Redraw => Vec::new(),
    }
}

fn handle_radio_group(
    props: &ComponentValue,
    node: &ComponentNode,
    state: &mut ComponentRuntimeState,
    area: Rect,
    event: &Event,
) -> Vec<ComponentEvent> {
    let options = radio_options(props);
    let selected = selected_option_index(props, state, node, &options);
    let mut group_state = RadioGroupState::new(selected);
    group_state.set_focused(selected.or(Some(0)));
    group_state.set_disabled(bool_prop(props, "disabled").unwrap_or(false));
    match RadioGroup::new(&options).handle_event(area, &mut group_state, event) {
        RadioGroupOutcome::Selected(index) => options.get(index).map_or_else(Vec::new, |option| {
            value_changed_event(node, state, ComponentValue::String(option.id.clone()))
        }),
        RadioGroupOutcome::Ignored | RadioGroupOutcome::Redraw | RadioGroupOutcome::Focused(_) => {
            Vec::new()
        }
    }
}

fn handle_radio_group_with_runtime(
    props: &ComponentValue,
    node: &ComponentNode,
    runtime: &mut ProtocolRuntime,
    area: Rect,
    event: &Event,
) -> Vec<ComponentEvent> {
    if !should_handle_focused_event(runtime.state(), node, event) {
        return Vec::new();
    }
    let options = radio_options(props);
    let selected = selected_option_index(props, runtime.state(), node, &options);
    let state_key = local_state_key(node, ComponentTypeId::new(RADIO_GROUP_TYPE_ID));
    let group_state =
        runtime.local_state_or_insert_with(&state_key, || RadioGroupState::new(selected));
    if selected != group_state.selected() {
        group_state.set_selected(selected);
    }
    group_state.set_disabled(bool_prop(props, "disabled").unwrap_or(false));
    match RadioGroup::new(&options).handle_event(area, group_state, event) {
        RadioGroupOutcome::Selected(index) => options.get(index).map_or_else(Vec::new, |option| {
            value_changed_event(
                node,
                runtime.state_mut(),
                ComponentValue::String(option.id.clone()),
            )
        }),
        RadioGroupOutcome::Ignored | RadioGroupOutcome::Redraw | RadioGroupOutcome::Focused(_) => {
            Vec::new()
        }
    }
}

fn handle_text_input_with_runtime(
    props: &ComponentValue,
    node: &ComponentNode,
    runtime: &mut ProtocolRuntime,
    area: Rect,
    event: &Event,
) -> Vec<ComponentEvent> {
    if !should_handle_focused_event(runtime.state(), node, event) {
        return Vec::new();
    }
    let type_id =
        node_type_id(node).unwrap_or_else(|| ComponentTypeId::new(TEXT_INPUT_BOX_TYPE_ID));
    let value = node_value_string(runtime.state(), node)
        .or_else(|| string_prop(props, "value"))
        .unwrap_or_default()
        .to_owned();
    let state_key = local_state_key(node, type_id);
    let focused = is_focused(runtime.state(), node);
    let input_state = runtime.local_state_or_insert_with(&state_key, || {
        TextInputState::new(TextEditBuffer::from_text(&value))
    });
    let outcome = text_input_box(props, focused).handle_event(area, input_state, event);
    let text = input_state.buffer().text().to_owned();
    match outcome {
        TextInputBoxOutcome::Edited => {
            value_changed_event(node, runtime.state_mut(), ComponentValue::String(text))
        }
        TextInputBoxOutcome::Submitted => vec![ComponentEvent::new(
            node.id.clone(),
            ComponentEventKind::Submit,
        )],
        TextInputBoxOutcome::Ignored
        | TextInputBoxOutcome::Redraw
        | TextInputBoxOutcome::EdgeUp
        | TextInputBoxOutcome::EdgeDown => Vec::new(),
    }
}

fn handle_text_input(
    props: &ComponentValue,
    node: &ComponentNode,
    state: &mut ComponentRuntimeState,
    area: Rect,
    event: &Event,
) -> Vec<ComponentEvent> {
    let mut runtime = ProtocolRuntime::from_state(std::mem::take(state));
    let events = handle_text_input_with_runtime(props, node, &mut runtime, area, event);
    *state = runtime.into_state();
    events
}
fn handle_select_dropdown_with_runtime(
    props: &ComponentValue,
    node: &ComponentNode,
    runtime: &mut ProtocolRuntime,
    area: Rect,
    event: &Event,
) -> Vec<ComponentEvent> {
    if !should_handle_focused_event(runtime.state(), node, event) {
        return Vec::new();
    }
    let options = select_options(props);
    let selected = selected_select_index(props, runtime.state(), node, &options);
    let state_key = local_state_key(node, ComponentTypeId::new(SELECT_DROPDOWN_TYPE_ID));
    let select_state =
        runtime.local_state_or_insert_with(&state_key, || SelectDropdownState::new(selected));
    if selected != select_state.selected() {
        select_state.set_selected(selected);
    }
    select_state.set_disabled(bool_prop(props, "disabled").unwrap_or(false));
    let select = SelectDropdown::new(&options)
        .placeholder(string_prop(props, "placeholder").unwrap_or("Select..."));
    match select.handle_event(area, select_state, event) {
        SelectDropdownOutcome::Selected(index) => {
            options.get(index).map_or_else(Vec::new, |option| {
                value_changed_event(
                    node,
                    runtime.state_mut(),
                    ComponentValue::String(option.id.clone()),
                )
            })
        }
        SelectDropdownOutcome::Ignored
        | SelectDropdownOutcome::Redraw
        | SelectDropdownOutcome::Opened
        | SelectDropdownOutcome::Closed
        | SelectDropdownOutcome::Focused(_) => Vec::new(),
    }
}

fn handle_select_dropdown(
    props: &ComponentValue,
    node: &ComponentNode,
    state: &mut ComponentRuntimeState,
    area: Rect,
    event: &Event,
) -> Vec<ComponentEvent> {
    let options = select_options(props);
    let selected = selected_select_index(props, state, node, &options);
    let mut select_state = SelectDropdownState::new(selected);
    select_state.set_open(bool_prop(props, "open").unwrap_or(false));
    select_state.set_disabled(bool_prop(props, "disabled").unwrap_or(false));
    let select = SelectDropdown::new(&options)
        .placeholder(string_prop(props, "placeholder").unwrap_or("Select..."));
    match select.handle_event(area, &mut select_state, event) {
        SelectDropdownOutcome::Selected(index) => {
            options.get(index).map_or_else(Vec::new, |option| {
                value_changed_event(node, state, ComponentValue::String(option.id.clone()))
            })
        }
        SelectDropdownOutcome::Ignored
        | SelectDropdownOutcome::Redraw
        | SelectDropdownOutcome::Opened
        | SelectDropdownOutcome::Closed
        | SelectDropdownOutcome::Focused(_) => Vec::new(),
    }
}
fn action_event(node: &ComponentNode, action: ActionId) -> Vec<ComponentEvent> {
    vec![ComponentEvent::new(
        node.id.clone(),
        ComponentEventKind::Action { action },
    )]
}

fn value_changed_event(
    node: &ComponentNode,
    state: &mut ComponentRuntimeState,
    value: ComponentValue,
) -> Vec<ComponentEvent> {
    if let Some(id) = &node.id {
        state.values.insert(id.clone(), value.clone());
    }
    vec![ComponentEvent::new(
        node.id.clone(),
        ComponentEventKind::ValueChanged { value },
    )]
}

fn action_buttons(props: &ComponentValue) -> Vec<ActionButton> {
    list_prop(props, "actions").map_or_else(Vec::new, |items| {
        items
            .iter()
            .enumerate()
            .map(|(index, item)| {
                let id = item_id(item).unwrap_or_else(|| index.to_string());
                let label = item_label(item).unwrap_or_else(|| id.clone());
                ActionButton::new(id, label)
            })
            .collect()
    })
}

fn radio_options(props: &ComponentValue) -> Vec<RadioOption> {
    list_prop(props, "options").map_or_else(Vec::new, |items| {
        items
            .iter()
            .enumerate()
            .map(|(index, item)| {
                let id = item_id(item).unwrap_or_else(|| index.to_string());
                let label = item_label(item).unwrap_or_else(|| id.clone());
                RadioOption::new(id, label).disabled(item_bool(item, "disabled").unwrap_or(false))
            })
            .collect()
    })
}

fn selected_option_index(
    props: &ComponentValue,
    state: &ComponentRuntimeState,
    node: &ComponentNode,
    options: &[RadioOption],
) -> Option<usize> {
    let selected = node_value_string(state, node).or_else(|| string_prop(props, "selected"));
    selected.and_then(|selected| options.iter().position(|option| option.id == selected))
}

fn local_state_key(node: &ComponentNode, type_id: ComponentTypeId) -> ProtocolLocalStateKey {
    let component_id = node
        .id
        .clone()
        .unwrap_or_else(|| bmux_tui_component_protocol::ids::ComponentId::new(type_id.as_str()));
    ProtocolLocalStateKey::new(component_id, type_id)
}

fn node_type_id(node: &ComponentNode) -> Option<ComponentTypeId> {
    match &node.kind {
        bmux_tui_component_protocol::model::ComponentKind::Component { type_id, .. } => {
            Some(type_id.clone())
        }
        bmux_tui_component_protocol::model::ComponentKind::Extension { kind, .. } => {
            Some(ComponentTypeId::new(kind.clone()))
        }
        _ => None,
    }
}

fn text_input_box(props: &ComponentValue, focused: bool) -> TextInputBox<'_> {
    let disabled = bool_prop(props, "disabled").unwrap_or(false);
    let mut input = TextInputBox::new(TextInputPolicy::chat_composer()).policy(
        TextInputBoxPolicy::field()
            .focused(focused)
            .disabled(disabled),
    );
    if let Some(label) = string_prop(props, "label") {
        input = input.label(label);
    }
    if let Some(placeholder) = string_prop(props, "placeholder") {
        input = input.placeholder(placeholder);
    }
    if let Some(help) = string_prop(props, "help") {
        input = input.help(help);
    }
    if let Some(error) = string_prop(props, "error") {
        input = input.error(error);
    }
    input.required(bool_prop(props, "required").unwrap_or(false))
}

fn select_options(props: &ComponentValue) -> Vec<SelectOption> {
    list_prop(props, "options").map_or_else(Vec::new, |items| {
        items
            .iter()
            .enumerate()
            .map(|(index, item)| {
                let id = item_id(item).unwrap_or_else(|| index.to_string());
                let label = item_label(item).unwrap_or_else(|| id.clone());
                SelectOption::new(id, label).disabled(item_bool(item, "disabled").unwrap_or(false))
            })
            .collect()
    })
}

fn selected_select_index(
    props: &ComponentValue,
    state: &ComponentRuntimeState,
    node: &ComponentNode,
    options: &[SelectOption],
) -> Option<usize> {
    let selected = node_value_string(state, node).or_else(|| string_prop(props, "selected"));
    selected.and_then(|selected| options.iter().position(|option| option.id == selected))
}

fn node_value_bool(state: &ComponentRuntimeState, node: &ComponentNode) -> Option<bool> {
    let id = node.id.as_ref()?;
    match state.values.get(id)? {
        ComponentValue::Bool(value) => Some(*value),
        ComponentValue::Null
        | ComponentValue::I64(_)
        | ComponentValue::U64(_)
        | ComponentValue::F64(_)
        | ComponentValue::String(_)
        | ComponentValue::List(_)
        | ComponentValue::Map(_) => None,
    }
}

fn node_value_string<'a>(
    state: &'a ComponentRuntimeState,
    node: &ComponentNode,
) -> Option<&'a str> {
    let id = node.id.as_ref()?;
    state.values.get(id)?.as_str()
}

fn is_focused(state: &ComponentRuntimeState, node: &ComponentNode) -> bool {
    node.id
        .as_ref()
        .is_some_and(|id| state.focus.focused.as_ref() == Some(id))
}

fn string_prop<'a>(value: &'a ComponentValue, key: &str) -> Option<&'a str> {
    value.as_map()?.get(key)?.as_str()
}

fn bool_prop(value: &ComponentValue, key: &str) -> Option<bool> {
    match value.as_map()?.get(key)? {
        ComponentValue::Bool(value) => Some(*value),
        ComponentValue::Null
        | ComponentValue::I64(_)
        | ComponentValue::U64(_)
        | ComponentValue::F64(_)
        | ComponentValue::String(_)
        | ComponentValue::List(_)
        | ComponentValue::Map(_) => None,
    }
}

fn number_prop(value: &ComponentValue, key: &str) -> Option<u64> {
    match value.as_map()?.get(key)? {
        ComponentValue::U64(value) => Some(*value),
        ComponentValue::I64(value) => u64::try_from(*value).ok(),
        ComponentValue::Null
        | ComponentValue::Bool(_)
        | ComponentValue::F64(_)
        | ComponentValue::String(_)
        | ComponentValue::List(_)
        | ComponentValue::Map(_) => None,
    }
}

fn lines_prop(value: &ComponentValue, key: &str) -> Option<Vec<Line>> {
    list_prop(value, key).map(|items| {
        items
            .iter()
            .filter_map(|item| item.as_str().map(Line::from))
            .collect()
    })
}

fn list_prop<'a>(value: &'a ComponentValue, key: &str) -> Option<&'a [ComponentValue]> {
    match value.as_map()?.get(key)? {
        ComponentValue::List(items) => Some(items),
        ComponentValue::Null
        | ComponentValue::Bool(_)
        | ComponentValue::I64(_)
        | ComponentValue::U64(_)
        | ComponentValue::F64(_)
        | ComponentValue::String(_)
        | ComponentValue::Map(_) => None,
    }
}

fn item_label(value: &ComponentValue) -> Option<String> {
    match value {
        ComponentValue::String(value) => Some(value.clone()),
        ComponentValue::Map(map) => map
            .get("label")
            .and_then(ComponentValue::as_str)
            .or_else(|| map.get("title").and_then(ComponentValue::as_str))
            .or_else(|| map.get("id").and_then(ComponentValue::as_str))
            .map(str::to_owned),
        ComponentValue::Null
        | ComponentValue::Bool(_)
        | ComponentValue::I64(_)
        | ComponentValue::U64(_)
        | ComponentValue::F64(_)
        | ComponentValue::List(_) => None,
    }
}

fn item_id(value: &ComponentValue) -> Option<String> {
    match value {
        ComponentValue::String(value) => Some(value.clone()),
        ComponentValue::Map(map) => map
            .get("id")
            .and_then(ComponentValue::as_str)
            .or_else(|| map.get("action").and_then(ComponentValue::as_str))
            .map(str::to_owned),
        ComponentValue::Null
        | ComponentValue::Bool(_)
        | ComponentValue::I64(_)
        | ComponentValue::U64(_)
        | ComponentValue::F64(_)
        | ComponentValue::List(_) => None,
    }
}

fn item_bool(value: &ComponentValue, key: &str) -> Option<bool> {
    match value {
        ComponentValue::Map(map) => match map.get(key)? {
            ComponentValue::Bool(value) => Some(*value),
            ComponentValue::Null
            | ComponentValue::I64(_)
            | ComponentValue::U64(_)
            | ComponentValue::F64(_)
            | ComponentValue::String(_)
            | ComponentValue::List(_)
            | ComponentValue::Map(_) => None,
        },
        ComponentValue::Null
        | ComponentValue::Bool(_)
        | ComponentValue::I64(_)
        | ComponentValue::U64(_)
        | ComponentValue::F64(_)
        | ComponentValue::String(_)
        | ComponentValue::List(_) => None,
    }
}

fn action(value: &ComponentValue) -> Option<ActionId> {
    string_prop(value, "action").map(ActionId::new)
}

const fn is_activation(event: &Event) -> bool {
    matches!(event, Event::Key(stroke) if stroke.modifiers.is_empty() && matches!(stroke.key, KeyCode::Enter | KeyCode::Space | KeyCode::Char(' ')))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use bmux_keyboard::{KeyCode, KeyStroke};
    use bmux_tui::event::Event;
    use bmux_tui::geometry::Rect;
    use bmux_tui_component_protocol::event::ComponentEventKind;
    use bmux_tui_component_protocol::model::ComponentNode;
    use bmux_tui_component_protocol::state::ComponentRuntimeState;
    use bmux_tui_component_protocol::value::ComponentValue;

    use super::{BUTTON_TYPE_ID, CHECKBOX_TYPE_ID, RADIO_GROUP_TYPE_ID, bmux_component_bindings};

    #[test]
    fn checkbox_binding_toggles_protocol_value() {
        let bindings = bmux_component_bindings();
        let binding = bindings
            .component(&CHECKBOX_TYPE_ID.into())
            .expect("binding");
        let node = ComponentNode::component(CHECKBOX_TYPE_ID, props(&[("label", "Accept")]))
            .with_id("accept");
        let mut state = ComponentRuntimeState::new();

        let events = binding.handle_event(
            &node,
            &mut state,
            Rect::new(0, 0, 20, 1),
            &Event::Key(KeyStroke::simple(KeyCode::Space)),
        );

        assert_eq!(
            state.values.get(&"accept".into()),
            Some(&ComponentValue::Bool(true))
        );
        assert!(matches!(
            events.first().map(|event| &event.kind),
            Some(ComponentEventKind::ValueChanged {
                value: ComponentValue::Bool(true)
            })
        ));
    }

    #[test]
    fn radio_binding_selects_focused_option() {
        let bindings = bmux_component_bindings();
        let binding = bindings
            .component(&RADIO_GROUP_TYPE_ID.into())
            .expect("binding");
        let mut props = BTreeMap::new();
        props.insert(
            "options".to_owned(),
            ComponentValue::List(vec![option("a", "A"), option("b", "B")]),
        );
        let node = ComponentNode::component(RADIO_GROUP_TYPE_ID, ComponentValue::Map(props))
            .with_id("choice");
        let mut state = ComponentRuntimeState::new();

        let events = binding.handle_event(
            &node,
            &mut state,
            Rect::new(0, 0, 20, 2),
            &Event::Key(KeyStroke::simple(KeyCode::Space)),
        );

        assert_eq!(
            state.values.get(&"choice".into()),
            Some(&ComponentValue::String("a".to_owned()))
        );
        assert!(
            matches!(events.first().map(|event| &event.kind), Some(ComponentEventKind::ValueChanged { value: ComponentValue::String(value) }) if value == "a")
        );
    }

    #[test]
    fn button_binding_emits_action() {
        let bindings = bmux_component_bindings();
        let binding = bindings.component(&BUTTON_TYPE_ID.into()).expect("binding");
        let node = ComponentNode::component(
            BUTTON_TYPE_ID,
            props(&[("label", "Submit"), ("action", "submit")]),
        )
        .with_id("submit_button");
        let mut state = ComponentRuntimeState::new();

        let events = binding.handle_event(
            &node,
            &mut state,
            Rect::new(0, 0, 20, 1),
            &Event::Key(KeyStroke::simple(KeyCode::Enter)),
        );

        assert!(
            matches!(events.first().map(|event| &event.kind), Some(ComponentEventKind::Action { action }) if action.as_str() == "submit")
        );
    }

    fn props(items: &[(&str, &str)]) -> ComponentValue {
        ComponentValue::Map(
            items
                .iter()
                .map(|(key, value)| {
                    (
                        (*key).to_owned(),
                        ComponentValue::String((*value).to_owned()),
                    )
                })
                .collect(),
        )
    }

    fn option(id: &str, label: &str) -> ComponentValue {
        props(&[("id", id), ("label", label)])
    }
}
