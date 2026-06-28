//! Component props helpers owned by `bmux_tui_components`.

use std::collections::BTreeMap;

use bmux_tui_component_protocol::ids::{ActionId, ComponentId};
use bmux_tui_component_protocol::model::{
    CheckboxOption, ComponentKind, ComponentNode, ComponentTree, OptionItem, StackDirection,
};
use bmux_tui_component_protocol::value::ComponentValue;

use crate::protocol::{
    BUTTON_TYPE_ID, CHECKBOX_TYPE_ID, FORM_FIELD_TYPE_ID, FORM_TYPE_ID,
    ProtocolComponentDefinition, ProtocolComponentError, RADIO_GROUP_TYPE_ID,
    SELECT_DROPDOWN_TYPE_ID, TEXT_INPUT_BOX_TYPE_ID, TEXT_INPUT_TYPE_ID,
};

/// Protocol props for [`crate::button::Button`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ButtonProps {
    /// Visible button label.
    pub label: String,
    /// Action emitted when the button is activated.
    pub action: ActionId,
    /// Whether the button is disabled.
    pub disabled: bool,
}

impl ButtonProps {
    /// Create button props.
    #[must_use]
    pub fn new(label: impl Into<String>, action: impl Into<ActionId>) -> Self {
        Self {
            label: label.into(),
            action: action.into(),
            disabled: false,
        }
    }

    /// Return props with disabled state set.
    #[must_use]
    pub const fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }

    /// Build a protocol node for this button.
    #[must_use]
    pub fn into_node(self, id: impl Into<ComponentId>) -> ComponentNode {
        ComponentNode::component(BUTTON_TYPE_ID, ButtonDefinition::props_to_value(self)).with_id(id)
    }
}

/// Protocol definition marker for [`crate::button::Button`].
pub struct ButtonDefinition;

impl ProtocolComponentDefinition for ButtonDefinition {
    type Props = ButtonProps;

    const TYPE_ID: &'static str = BUTTON_TYPE_ID;

    fn props_from_value(value: &ComponentValue) -> Result<Self::Props, ProtocolComponentError> {
        let map = expect_map(value, BUTTON_TYPE_ID)?;
        let label = required_string(map, BUTTON_TYPE_ID, "label")?;
        let action = required_string(map, BUTTON_TYPE_ID, "action")?;
        Ok(ButtonProps {
            label: label.to_owned(),
            action: ActionId::new(action),
            disabled: bool_value(map, "disabled").unwrap_or(false),
        })
    }

    fn props_to_value(props: Self::Props) -> ComponentValue {
        let mut map = BTreeMap::new();
        map.insert("label".to_owned(), ComponentValue::String(props.label));
        map.insert("action".to_owned(), ComponentValue::String(props.action.0));
        map.insert("disabled".to_owned(), ComponentValue::Bool(props.disabled));
        ComponentValue::Map(map)
    }
}

/// One protocol choice option.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChoiceOptionProps {
    /// Stable option id.
    pub id: String,
    /// Visible option label.
    pub label: String,
    /// Whether the option is disabled.
    pub disabled: bool,
}

impl ChoiceOptionProps {
    /// Create an enabled choice option.
    #[must_use]
    pub fn new(id: impl Into<String>, label: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            disabled: false,
        }
    }

    /// Return this option with disabled state set.
    #[must_use]
    pub const fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }
}

/// Text-input protocol props.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TextInputProps {
    /// Initial value.
    pub value: String,
    /// Placeholder shown for empty input.
    pub placeholder: Option<String>,
    /// Optional label for boxed inputs.
    pub label: Option<String>,
    /// Optional help text.
    pub help: Option<String>,
    /// Optional validation error.
    pub error: Option<String>,
    /// Required marker.
    pub required: bool,
    /// Disabled state.
    pub disabled: bool,
}

impl TextInputProps {
    /// Create empty text-input props.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Set placeholder text.
    #[must_use]
    pub fn placeholder(mut self, placeholder: impl Into<String>) -> Self {
        self.placeholder = Some(placeholder.into());
        self
    }

    /// Set label text.
    #[must_use]
    pub fn label(mut self, label: impl Into<String>) -> Self {
        self.label = Some(label.into());
        self
    }

    /// Build a `bmux.text_input_box` node.
    #[must_use]
    pub fn into_box_node(self, id: impl Into<ComponentId>) -> ComponentNode {
        ComponentNode::component(TEXT_INPUT_BOX_TYPE_ID, text_input_value(self)).with_id(id)
    }

    /// Build a `bmux.text_input` node.
    #[must_use]
    pub fn into_node(self, id: impl Into<ComponentId>) -> ComponentNode {
        ComponentNode::component(TEXT_INPUT_TYPE_ID, text_input_value(self)).with_id(id)
    }
}

/// Select/dropdown protocol props.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectDropdownProps {
    /// Select options.
    pub options: Vec<ChoiceOptionProps>,
    /// Selected option id.
    pub selected: Option<String>,
    /// Placeholder shown when no option is selected.
    pub placeholder: Option<String>,
    /// Disabled state.
    pub disabled: bool,
}

impl SelectDropdownProps {
    /// Create select props over options.
    #[must_use]
    pub const fn new(options: Vec<ChoiceOptionProps>) -> Self {
        Self {
            options,
            selected: None,
            placeholder: None,
            disabled: false,
        }
    }

    /// Build a protocol node.
    #[must_use]
    pub fn into_node(self, id: impl Into<ComponentId>) -> ComponentNode {
        ComponentNode::component(
            SELECT_DROPDOWN_TYPE_ID,
            options_value(self.options, self.selected, self.placeholder, self.disabled),
        )
        .with_id(id)
    }
}

/// Radio-group protocol props.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RadioGroupProps(pub SelectDropdownProps);

impl RadioGroupProps {
    /// Create radio-group props over options.
    #[must_use]
    pub const fn new(options: Vec<ChoiceOptionProps>) -> Self {
        Self(SelectDropdownProps::new(options))
    }

    /// Build a protocol node.
    #[must_use]
    pub fn into_node(self, id: impl Into<ComponentId>) -> ComponentNode {
        let props = self.0;
        ComponentNode::component(
            RADIO_GROUP_TYPE_ID,
            options_value(
                props.options,
                props.selected,
                props.placeholder,
                props.disabled,
            ),
        )
        .with_id(id)
    }
}

/// Protocol props for an intrinsic checkbox group.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckboxGroupProps {
    /// Available options.
    pub options: Vec<ChoiceOptionProps>,
    /// Selected option ids.
    pub selected: Vec<String>,
    /// Required marker.
    pub required: bool,
    /// Disabled state.
    pub disabled: bool,
}

impl CheckboxGroupProps {
    /// Create checkbox-group props.
    #[must_use]
    pub const fn new(options: Vec<ChoiceOptionProps>) -> Self {
        Self {
            options,
            selected: Vec::new(),
            required: false,
            disabled: false,
        }
    }

    /// Set selected option ids.
    #[must_use]
    pub fn selected(mut self, selected: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.selected = selected.into_iter().map(Into::into).collect();
        self
    }

    /// Build a protocol node.
    #[must_use]
    pub fn into_node(self, id: impl Into<ComponentId>) -> ComponentNode {
        let selected = self
            .selected
            .into_iter()
            .collect::<std::collections::BTreeSet<_>>();
        let options = self
            .options
            .into_iter()
            .map(|option| {
                let checked = selected.contains(&option.id);
                let mut checkbox_option =
                    CheckboxOption::new(OptionItem::new(option.id, option.label));
                checkbox_option.checked = checked;
                checkbox_option
            })
            .collect();
        ComponentNode::leaf(ComponentKind::CheckboxGroup {
            options,
            required: self.required,
            disabled: self.disabled,
        })
        .with_id(id)
    }
}

/// Checkbox protocol props.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckboxProps {
    /// Visible label.
    pub label: String,
    /// Initial checked state.
    pub checked: bool,
    /// Disabled state.
    pub disabled: bool,
}

impl CheckboxProps {
    /// Create checkbox props.
    #[must_use]
    pub fn new(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            checked: false,
            disabled: false,
        }
    }

    /// Build a protocol node.
    #[must_use]
    pub fn into_node(self, id: impl Into<ComponentId>) -> ComponentNode {
        let mut map = BTreeMap::new();
        map.insert("label".to_owned(), ComponentValue::String(self.label));
        map.insert("checked".to_owned(), ComponentValue::Bool(self.checked));
        map.insert("disabled".to_owned(), ComponentValue::Bool(self.disabled));
        ComponentNode::component(CHECKBOX_TYPE_ID, ComponentValue::Map(map)).with_id(id)
    }
}

/// Form-field protocol props.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FormFieldProps {
    /// Field label.
    pub label: String,
    /// Required marker.
    pub required: bool,
    /// Help text.
    pub help: Option<String>,
    /// Validation error.
    pub error: Option<String>,
}

impl FormFieldProps {
    /// Create form-field props.
    #[must_use]
    pub fn new(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            required: false,
            help: None,
            error: None,
        }
    }

    /// Wrap a child node in a form field.
    #[must_use]
    pub fn with_child(self, id: impl Into<ComponentId>, child: ComponentNode) -> ComponentNode {
        ComponentNode::component_container(FORM_FIELD_TYPE_ID, form_field_value(self), vec![child])
            .with_id(id)
    }
}

/// Generic vertical form builder for protocol component trees.
#[derive(Debug, Clone, PartialEq)]
pub struct FormBuilder {
    id: ComponentId,
    children: Vec<ComponentNode>,
    gap: u16,
}

impl FormBuilder {
    /// Create a generic form builder.
    #[must_use]
    pub fn new(id: impl Into<ComponentId>) -> Self {
        Self {
            id: id.into(),
            children: Vec::new(),
            gap: 1,
        }
    }

    /// Set vertical gap between children.
    #[must_use]
    pub const fn gap(mut self, gap: u16) -> Self {
        self.gap = gap;
        self
    }

    /// Add plain text.
    #[must_use]
    pub fn text(mut self, text: impl Into<String>) -> Self {
        self.children.push(ComponentNode::leaf(ComponentKind::Text {
            text: text.into(),
            align: None,
        }));
        self
    }

    /// Add an arbitrary child.
    #[must_use]
    pub fn child(mut self, child: ComponentNode) -> Self {
        self.children.push(child);
        self
    }

    /// Build the component tree.
    #[must_use]
    pub fn build(self) -> ComponentTree {
        ComponentTree::new(
            ComponentNode::component_container(
                FORM_TYPE_ID,
                ComponentValue::Map(BTreeMap::new()),
                vec![ComponentNode::container(
                    ComponentKind::Stack {
                        direction: StackDirection::Vertical,
                        gap: self.gap,
                    },
                    self.children,
                )],
            )
            .with_id(self.id),
        )
    }
}

fn text_input_value(props: TextInputProps) -> ComponentValue {
    let mut map = BTreeMap::new();
    map.insert("value".to_owned(), ComponentValue::String(props.value));
    insert_optional(&mut map, "placeholder", props.placeholder);
    insert_optional(&mut map, "label", props.label);
    insert_optional(&mut map, "help", props.help);
    insert_optional(&mut map, "error", props.error);
    map.insert("required".to_owned(), ComponentValue::Bool(props.required));
    map.insert("disabled".to_owned(), ComponentValue::Bool(props.disabled));
    ComponentValue::Map(map)
}

fn form_field_value(props: FormFieldProps) -> ComponentValue {
    let mut map = BTreeMap::new();
    map.insert("label".to_owned(), ComponentValue::String(props.label));
    map.insert("required".to_owned(), ComponentValue::Bool(props.required));
    insert_optional(&mut map, "help", props.help);
    insert_optional(&mut map, "error", props.error);
    ComponentValue::Map(map)
}

fn options_value(
    options: Vec<ChoiceOptionProps>,
    selected: Option<String>,
    placeholder: Option<String>,
    disabled: bool,
) -> ComponentValue {
    let mut map = BTreeMap::new();
    map.insert(
        "options".to_owned(),
        ComponentValue::List(options.into_iter().map(option_value).collect()),
    );
    insert_optional(&mut map, "selected", selected);
    insert_optional(&mut map, "placeholder", placeholder);
    map.insert("disabled".to_owned(), ComponentValue::Bool(disabled));
    ComponentValue::Map(map)
}

fn option_value(option: ChoiceOptionProps) -> ComponentValue {
    let mut map = BTreeMap::new();
    map.insert("id".to_owned(), ComponentValue::String(option.id));
    map.insert("label".to_owned(), ComponentValue::String(option.label));
    map.insert("disabled".to_owned(), ComponentValue::Bool(option.disabled));
    ComponentValue::Map(map)
}

fn insert_optional(map: &mut BTreeMap<String, ComponentValue>, key: &str, value: Option<String>) {
    if let Some(value) = value {
        map.insert(key.to_owned(), ComponentValue::String(value));
    }
}

fn expect_map<'a>(
    value: &'a ComponentValue,
    type_id: &str,
) -> Result<&'a BTreeMap<String, ComponentValue>, ProtocolComponentError> {
    value
        .as_map()
        .ok_or_else(|| ProtocolComponentError::InvalidProps {
            type_id: type_id.to_owned(),
            message: "expected props map".to_owned(),
        })
}

fn required_string<'a>(
    map: &'a BTreeMap<String, ComponentValue>,
    type_id: &str,
    key: &str,
) -> Result<&'a str, ProtocolComponentError> {
    map.get(key)
        .and_then(ComponentValue::as_str)
        .ok_or_else(|| ProtocolComponentError::InvalidProps {
            type_id: type_id.to_owned(),
            message: format!("missing string prop `{key}`"),
        })
}

fn bool_value(map: &BTreeMap<String, ComponentValue>, key: &str) -> Option<bool> {
    match map.get(key)? {
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
