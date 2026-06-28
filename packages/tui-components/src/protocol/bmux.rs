//! Built-in open protocol bindings for components exported by this crate.

use bmux_keyboard::KeyCode;
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
use crate::protocol::{ProtocolBindings, ProtocolComponentBinding};
use crate::radio_group::{RadioGroup, RadioGroupOutcome, RadioGroupState, RadioOption};

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

/// Register built-in open bindings for every component module exported by this crate.
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
    fn render(
        &self,
        node: &ComponentNode,
        state: &ComponentRuntimeState,
        area: Rect,
        frame: &mut Frame<'_>,
    ) {
        let props = component_props(node);
        match self.type_id.as_str() {
            BUTTON_TYPE_ID => render_button(props, area, frame),
            ACTION_ROW_TYPE_ID => render_action_row(props, area, frame),
            BADGE_TYPE_ID => render_badge(props, area, frame),
            BAR_CHART_TYPE_ID => render_chart_like(props, area, frame, "bars"),
            BREADCRUMBS_TYPE_ID => render_joined_list(props, area, frame, "items", " / "),
            CHECKBOX_TYPE_ID => render_checkbox(props, state, node, area, frame),
            DIALOG_TYPE_ID | MODAL_FRAME_TYPE_ID | PANE_TYPE_ID | PICKER_FRAME_TYPE_ID => {
                render_titled_container(props, node, area, frame);
            }
            EMPTY_STATE_TYPE_ID => render_title_message(props, area, frame),
            FILTERED_LIST_TYPE_ID | MENU_TYPE_ID | SELECTABLE_LIST_TYPE_ID => {
                render_vertical_items(props, area, frame, "items");
            }
            FORM_TYPE_ID | FORM_FIELD_TYPE_ID | PANEL_GROUP_TYPE_ID | SCROLL_AREA_TYPE_ID => {
                render_children_or_title(props, node, area, frame);
            }
            KEY_HINT_BAR_TYPE_ID | STATUS_BAR_TYPE_ID | TAB_BAR_TYPE_ID => {
                render_joined_list(props, area, frame, "items", "  ");
            }
            LABELED_DETAILS_TYPE_ID | TABLE_TYPE_ID => render_table_like(props, area, frame),
            PROGRESS_BAR_TYPE_ID => render_progress(props, area, frame),
            RADIO_GROUP_TYPE_ID | SELECT_DROPDOWN_TYPE_ID => {
                render_radio_group(props, state, node, area, frame);
            }
            SCROLLBAR_TYPE_ID | SCROLLBAR_LAYOUT_TYPE_ID => {
                render_scalar(props, area, frame, "scroll");
            }
            STEPPER_TYPE_ID => render_joined_list(props, area, frame, "steps", " > "),
            TEXT_INPUT_TYPE_ID | TEXT_INPUT_BOX_TYPE_ID => render_input(props, area, frame),
            TEXT_VIEW_TYPE_ID => render_text(props, area, frame),
            TOAST_STACK_TYPE_ID => render_vertical_items(props, area, frame, "toasts"),
            TREE_VIEW_TYPE_ID => render_vertical_items(props, area, frame, "nodes"),
            CANVAS_TYPE_ID | CHART_TYPE_ID | SPARKLINE_TYPE_ID => {
                render_chart_like(props, area, frame, "points");
            }
            _ => render_children_or_title(props, node, area, frame),
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
            CHECKBOX_TYPE_ID => handle_checkbox(props, node, state, area, event),
            RADIO_GROUP_TYPE_ID | SELECT_DROPDOWN_TYPE_ID => {
                handle_radio_group(props, node, state, area, event)
            }
            _ => {
                if !is_activation(event) {
                    return Vec::new();
                }
                action(props).map_or_else(Vec::new, |action| {
                    vec![ComponentEvent::new(
                        node.id.clone(),
                        ComponentEventKind::Action { action },
                    )]
                })
            }
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

fn render_button(props: &ComponentValue, area: Rect, frame: &mut Frame<'_>) {
    let label = string_prop(props, "label").unwrap_or("button");
    let button = Button::new(label);
    let state = ButtonState::new();
    button.render(area, &state, frame);
}

fn render_action_row(props: &ComponentValue, area: Rect, frame: &mut Frame<'_>) {
    let actions = action_buttons(props);
    let mut state = ActionRowState::new();
    state.set_focused(Some(0));
    ActionRow::new(&actions).render_state(area, &state, frame);
}
fn render_badge(props: &ComponentValue, area: Rect, frame: &mut Frame<'_>) {
    let label = string_prop(props, "label").unwrap_or("badge");
    frame.write_line(area, &Line::from(format!("[ {label} ]")));
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

fn render_input(props: &ComponentValue, area: Rect, frame: &mut Frame<'_>) {
    let value = string_prop(props, "value")
        .or_else(|| string_prop(props, "placeholder"))
        .unwrap_or_default();
    frame.write_line(area, &Line::from(format!(" {value}")));
}

fn render_text(props: &ComponentValue, area: Rect, frame: &mut Frame<'_>) {
    frame.write_line(
        area,
        &Line::from(string_prop(props, "text").unwrap_or_default()),
    );
}

fn render_title_message(props: &ComponentValue, area: Rect, frame: &mut Frame<'_>) {
    let title = string_prop(props, "title").unwrap_or_default();
    let message = string_prop(props, "message").unwrap_or_default();
    frame.write_line(area, &Line::from(format!("{title} {message}")));
}

fn render_titled_container(
    props: &ComponentValue,
    node: &ComponentNode,
    area: Rect,
    frame: &mut Frame<'_>,
) {
    frame.write_line(
        area,
        &Line::from(string_prop(props, "title").unwrap_or_default()),
    );
    render_child_labels(
        node,
        Rect::new(
            area.x,
            area.y.saturating_add(1),
            area.width,
            area.height.saturating_sub(1),
        ),
        frame,
    );
}

fn render_children_or_title(
    props: &ComponentValue,
    node: &ComponentNode,
    area: Rect,
    frame: &mut Frame<'_>,
) {
    if node.children.is_empty() {
        frame.write_line(
            area,
            &Line::from(string_prop(props, "title").unwrap_or_default()),
        );
    } else {
        render_child_labels(node, area, frame);
    }
}

fn render_child_labels(node: &ComponentNode, area: Rect, frame: &mut Frame<'_>) {
    for (index, child) in node
        .children
        .iter()
        .take(usize::from(area.height))
        .enumerate()
    {
        let row = area
            .y
            .saturating_add(u16::try_from(index).unwrap_or(u16::MAX));
        frame.write_line(
            Rect::new(area.x, row, area.width, 1),
            &Line::from(child.id.as_ref().map_or("component", |id| id.as_str())),
        );
    }
}

fn render_joined_list(
    props: &ComponentValue,
    area: Rect,
    frame: &mut Frame<'_>,
    key: &str,
    separator: &str,
) {
    let text = list_prop(props, key)
        .map(|items| {
            items
                .iter()
                .filter_map(item_label)
                .collect::<Vec<_>>()
                .join(separator)
        })
        .unwrap_or_default();
    frame.write_line(area, &Line::from(text));
}

fn render_vertical_items(props: &ComponentValue, area: Rect, frame: &mut Frame<'_>, key: &str) {
    if let Some(items) = list_prop(props, key) {
        for (index, item) in items.iter().take(usize::from(area.height)).enumerate() {
            let row = area
                .y
                .saturating_add(u16::try_from(index).unwrap_or(u16::MAX));
            frame.write_line(
                Rect::new(area.x, row, area.width, 1),
                &Line::from(item_label(item).unwrap_or_default()),
            );
        }
    }
}

fn render_table_like(props: &ComponentValue, area: Rect, frame: &mut Frame<'_>) {
    render_vertical_items(props, area, frame, "rows");
}

fn render_chart_like(props: &ComponentValue, area: Rect, frame: &mut Frame<'_>, key: &str) {
    let count = list_prop(props, key).map_or(0, <[_]>::len);
    let label = string_prop(props, "label").unwrap_or("chart");
    frame.write_line(area, &Line::from(format!("{label}: {count} items")));
}

fn render_scalar(props: &ComponentValue, area: Rect, frame: &mut Frame<'_>, label: &str) {
    let value = number_prop(props, "value").unwrap_or_default();
    frame.write_line(area, &Line::from(format!("{label}: {value}")));
}

fn render_progress(props: &ComponentValue, area: Rect, frame: &mut Frame<'_>) {
    let value = number_prop(props, "value").unwrap_or_default();
    let max = number_prop(props, "max").unwrap_or(100);
    frame.write_line(area, &Line::from(format!("[{value}/{max}]")));
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
        ActionRowOutcome::Activated { id, .. } => vec![ComponentEvent::new(
            node.id.clone(),
            ComponentEventKind::Action {
                action: ActionId::new(id),
            },
        )],
        ActionRowOutcome::Ignored
        | ActionRowOutcome::Handled
        | ActionRowOutcome::Redraw
        | ActionRowOutcome::FocusRequested { .. }
        | ActionRowOutcome::FocusMoved { .. } => Vec::new(),
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
            set_node_value(node, state, ComponentValue::Bool(value));
            vec![ComponentEvent::new(
                node.id.clone(),
                ComponentEventKind::ValueChanged {
                    value: ComponentValue::Bool(value),
                },
            )]
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
            let value = ComponentValue::String(option.id.clone());
            set_node_value(node, state, value.clone());
            vec![ComponentEvent::new(
                node.id.clone(),
                ComponentEventKind::ValueChanged { value },
            )]
        }),
        RadioGroupOutcome::Ignored | RadioGroupOutcome::Redraw | RadioGroupOutcome::Focused(_) => {
            Vec::new()
        }
    }
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

fn node_value_bool(state: &ComponentRuntimeState, node: &ComponentNode) -> Option<bool> {
    let id = node.id.as_ref()?;
    match state.values.get(id)? {
        ComponentValue::Bool(value) => Some(*value),
        _ => None,
    }
}

fn node_value_string<'a>(
    state: &'a ComponentRuntimeState,
    node: &ComponentNode,
) -> Option<&'a str> {
    let id = node.id.as_ref()?;
    state.values.get(id)?.as_str()
}

fn set_node_value(node: &ComponentNode, state: &mut ComponentRuntimeState, value: ComponentValue) {
    if let Some(id) = &node.id {
        state.values.insert(id.clone(), value);
    }
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
        _ => None,
    }
}

fn number_prop(value: &ComponentValue, key: &str) -> Option<u64> {
    match value.as_map()?.get(key)? {
        ComponentValue::U64(value) => Some(*value),
        ComponentValue::I64(value) => u64::try_from(*value).ok(),
        _ => None,
    }
}

fn list_prop<'a>(value: &'a ComponentValue, key: &str) -> Option<&'a [ComponentValue]> {
    match value.as_map()?.get(key)? {
        ComponentValue::List(items) => Some(items),
        _ => None,
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
        _ => None,
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
        _ => None,
    }
}

fn item_bool(value: &ComponentValue, key: &str) -> Option<bool> {
    match value {
        ComponentValue::Map(map) => match map.get(key)? {
            ComponentValue::Bool(value) => Some(*value),
            _ => None,
        },
        _ => None,
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
