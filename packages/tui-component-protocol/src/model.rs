//! Declarative component tree model.

use crate::ids::{ActionId, ComponentId};
use crate::value::ComponentValue;

/// Current protocol version for newly-created component trees.
pub const COMPONENT_PROTOCOL_VERSION: u16 = 1;

/// Root component tree sent from a producer to a BMUX TUI host.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub struct ComponentTree {
    /// Protocol version used by this tree.
    pub protocol_version: u16,
    /// Root component node.
    pub root: ComponentNode,
}

impl ComponentTree {
    /// Create a component tree with the current protocol version.
    #[must_use]
    pub const fn new(root: ComponentNode) -> Self {
        Self {
            protocol_version: COMPONENT_PROTOCOL_VERSION,
            root,
        }
    }
}

/// One declarative component node.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub struct ComponentNode {
    /// Stable component id used by state and events.
    pub id: Option<ComponentId>,
    /// Component behavior and data.
    pub kind: ComponentKind,
    /// Child nodes, when the component is a container.
    pub children: Vec<Self>,
}

impl ComponentNode {
    /// Create a leaf component node.
    #[must_use]
    pub const fn leaf(kind: ComponentKind) -> Self {
        Self {
            id: None,
            kind,
            children: Vec::new(),
        }
    }

    /// Create a container component node.
    #[must_use]
    pub const fn container(kind: ComponentKind, children: Vec<Self>) -> Self {
        Self {
            id: None,
            kind,
            children,
        }
    }

    /// Return this node with a stable component id.
    #[must_use]
    pub fn with_id(mut self, id: impl Into<ComponentId>) -> Self {
        self.id = Some(id.into());
        self
    }
}

/// Declarative component kind.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum ComponentKind {
    /// Plain text.
    Text {
        /// Text content.
        text: String,
        /// Optional alignment hint.
        align: Option<TextAlign>,
    },
    /// Markdown text for hosts that support markdown rendering.
    Markdown {
        /// Markdown content.
        markdown: String,
    },
    /// Vertical or horizontal stack container.
    Stack {
        /// Stack direction.
        direction: StackDirection,
        /// Optional cell gap between children.
        gap: u16,
    },
    /// Panel container.
    Panel {
        /// Optional title.
        title: Option<String>,
        /// Chrome style hint.
        chrome: PanelChrome,
    },
    /// Divider line.
    Divider,
    /// Empty space.
    Spacer {
        /// Desired size in terminal cells.
        size: u16,
    },
    /// Push button.
    Button {
        /// Visible label.
        label: String,
        /// Action emitted when activated.
        action: ActionId,
        /// Button role hint.
        role: ButtonRole,
        /// Disabled state.
        disabled: bool,
    },
    /// Single-line text input.
    TextInput {
        /// Placeholder text.
        placeholder: Option<String>,
        /// Current value.
        value: String,
        /// Input kind hint.
        input_kind: InputKind,
        /// Whether the input is required for form submission.
        required: bool,
        /// Disabled state.
        disabled: bool,
    },
    /// Multi-line text input.
    TextArea {
        /// Placeholder text.
        placeholder: Option<String>,
        /// Current value.
        value: String,
        /// Preferred visible row count.
        rows: u16,
        /// Whether the input is required for form submission.
        required: bool,
        /// Disabled state.
        disabled: bool,
    },
    /// Radio button group.
    RadioGroup {
        /// Available options.
        options: Vec<OptionItem>,
        /// Selected option id.
        selected: Option<String>,
        /// Whether a selection is required for form submission.
        required: bool,
        /// Disabled state.
        disabled: bool,
    },
    /// Checkbox group.
    CheckboxGroup {
        /// Available options.
        options: Vec<CheckboxOption>,
        /// Whether at least one selection is required for form submission.
        required: bool,
        /// Disabled state.
        disabled: bool,
    },
    /// Dropdown/select control.
    Select {
        /// Available options.
        options: Vec<OptionItem>,
        /// Selected option id.
        selected: Option<String>,
        /// Whether a selection is required for form submission.
        required: bool,
        /// Disabled state.
        disabled: bool,
    },
    /// Form container.
    Form {
        /// Action emitted on submit.
        submit: ActionId,
        /// Optional action emitted on cancel.
        cancel: Option<ActionId>,
    },
    /// Status message.
    Status {
        /// Severity level.
        level: StatusLevel,
        /// Message text.
        message: String,
    },
    /// Host-defined extension component.
    Extension {
        /// Extension kind identifier.
        kind: String,
        /// Extension payload.
        payload: ComponentValue,
    },
}

/// Horizontal text alignment hint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum TextAlign {
    /// Align to the left edge.
    Left,
    /// Center within the available area.
    Center,
    /// Align to the right edge.
    Right,
}

/// Stack layout direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum StackDirection {
    /// Children are arranged from top to bottom.
    Vertical,
    /// Children are arranged from left to right.
    Horizontal,
}

/// Panel chrome style hint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum PanelChrome {
    /// No visible chrome.
    None,
    /// Draw a border around content.
    Border,
}

/// Button intent hint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum ButtonRole {
    /// Normal action.
    Normal,
    /// Primary/default action.
    Primary,
    /// Destructive action.
    Danger,
}

/// Text input content hint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum InputKind {
    /// Plain text input.
    Text,
    /// Password/secret input.
    Password,
    /// Numeric input.
    Number,
}

/// Generic option item used by choice controls.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub struct OptionItem {
    /// Stable option id.
    pub id: String,
    /// Visible option label.
    pub label: String,
    /// Optional help text.
    pub description: Option<String>,
    /// Optional application-owned value.
    pub value: Option<ComponentValue>,
    /// Disabled state.
    pub disabled: bool,
}

impl OptionItem {
    /// Create an enabled option item.
    #[must_use]
    pub fn new(id: impl Into<String>, label: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            description: None,
            value: None,
            disabled: false,
        }
    }
}

/// Checkbox option with selection state.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub struct CheckboxOption {
    /// Shared option data.
    pub option: OptionItem,
    /// Current checked state.
    pub checked: bool,
}

impl CheckboxOption {
    /// Create an unchecked checkbox option.
    #[must_use]
    pub const fn new(option: OptionItem) -> Self {
        Self {
            option,
            checked: false,
        }
    }
}

/// Status severity hint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum StatusLevel {
    /// Informational status.
    Info,
    /// Successful status.
    Success,
    /// Warning status.
    Warning,
    /// Error status.
    Error,
}
