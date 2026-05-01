#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

use anyhow::{Result, bail};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuntimeAction {
    NoOp,
    Quit,
    Detach,
    ShowHelp,
    EnterScrollMode,
    ExitScrollMode,
    ScrollUpLine,
    ScrollDownLine,
    ScrollUpPage,
    ScrollDownPage,
    ScrollTop,
    ScrollBottom,
    BeginSelection,
    MoveCursorLeft,
    MoveCursorRight,
    MoveCursorUp,
    MoveCursorDown,
    CopyScrollback,
    ConfirmScrollback,
    ExitMode,
    EnterMode(String),
    SwitchProfile(String),
    PluginCommand {
        plugin_id: String,
        command_name: String,
        args: Vec<String>,
    },
    ForwardToPane(Vec<u8>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BindableActionArgument {
    pub name: &'static str,
    pub label: &'static str,
    pub placeholder: &'static str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BindableActionInfo {
    pub action: &'static str,
    pub label: &'static str,
    pub detail: &'static str,
    pub argument: Option<BindableActionArgument>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct StaticBindableAction {
    info: BindableActionInfo,
    target: ActionTarget,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ActionTarget {
    NoOp,
    Quit,
    Detach,
    ShowHelp,
    EnterScrollMode,
    ExitScrollMode,
    ScrollUpLine,
    ScrollDownLine,
    ScrollUpPage,
    ScrollDownPage,
    ScrollTop,
    ScrollBottom,
    BeginSelection,
    MoveCursorLeft,
    MoveCursorRight,
    MoveCursorUp,
    MoveCursorDown,
    CopyScrollback,
    ConfirmScrollback,
    ExitMode,
    Plugin {
        plugin_id: &'static str,
        command_name: &'static str,
        args: &'static [&'static str],
    },
}

impl ActionTarget {
    fn to_runtime_action(self) -> RuntimeAction {
        match self {
            Self::NoOp => RuntimeAction::NoOp,
            Self::Quit => RuntimeAction::Quit,
            Self::Detach => RuntimeAction::Detach,
            Self::ShowHelp => RuntimeAction::ShowHelp,
            Self::EnterScrollMode => RuntimeAction::EnterScrollMode,
            Self::ExitScrollMode => RuntimeAction::ExitScrollMode,
            Self::ScrollUpLine => RuntimeAction::ScrollUpLine,
            Self::ScrollDownLine => RuntimeAction::ScrollDownLine,
            Self::ScrollUpPage => RuntimeAction::ScrollUpPage,
            Self::ScrollDownPage => RuntimeAction::ScrollDownPage,
            Self::ScrollTop => RuntimeAction::ScrollTop,
            Self::ScrollBottom => RuntimeAction::ScrollBottom,
            Self::BeginSelection => RuntimeAction::BeginSelection,
            Self::MoveCursorLeft => RuntimeAction::MoveCursorLeft,
            Self::MoveCursorRight => RuntimeAction::MoveCursorRight,
            Self::MoveCursorUp => RuntimeAction::MoveCursorUp,
            Self::MoveCursorDown => RuntimeAction::MoveCursorDown,
            Self::CopyScrollback => RuntimeAction::CopyScrollback,
            Self::ConfirmScrollback => RuntimeAction::ConfirmScrollback,
            Self::ExitMode => RuntimeAction::ExitMode,
            Self::Plugin {
                plugin_id,
                command_name,
                args,
            } => plugin_command_vec(plugin_id, command_name, args),
        }
    }
}

#[must_use]
pub const fn action_to_name(action: &RuntimeAction) -> &'static str {
    match action {
        RuntimeAction::NoOp => "no_op",
        RuntimeAction::Quit => "quit",
        RuntimeAction::Detach => "detach",
        RuntimeAction::ShowHelp => "show_help",
        RuntimeAction::EnterScrollMode => "enter_scroll_mode",
        RuntimeAction::ExitScrollMode => "exit_scroll_mode",
        RuntimeAction::ScrollUpLine => "scroll_up_line",
        RuntimeAction::ScrollDownLine => "scroll_down_line",
        RuntimeAction::ScrollUpPage => "scroll_up_page",
        RuntimeAction::ScrollDownPage => "scroll_down_page",
        RuntimeAction::ScrollTop => "scroll_top",
        RuntimeAction::ScrollBottom => "scroll_bottom",
        RuntimeAction::BeginSelection => "begin_selection",
        RuntimeAction::MoveCursorLeft => "move_cursor_left",
        RuntimeAction::MoveCursorRight => "move_cursor_right",
        RuntimeAction::MoveCursorUp => "move_cursor_up",
        RuntimeAction::MoveCursorDown => "move_cursor_down",
        RuntimeAction::CopyScrollback => "copy_scrollback",
        RuntimeAction::ConfirmScrollback => "confirm_scrollback",
        RuntimeAction::ExitMode => "exit_mode",
        RuntimeAction::EnterMode(_) => "enter_mode",
        RuntimeAction::SwitchProfile(_) => "switch_profile",
        RuntimeAction::PluginCommand { .. } => "plugin_command",
        RuntimeAction::ForwardToPane(_) => "forward_to_pane",
    }
}

#[must_use]
pub fn action_to_config_name(action: &RuntimeAction) -> String {
    match action {
        RuntimeAction::EnterMode(mode_id) => format!("enter_mode {mode_id}"),
        RuntimeAction::SwitchProfile(profile_id) => format!("switch_profile {profile_id}"),
        RuntimeAction::PluginCommand {
            plugin_id,
            command_name,
            args,
        } => {
            if args.is_empty() {
                format!("plugin:{plugin_id}:{command_name}")
            } else {
                format!(
                    "plugin:{plugin_id}:{command_name} {}",
                    args.iter()
                        .map(|arg| shell_quote(arg))
                        .collect::<Vec<_>>()
                        .join(" ")
                )
            }
        }
        _ => action_to_name(action).to_string(),
    }
}

const fn static_action(
    action: &'static str,
    label: &'static str,
    detail: &'static str,
    target: ActionTarget,
) -> StaticBindableAction {
    StaticBindableAction {
        info: BindableActionInfo {
            action,
            label,
            detail,
            argument: None,
        },
        target,
    }
}

const STATIC_BINDABLE_ACTIONS: &[StaticBindableAction] = &[
    static_action("quit", "Quit", "keybind: quit", ActionTarget::Quit),
    static_action(
        "quit_destroy",
        "Quit Destroy",
        "keybind alias: quit_destroy",
        ActionTarget::Quit,
    ),
    static_action("detach", "Detach", "keybind: detach", ActionTarget::Detach),
    static_action("no_op", "No Op", "keybind: no_op", ActionTarget::NoOp),
    static_action(
        "toggle_split_direction",
        "Toggle Split Direction",
        "legacy keybind alias: toggle_split_direction",
        ActionTarget::NoOp,
    ),
    static_action(
        "enter_window_mode",
        "Enter Window Mode",
        "legacy keybind alias: enter_window_mode",
        ActionTarget::NoOp,
    ),
    static_action(
        "show_help",
        "Show Help",
        "keybind: show_help",
        ActionTarget::ShowHelp,
    ),
    static_action(
        "enter_scroll_mode",
        "Enter Scroll Mode",
        "keybind: enter_scroll_mode",
        ActionTarget::EnterScrollMode,
    ),
    static_action(
        "exit_scroll_mode",
        "Exit Scroll Mode",
        "keybind: exit_scroll_mode",
        ActionTarget::ExitScrollMode,
    ),
    static_action(
        "scroll_up_line",
        "Scroll Up Line",
        "keybind: scroll_up_line",
        ActionTarget::ScrollUpLine,
    ),
    static_action(
        "scroll_down_line",
        "Scroll Down Line",
        "keybind: scroll_down_line",
        ActionTarget::ScrollDownLine,
    ),
    static_action(
        "scroll_up_page",
        "Scroll Up Page",
        "keybind: scroll_up_page",
        ActionTarget::ScrollUpPage,
    ),
    static_action(
        "scroll_down_page",
        "Scroll Down Page",
        "keybind: scroll_down_page",
        ActionTarget::ScrollDownPage,
    ),
    static_action(
        "scroll_top",
        "Scroll Top",
        "keybind: scroll_top",
        ActionTarget::ScrollTop,
    ),
    static_action(
        "scroll_bottom",
        "Scroll Bottom",
        "keybind: scroll_bottom",
        ActionTarget::ScrollBottom,
    ),
    static_action(
        "begin_selection",
        "Begin Selection",
        "keybind: begin_selection",
        ActionTarget::BeginSelection,
    ),
    static_action(
        "move_cursor_left",
        "Move Cursor Left",
        "keybind: move_cursor_left",
        ActionTarget::MoveCursorLeft,
    ),
    static_action(
        "move_cursor_right",
        "Move Cursor Right",
        "keybind: move_cursor_right",
        ActionTarget::MoveCursorRight,
    ),
    static_action(
        "move_cursor_up",
        "Move Cursor Up",
        "keybind: move_cursor_up",
        ActionTarget::MoveCursorUp,
    ),
    static_action(
        "move_cursor_down",
        "Move Cursor Down",
        "keybind: move_cursor_down",
        ActionTarget::MoveCursorDown,
    ),
    static_action(
        "copy_scrollback",
        "Copy Scrollback",
        "keybind: copy_scrollback",
        ActionTarget::CopyScrollback,
    ),
    static_action(
        "confirm_scrollback",
        "Confirm Scrollback",
        "keybind: confirm_scrollback",
        ActionTarget::ConfirmScrollback,
    ),
    static_action(
        "exit_mode",
        "Exit Mode",
        "keybind: exit_mode",
        ActionTarget::ExitMode,
    ),
    static_action(
        "focus_next_pane",
        "Focus Next Pane",
        "keybind: focus_next_pane",
        ActionTarget::Plugin {
            plugin_id: "bmux.windows",
            command_name: "focus-pane-in-direction",
            args: &["--direction", "next"],
        },
    ),
    static_action(
        "focus_previous_pane",
        "Focus Previous Pane",
        "keybind: focus_previous_pane",
        ActionTarget::Plugin {
            plugin_id: "bmux.windows",
            command_name: "focus-pane-in-direction",
            args: &["--direction", "prev"],
        },
    ),
    static_action(
        "focus_prev_pane",
        "Focus Previous Pane",
        "keybind alias: focus_prev_pane",
        ActionTarget::Plugin {
            plugin_id: "bmux.windows",
            command_name: "focus-pane-in-direction",
            args: &["--direction", "prev"],
        },
    ),
    static_action(
        "focus_left_pane",
        "Focus Left Pane",
        "keybind: focus_left_pane",
        ActionTarget::Plugin {
            plugin_id: "bmux.windows",
            command_name: "focus-pane-in-direction",
            args: &["--direction", "left"],
        },
    ),
    static_action(
        "focus_right_pane",
        "Focus Right Pane",
        "keybind: focus_right_pane",
        ActionTarget::Plugin {
            plugin_id: "bmux.windows",
            command_name: "focus-pane-in-direction",
            args: &["--direction", "right"],
        },
    ),
    static_action(
        "focus_up_pane",
        "Focus Up Pane",
        "keybind: focus_up_pane",
        ActionTarget::Plugin {
            plugin_id: "bmux.windows",
            command_name: "focus-pane-in-direction",
            args: &["--direction", "up"],
        },
    ),
    static_action(
        "focus_down_pane",
        "Focus Down Pane",
        "keybind: focus_down_pane",
        ActionTarget::Plugin {
            plugin_id: "bmux.windows",
            command_name: "focus-pane-in-direction",
            args: &["--direction", "down"],
        },
    ),
    static_action(
        "split_focused_vertical",
        "Split Focused Vertical",
        "keybind: split_focused_vertical",
        ActionTarget::Plugin {
            plugin_id: "bmux.windows",
            command_name: "split-pane",
            args: &["--direction", "vertical"],
        },
    ),
    static_action(
        "split_focused_horizontal",
        "Split Focused Horizontal",
        "keybind: split_focused_horizontal",
        ActionTarget::Plugin {
            plugin_id: "bmux.windows",
            command_name: "split-pane",
            args: &["--direction", "horizontal"],
        },
    ),
    static_action(
        "increase_split",
        "Increase Split",
        "keybind: increase_split",
        ActionTarget::Plugin {
            plugin_id: "bmux.windows",
            command_name: "resize-pane",
            args: &["--direction", "increase"],
        },
    ),
    static_action(
        "decrease_split",
        "Decrease Split",
        "keybind: decrease_split",
        ActionTarget::Plugin {
            plugin_id: "bmux.windows",
            command_name: "resize-pane",
            args: &["--direction", "decrease"],
        },
    ),
    static_action(
        "resize_left",
        "Resize Left",
        "keybind: resize_left",
        ActionTarget::Plugin {
            plugin_id: "bmux.windows",
            command_name: "resize-pane",
            args: &["--direction", "left"],
        },
    ),
    static_action(
        "resize_right",
        "Resize Right",
        "keybind: resize_right",
        ActionTarget::Plugin {
            plugin_id: "bmux.windows",
            command_name: "resize-pane",
            args: &["--direction", "right"],
        },
    ),
    static_action(
        "resize_up",
        "Resize Up",
        "keybind: resize_up",
        ActionTarget::Plugin {
            plugin_id: "bmux.windows",
            command_name: "resize-pane",
            args: &["--direction", "up"],
        },
    ),
    static_action(
        "resize_down",
        "Resize Down",
        "keybind: resize_down",
        ActionTarget::Plugin {
            plugin_id: "bmux.windows",
            command_name: "resize-pane",
            args: &["--direction", "down"],
        },
    ),
    static_action(
        "restart_focused_pane",
        "Restart Focused Pane",
        "keybind: restart_focused_pane",
        ActionTarget::Plugin {
            plugin_id: "bmux.windows",
            command_name: "restart-pane",
            args: &[],
        },
    ),
    static_action(
        "close_focused_pane",
        "Close Focused Pane",
        "keybind: close_focused_pane",
        ActionTarget::Plugin {
            plugin_id: "bmux.windows",
            command_name: "close-active-pane",
            args: &[],
        },
    ),
    static_action(
        "zoom_pane",
        "Zoom Pane",
        "keybind: zoom_pane",
        ActionTarget::Plugin {
            plugin_id: "bmux.windows",
            command_name: "zoom-pane",
            args: &[],
        },
    ),
    static_action(
        "window_prev",
        "Previous Window",
        "keybind: window_prev",
        ActionTarget::Plugin {
            plugin_id: "bmux.windows",
            command_name: "prev-window",
            args: &[],
        },
    ),
    static_action(
        "window_next",
        "Next Window",
        "keybind: window_next",
        ActionTarget::Plugin {
            plugin_id: "bmux.windows",
            command_name: "next-window",
            args: &[],
        },
    ),
    static_action(
        "rename_window",
        "Rename Window",
        "keybind: rename_window",
        ActionTarget::Plugin {
            plugin_id: "bmux.windows",
            command_name: "rename-window",
            args: &[],
        },
    ),
    static_action(
        "window_rename",
        "Rename Window",
        "keybind alias: window_rename",
        ActionTarget::Plugin {
            plugin_id: "bmux.windows",
            command_name: "rename-window",
            args: &[],
        },
    ),
    static_action(
        "window_close",
        "Close Window",
        "keybind: window_close",
        ActionTarget::Plugin {
            plugin_id: "bmux.windows",
            command_name: "close-current-window",
            args: &[],
        },
    ),
    static_action(
        "window_goto_1",
        "Go To Window 1",
        "keybind: window_goto_1",
        ActionTarget::Plugin {
            plugin_id: "bmux.windows",
            command_name: "goto-window",
            args: &["1"],
        },
    ),
    static_action(
        "window_goto_2",
        "Go To Window 2",
        "keybind: window_goto_2",
        ActionTarget::Plugin {
            plugin_id: "bmux.windows",
            command_name: "goto-window",
            args: &["2"],
        },
    ),
    static_action(
        "window_goto_3",
        "Go To Window 3",
        "keybind: window_goto_3",
        ActionTarget::Plugin {
            plugin_id: "bmux.windows",
            command_name: "goto-window",
            args: &["3"],
        },
    ),
    static_action(
        "window_goto_4",
        "Go To Window 4",
        "keybind: window_goto_4",
        ActionTarget::Plugin {
            plugin_id: "bmux.windows",
            command_name: "goto-window",
            args: &["4"],
        },
    ),
    static_action(
        "window_goto_5",
        "Go To Window 5",
        "keybind: window_goto_5",
        ActionTarget::Plugin {
            plugin_id: "bmux.windows",
            command_name: "goto-window",
            args: &["5"],
        },
    ),
    static_action(
        "window_goto_6",
        "Go To Window 6",
        "keybind: window_goto_6",
        ActionTarget::Plugin {
            plugin_id: "bmux.windows",
            command_name: "goto-window",
            args: &["6"],
        },
    ),
    static_action(
        "window_goto_7",
        "Go To Window 7",
        "keybind: window_goto_7",
        ActionTarget::Plugin {
            plugin_id: "bmux.windows",
            command_name: "goto-window",
            args: &["7"],
        },
    ),
    static_action(
        "window_goto_8",
        "Go To Window 8",
        "keybind: window_goto_8",
        ActionTarget::Plugin {
            plugin_id: "bmux.windows",
            command_name: "goto-window",
            args: &["8"],
        },
    ),
    static_action(
        "window_goto_9",
        "Go To Window 9",
        "keybind: window_goto_9",
        ActionTarget::Plugin {
            plugin_id: "bmux.windows",
            command_name: "goto-window",
            args: &["9"],
        },
    ),
];

const PARAMETERIZED_BINDABLE_ACTIONS: &[BindableActionInfo] = &[
    BindableActionInfo {
        action: "enter_mode",
        label: "Enter Mode",
        detail: "keybind: enter_mode <mode>",
        argument: Some(BindableActionArgument {
            name: "mode",
            label: "Mode",
            placeholder: "normal",
        }),
    },
    BindableActionInfo {
        action: "switch_profile",
        label: "Switch Profile",
        detail: "keybind: switch_profile <profile>",
        argument: Some(BindableActionArgument {
            name: "profile",
            label: "Profile",
            placeholder: "default",
        }),
    },
];

#[must_use]
pub fn bindable_action_catalog() -> Vec<BindableActionInfo> {
    STATIC_BINDABLE_ACTIONS
        .iter()
        .map(|spec| spec.info)
        .chain(PARAMETERIZED_BINDABLE_ACTIONS.iter().copied())
        .collect()
}

/// Parse a string action name into a `RuntimeAction`.
///
/// Plugin command arguments are preserved verbatim (case-sensitive).
/// Built-in action names and the `plugin:` prefix / plugin ID / command
/// name are matched case-insensitively.
///
/// # Errors
///
/// Returns an error if the action name is not recognized.
pub fn parse_action(value: &str) -> Result<RuntimeAction> {
    let trimmed = value.trim();
    if let Some(mode_action) = parse_enter_mode_action(trimmed) {
        return mode_action;
    }
    if let Some(profile_action) = parse_switch_profile_action(trimmed) {
        return profile_action;
    }
    // Try plugin action first on the original string so that arguments
    // preserve their original case (e.g. file paths, user-entered values).
    if let Some(plugin_action) = parse_plugin_action(trimmed) {
        return plugin_action;
    }
    if let Some(spec) = STATIC_BINDABLE_ACTIONS
        .iter()
        .find(|spec| spec.info.action.eq_ignore_ascii_case(trimmed))
    {
        return Ok(spec.target.to_runtime_action());
    }
    bail!("unknown keymap action '{trimmed}'")
}

fn plugin_command_vec(plugin_id: &str, command_name: &str, args: &[&str]) -> RuntimeAction {
    RuntimeAction::PluginCommand {
        plugin_id: plugin_id.to_string(),
        command_name: command_name.to_string(),
        args: args.iter().map(ToString::to_string).collect(),
    }
}

fn parse_enter_mode_action(value: &str) -> Option<Result<RuntimeAction>> {
    let (command, target_mode) = value.split_once(' ')?;
    if !command.eq_ignore_ascii_case("enter_mode") {
        return None;
    }
    let mode_id = target_mode.trim();
    if mode_id.is_empty() {
        return Some(Err(anyhow::anyhow!(
            "invalid enter_mode action '{value}' (mode id is required)"
        )));
    }
    Some(Ok(RuntimeAction::EnterMode(mode_id.to_ascii_lowercase())))
}

fn parse_switch_profile_action(value: &str) -> Option<Result<RuntimeAction>> {
    let (command, target_profile) = value.split_once(' ')?;
    if !command.eq_ignore_ascii_case("switch_profile") {
        return None;
    }
    let profile_id = target_profile.trim();
    if profile_id.is_empty() {
        return Some(Err(anyhow::anyhow!(
            "invalid switch_profile action '{value}' (profile id is required)"
        )));
    }
    Some(Ok(RuntimeAction::SwitchProfile(
        profile_id.to_ascii_lowercase(),
    )))
}

fn shell_quote(value: &str) -> String {
    if !value.is_empty()
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'/' | b':' | b'=')
        })
    {
        return value.to_string();
    }
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn split_shell_words(value: &str) -> Result<Vec<String>> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut chars = value.chars().peekable();
    let mut quote: Option<char> = None;
    let mut escaped = false;
    while let Some(ch) = chars.next() {
        if escaped {
            current.push(ch);
            escaped = false;
            continue;
        }
        if ch == '\\' && quote != Some('\'') {
            escaped = true;
            continue;
        }
        if let Some(active_quote) = quote {
            if ch == active_quote {
                quote = None;
            } else {
                current.push(ch);
            }
            continue;
        }
        match ch {
            '\'' | '"' => quote = Some(ch),
            ch if ch.is_whitespace() => {
                if !current.is_empty() {
                    words.push(std::mem::take(&mut current));
                }
                while chars.peek().is_some_and(|next| next.is_whitespace()) {
                    let _ = chars.next();
                }
            }
            _ => current.push(ch),
        }
    }
    if escaped {
        current.push('\\');
    }
    if quote.is_some() {
        bail!("invalid plugin keymap action arguments (unterminated quote)");
    }
    if !current.is_empty() {
        words.push(current);
    }
    Ok(words)
}

fn parse_plugin_action(value: &str) -> Option<Result<RuntimeAction>> {
    // Case-insensitive check for the "plugin:" prefix without lowercasing
    // the entire string — arguments must preserve their original case.
    let prefix = "plugin:";
    if value.len() < prefix.len() || !value[..prefix.len()].eq_ignore_ascii_case(prefix) {
        return None;
    }
    let rest = &value[prefix.len()..];
    let Some((plugin_id, remainder)) = rest.split_once(':') else {
        return Some(Err(anyhow::anyhow!(
            "invalid plugin keymap action '{value}' (expected plugin:<plugin-id>:<command>)"
        )));
    };
    if plugin_id.trim().is_empty() || remainder.trim().is_empty() {
        return Some(Err(anyhow::anyhow!(
            "invalid plugin keymap action '{value}' (plugin id and command are required)"
        )));
    }
    let (command_name, args) = match remainder.split_once(' ') {
        Some((cmd, args_str)) => match split_shell_words(args_str) {
            Ok(args) => (cmd, args),
            Err(error) => return Some(Err(error)),
        },
        None => (remainder, Vec::new()),
    };
    if command_name.trim().is_empty() {
        return Some(Err(anyhow::anyhow!(
            "invalid plugin keymap action '{value}' (command name is required)"
        )));
    }
    Some(Ok(RuntimeAction::PluginCommand {
        // Lowercase plugin ID and command name for case-insensitive matching.
        plugin_id: plugin_id.to_ascii_lowercase(),
        command_name: command_name.to_ascii_lowercase(),
        // Arguments are preserved verbatim — they may contain user-provided
        // values (file paths, names from prompt substitution, etc.).
        args,
    }))
}

#[cfg(test)]
mod tests {
    use super::{RuntimeAction, action_to_config_name, bindable_action_catalog, parse_action};
    use std::collections::BTreeSet;

    #[test]
    fn bindable_action_catalog_static_entries_parse() {
        for action in bindable_action_catalog()
            .into_iter()
            .filter(|action| action.argument.is_none())
        {
            parse_action(action.action).unwrap_or_else(|error| {
                panic!("catalog action {:?} should parse: {error}", action.action);
            });
        }
    }

    #[test]
    fn bindable_action_catalog_parameterized_entries_parse_with_sample_values() {
        for action in bindable_action_catalog()
            .into_iter()
            .filter(|action| action.argument.is_some())
        {
            let value = format!("{} sample", action.action);
            parse_action(&value).unwrap_or_else(|error| {
                panic!("parameterized catalog action {value:?} should parse: {error}");
            });
        }
    }

    #[test]
    fn bindable_action_catalog_has_no_duplicate_actions() {
        let mut seen = BTreeSet::new();
        for action in bindable_action_catalog() {
            assert!(
                seen.insert(action.action),
                "duplicate bindable action {:?}",
                action.action
            );
        }
    }

    #[test]
    fn parse_action_accepts_quit_destroy_alias() {
        assert_eq!(
            parse_action("quit_destroy").expect("alias should parse"),
            RuntimeAction::Quit
        );
    }

    #[test]
    fn parse_action_accepts_plugin_command_action() {
        let action =
            parse_action("plugin:bmux.windows:new-window").expect("plugin action should parse");
        assert_eq!(
            action,
            RuntimeAction::PluginCommand {
                plugin_id: "bmux.windows".to_string(),
                command_name: "new-window".to_string(),
                args: vec![],
            }
        );
    }

    #[test]
    fn parse_action_accepts_plugin_command_with_args() {
        let action = parse_action("plugin:bmux.windows:goto-window 1")
            .expect("plugin action with args should parse");
        assert_eq!(
            action,
            RuntimeAction::PluginCommand {
                plugin_id: "bmux.windows".to_string(),
                command_name: "goto-window".to_string(),
                args: vec!["1".to_string()],
            }
        );
    }

    #[test]
    fn parse_action_maps_legacy_window_actions_to_plugin_commands() {
        assert_eq!(
            parse_action("window_prev").expect("legacy previous-window action should parse"),
            RuntimeAction::PluginCommand {
                plugin_id: "bmux.windows".to_string(),
                command_name: "prev-window".to_string(),
                args: Vec::new(),
            }
        );
        assert_eq!(
            parse_action("window_goto_3").expect("legacy goto-window action should parse"),
            RuntimeAction::PluginCommand {
                plugin_id: "bmux.windows".to_string(),
                command_name: "goto-window".to_string(),
                args: vec!["3".to_string()],
            }
        );
        assert_eq!(
            parse_action("window_close").expect("legacy close-window action should parse"),
            RuntimeAction::PluginCommand {
                plugin_id: "bmux.windows".to_string(),
                command_name: "close-current-window".to_string(),
                args: Vec::new(),
            }
        );
        assert_eq!(
            parse_action("window_rename").expect("legacy rename-window action should parse"),
            RuntimeAction::PluginCommand {
                plugin_id: "bmux.windows".to_string(),
                command_name: "rename-window".to_string(),
                args: Vec::new(),
            }
        );
    }

    #[test]
    fn parse_action_maps_legacy_focus_actions_to_plugin_commands() {
        assert_eq!(
            parse_action("focus_prev_pane").expect("legacy focus previous action should parse"),
            RuntimeAction::PluginCommand {
                plugin_id: "bmux.windows".to_string(),
                command_name: "focus-pane-in-direction".to_string(),
                args: vec!["--direction".to_string(), "prev".to_string()],
            }
        );
        assert_eq!(
            parse_action("focus_left_pane").expect("legacy focus left action should parse"),
            RuntimeAction::PluginCommand {
                plugin_id: "bmux.windows".to_string(),
                command_name: "focus-pane-in-direction".to_string(),
                args: vec!["--direction".to_string(), "left".to_string()],
            }
        );
    }

    #[test]
    fn parse_action_maps_legacy_resize_actions_to_plugin_commands() {
        assert_eq!(
            parse_action("increase_split").expect("legacy increase split action should parse"),
            RuntimeAction::PluginCommand {
                plugin_id: "bmux.windows".to_string(),
                command_name: "resize-pane".to_string(),
                args: vec!["--direction".to_string(), "increase".to_string()],
            }
        );
        assert_eq!(
            parse_action("resize_left").expect("legacy resize left action should parse"),
            RuntimeAction::PluginCommand {
                plugin_id: "bmux.windows".to_string(),
                command_name: "resize-pane".to_string(),
                args: vec!["--direction".to_string(), "left".to_string()],
            }
        );
    }

    #[test]
    fn parse_action_maps_legacy_split_and_zoom_actions_to_plugin_commands() {
        assert_eq!(
            parse_action("split_focused_vertical")
                .expect("legacy vertical split action should parse"),
            RuntimeAction::PluginCommand {
                plugin_id: "bmux.windows".to_string(),
                command_name: "split-pane".to_string(),
                args: vec!["--direction".to_string(), "vertical".to_string()],
            }
        );
        assert_eq!(
            parse_action("split_focused_horizontal")
                .expect("legacy horizontal split action should parse"),
            RuntimeAction::PluginCommand {
                plugin_id: "bmux.windows".to_string(),
                command_name: "split-pane".to_string(),
                args: vec!["--direction".to_string(), "horizontal".to_string()],
            }
        );
        assert_eq!(
            parse_action("zoom_pane").expect("legacy zoom action should parse"),
            RuntimeAction::PluginCommand {
                plugin_id: "bmux.windows".to_string(),
                command_name: "zoom-pane".to_string(),
                args: Vec::new(),
            }
        );
    }

    #[test]
    fn parse_action_maps_legacy_restart_action_to_plugin_command() {
        assert_eq!(
            parse_action("restart_focused_pane").expect("legacy restart action should parse"),
            RuntimeAction::PluginCommand {
                plugin_id: "bmux.windows".to_string(),
                command_name: "restart-pane".to_string(),
                args: Vec::new(),
            }
        );
    }

    #[test]
    fn parse_action_accepts_plugin_command_with_multiple_args() {
        let action = parse_action("plugin:bmux.windows:switch-window --session dev")
            .expect("plugin action with multiple args should parse");
        assert_eq!(
            action,
            RuntimeAction::PluginCommand {
                plugin_id: "bmux.windows".to_string(),
                command_name: "switch-window".to_string(),
                args: vec!["--session".to_string(), "dev".to_string()],
            }
        );
    }

    #[test]
    fn parse_action_accepts_quoted_plugin_args() {
        let action = parse_action("plugin:bmux.test:run --name 'hello world'")
            .expect("plugin action with quoted args should parse");
        assert_eq!(
            action,
            RuntimeAction::PluginCommand {
                plugin_id: "bmux.test".to_string(),
                command_name: "run".to_string(),
                args: vec!["--name".to_string(), "hello world".to_string()],
            }
        );
    }

    #[test]
    fn action_to_config_name_quotes_plugin_args_with_spaces() {
        let action = RuntimeAction::PluginCommand {
            plugin_id: "bmux.test".to_string(),
            command_name: "run".to_string(),
            args: vec!["hello world".to_string()],
        };
        assert_eq!(
            action_to_config_name(&action),
            "plugin:bmux.test:run 'hello world'"
        );
    }

    #[test]
    fn action_to_config_name_serializes_plugin_command_action() {
        let action = RuntimeAction::PluginCommand {
            plugin_id: "bmux.windows".to_string(),
            command_name: "new-window".to_string(),
            args: vec![],
        };
        assert_eq!(
            action_to_config_name(&action),
            "plugin:bmux.windows:new-window"
        );
    }

    #[test]
    fn action_to_config_name_serializes_plugin_command_with_args() {
        let action = RuntimeAction::PluginCommand {
            plugin_id: "bmux.windows".to_string(),
            command_name: "goto-window".to_string(),
            args: vec!["1".to_string()],
        };
        assert_eq!(
            action_to_config_name(&action),
            "plugin:bmux.windows:goto-window 1"
        );
    }

    #[test]
    fn parse_action_preserves_plugin_argument_case() {
        let action = parse_action("plugin:bmux.test:cmd --name MyRecording /tmp/MyFile.gif")
            .expect("should parse");
        assert_eq!(
            action,
            RuntimeAction::PluginCommand {
                plugin_id: "bmux.test".to_string(),
                command_name: "cmd".to_string(),
                args: vec![
                    "--name".to_string(),
                    "MyRecording".to_string(),
                    "/tmp/MyFile.gif".to_string(),
                ],
            }
        );
    }

    #[test]
    fn parse_action_lowercases_plugin_id_and_command() {
        let action =
            parse_action("Plugin:Bmux.Windows:New-Window").expect("mixed case should parse");
        assert_eq!(
            action,
            RuntimeAction::PluginCommand {
                plugin_id: "bmux.windows".to_string(),
                command_name: "new-window".to_string(),
                args: vec![],
            }
        );
    }

    #[test]
    fn parse_action_is_case_insensitive_for_builtins() {
        assert_eq!(
            parse_action("QUIT").expect("uppercase built-in should parse"),
            RuntimeAction::Quit
        );
        assert_eq!(
            parse_action("Focus_Next_Pane").expect("mixed case built-in should parse"),
            RuntimeAction::PluginCommand {
                plugin_id: "bmux.windows".to_string(),
                command_name: "focus-pane-in-direction".to_string(),
                args: vec!["--direction".to_string(), "next".to_string()],
            }
        );
    }

    #[test]
    fn parse_action_accepts_enter_mode_action() {
        assert_eq!(
            parse_action("enter_mode insert").expect("enter_mode should parse"),
            RuntimeAction::EnterMode("insert".to_string())
        );
    }

    #[test]
    fn parse_action_normalizes_enter_mode_target_case() {
        assert_eq!(
            parse_action("ENTER_MODE InSeRt").expect("mixed case enter_mode should parse"),
            RuntimeAction::EnterMode("insert".to_string())
        );
    }

    #[test]
    fn action_to_config_name_serializes_enter_mode_action() {
        assert_eq!(
            action_to_config_name(&RuntimeAction::EnterMode("normal".to_string())),
            "enter_mode normal"
        );
    }

    #[test]
    fn parse_action_accepts_switch_profile_action() {
        assert_eq!(
            parse_action("switch_profile Vim").expect("switch_profile should parse"),
            RuntimeAction::SwitchProfile("vim".to_string())
        );
    }

    #[test]
    fn action_to_config_name_serializes_switch_profile_action() {
        assert_eq!(
            action_to_config_name(&RuntimeAction::SwitchProfile("zellij_compat".to_string())),
            "switch_profile zellij_compat"
        );
    }
}
