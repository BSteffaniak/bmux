#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

use anyhow::{Result, bail};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuntimeAction {
    Quit,
    Detach,
    NewWindow,
    NewSession,
    SessionPrev,
    SessionNext,
    FocusNext,
    FocusPrev,
    FocusLeft,
    FocusRight,
    FocusUp,
    FocusDown,
    ToggleSplitDirection,
    SplitFocusedVertical,
    SplitFocusedHorizontal,
    IncreaseSplit,
    DecreaseSplit,
    ResizeLeft,
    ResizeRight,
    ResizeUp,
    ResizeDown,
    RestartFocusedPane,
    CloseFocusedPane,
    ZoomPane,
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
    EnterWindowMode,
    ExitMode,
    WindowPrev,
    WindowNext,
    WindowGoto1,
    WindowGoto2,
    WindowGoto3,
    WindowGoto4,
    WindowGoto5,
    WindowGoto6,
    WindowGoto7,
    WindowGoto8,
    WindowGoto9,
    WindowClose,
    PluginCommand {
        plugin_id: String,
        command_name: String,
        args: Vec<String>,
    },
    ForwardToPane(Vec<u8>),
}

#[must_use]
pub const fn action_to_name(action: &RuntimeAction) -> &'static str {
    match action {
        RuntimeAction::Quit => "quit",
        RuntimeAction::Detach => "detach",
        RuntimeAction::NewWindow => "new_window",
        RuntimeAction::NewSession => "new_session",
        RuntimeAction::SessionPrev => "session_prev",
        RuntimeAction::SessionNext => "session_next",
        RuntimeAction::FocusNext => "focus_next_pane",
        RuntimeAction::FocusPrev => "focus_previous_pane",
        RuntimeAction::FocusLeft => "focus_left_pane",
        RuntimeAction::FocusRight => "focus_right_pane",
        RuntimeAction::FocusUp => "focus_up_pane",
        RuntimeAction::FocusDown => "focus_down_pane",
        RuntimeAction::ToggleSplitDirection => "toggle_split_direction",
        RuntimeAction::SplitFocusedVertical => "split_focused_vertical",
        RuntimeAction::SplitFocusedHorizontal => "split_focused_horizontal",
        RuntimeAction::IncreaseSplit => "increase_split",
        RuntimeAction::DecreaseSplit => "decrease_split",
        RuntimeAction::ResizeLeft => "resize_left",
        RuntimeAction::ResizeRight => "resize_right",
        RuntimeAction::ResizeUp => "resize_up",
        RuntimeAction::ResizeDown => "resize_down",
        RuntimeAction::RestartFocusedPane => "restart_focused_pane",
        RuntimeAction::CloseFocusedPane => "close_focused_pane",
        RuntimeAction::ZoomPane => "zoom_pane",
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
        RuntimeAction::EnterWindowMode => "enter_window_mode",
        RuntimeAction::ExitMode => "exit_mode",
        RuntimeAction::WindowPrev => "window_prev",
        RuntimeAction::WindowNext => "window_next",
        RuntimeAction::WindowGoto1 => "window_goto_1",
        RuntimeAction::WindowGoto2 => "window_goto_2",
        RuntimeAction::WindowGoto3 => "window_goto_3",
        RuntimeAction::WindowGoto4 => "window_goto_4",
        RuntimeAction::WindowGoto5 => "window_goto_5",
        RuntimeAction::WindowGoto6 => "window_goto_6",
        RuntimeAction::WindowGoto7 => "window_goto_7",
        RuntimeAction::WindowGoto8 => "window_goto_8",
        RuntimeAction::WindowGoto9 => "window_goto_9",
        RuntimeAction::WindowClose => "window_close",
        RuntimeAction::PluginCommand { .. } => "plugin_command",
        RuntimeAction::ForwardToPane(_) => "forward_to_pane",
    }
}

#[must_use]
pub fn action_to_config_name(action: &RuntimeAction) -> String {
    match action {
        RuntimeAction::PluginCommand {
            plugin_id,
            command_name,
            args,
        } => {
            if args.is_empty() {
                format!("plugin:{plugin_id}:{command_name}")
            } else {
                format!("plugin:{plugin_id}:{command_name} {}", args.join(" "))
            }
        }
        _ => action_to_name(action).to_string(),
    }
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
    // Try plugin action first on the original string so that arguments
    // preserve their original case (e.g. file paths, user-entered values).
    if let Some(plugin_action) = parse_plugin_action(trimmed) {
        return plugin_action;
    }
    // Built-in actions are single tokens — safe to lowercase for
    // case-insensitive matching.
    let normalized = trimmed.to_ascii_lowercase();
    match normalized.as_str() {
        "quit" | "quit_destroy" => Ok(RuntimeAction::Quit),
        "detach" => Ok(RuntimeAction::Detach),
        "new_window" => Ok(RuntimeAction::NewWindow),
        "new_session" => Ok(RuntimeAction::NewSession),
        "session_prev" => Ok(RuntimeAction::SessionPrev),
        "session_next" => Ok(RuntimeAction::SessionNext),
        "focus_next_pane" => Ok(RuntimeAction::FocusNext),
        "focus_previous_pane" | "focus_prev_pane" => Ok(RuntimeAction::FocusPrev),
        "focus_left_pane" => Ok(RuntimeAction::FocusLeft),
        "focus_right_pane" => Ok(RuntimeAction::FocusRight),
        "focus_up_pane" => Ok(RuntimeAction::FocusUp),
        "focus_down_pane" => Ok(RuntimeAction::FocusDown),
        "toggle_split_direction" => Ok(RuntimeAction::ToggleSplitDirection),
        "split_focused_vertical" => Ok(RuntimeAction::SplitFocusedVertical),
        "split_focused_horizontal" => Ok(RuntimeAction::SplitFocusedHorizontal),
        "increase_split" => Ok(RuntimeAction::IncreaseSplit),
        "decrease_split" => Ok(RuntimeAction::DecreaseSplit),
        "resize_left" => Ok(RuntimeAction::ResizeLeft),
        "resize_right" => Ok(RuntimeAction::ResizeRight),
        "resize_up" => Ok(RuntimeAction::ResizeUp),
        "resize_down" => Ok(RuntimeAction::ResizeDown),
        "restart_focused_pane" => Ok(RuntimeAction::RestartFocusedPane),
        "close_focused_pane" => Ok(RuntimeAction::CloseFocusedPane),
        "zoom_pane" => Ok(RuntimeAction::ZoomPane),
        "show_help" => Ok(RuntimeAction::ShowHelp),
        "enter_scroll_mode" => Ok(RuntimeAction::EnterScrollMode),
        "exit_scroll_mode" => Ok(RuntimeAction::ExitScrollMode),
        "scroll_up_line" => Ok(RuntimeAction::ScrollUpLine),
        "scroll_down_line" => Ok(RuntimeAction::ScrollDownLine),
        "scroll_up_page" => Ok(RuntimeAction::ScrollUpPage),
        "scroll_down_page" => Ok(RuntimeAction::ScrollDownPage),
        "scroll_top" => Ok(RuntimeAction::ScrollTop),
        "scroll_bottom" => Ok(RuntimeAction::ScrollBottom),
        "begin_selection" => Ok(RuntimeAction::BeginSelection),
        "move_cursor_left" => Ok(RuntimeAction::MoveCursorLeft),
        "move_cursor_right" => Ok(RuntimeAction::MoveCursorRight),
        "move_cursor_up" => Ok(RuntimeAction::MoveCursorUp),
        "move_cursor_down" => Ok(RuntimeAction::MoveCursorDown),
        "copy_scrollback" => Ok(RuntimeAction::CopyScrollback),
        "confirm_scrollback" => Ok(RuntimeAction::ConfirmScrollback),
        "enter_window_mode" => Ok(RuntimeAction::EnterWindowMode),
        "exit_mode" => Ok(RuntimeAction::ExitMode),
        "window_prev" => Ok(RuntimeAction::WindowPrev),
        "window_next" => Ok(RuntimeAction::WindowNext),
        "window_goto_1" => Ok(RuntimeAction::WindowGoto1),
        "window_goto_2" => Ok(RuntimeAction::WindowGoto2),
        "window_goto_3" => Ok(RuntimeAction::WindowGoto3),
        "window_goto_4" => Ok(RuntimeAction::WindowGoto4),
        "window_goto_5" => Ok(RuntimeAction::WindowGoto5),
        "window_goto_6" => Ok(RuntimeAction::WindowGoto6),
        "window_goto_7" => Ok(RuntimeAction::WindowGoto7),
        "window_goto_8" => Ok(RuntimeAction::WindowGoto8),
        "window_goto_9" => Ok(RuntimeAction::WindowGoto9),
        "window_close" => Ok(RuntimeAction::WindowClose),
        unknown => bail!("unknown keymap action '{unknown}'"),
    }
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
        Some((cmd, args_str)) => (
            cmd,
            args_str
                .split_whitespace()
                .map(String::from)
                .collect::<Vec<_>>(),
        ),
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
    use super::{RuntimeAction, action_to_config_name, parse_action};

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
            RuntimeAction::FocusNext
        );
    }
}
