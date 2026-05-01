#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

use anyhow::{Result, bail};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuntimeAction {
    NoOp,
    Quit,
    Detach,
    CloseFocusedPane,
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

#[must_use]
pub const fn action_to_name(action: &RuntimeAction) -> &'static str {
    match action {
        RuntimeAction::NoOp => "no_op",
        RuntimeAction::Quit => "quit",
        RuntimeAction::Detach => "detach",
        RuntimeAction::CloseFocusedPane => "close_focused_pane",
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
    // Built-in actions are single tokens — safe to lowercase for
    // case-insensitive matching.
    let normalized = trimmed.to_ascii_lowercase();
    match normalized.as_str() {
        "quit" | "quit_destroy" => Ok(RuntimeAction::Quit),
        "detach" => Ok(RuntimeAction::Detach),
        "focus_next_pane" => Ok(windows_focus_command("next")),
        "focus_previous_pane" | "focus_prev_pane" => Ok(windows_focus_command("prev")),
        "focus_left_pane" => Ok(windows_focus_command("left")),
        "focus_right_pane" => Ok(windows_focus_command("right")),
        "focus_up_pane" => Ok(windows_focus_command("up")),
        "focus_down_pane" => Ok(windows_focus_command("down")),
        "no_op" | "toggle_split_direction" | "enter_window_mode" => Ok(RuntimeAction::NoOp),
        "split_focused_vertical" => Ok(windows_split_command("vertical")),
        "split_focused_horizontal" => Ok(windows_split_command("horizontal")),
        "increase_split" => Ok(windows_resize_command("increase")),
        "decrease_split" => Ok(windows_resize_command("decrease")),
        "resize_left" => Ok(windows_resize_command("left")),
        "resize_right" => Ok(windows_resize_command("right")),
        "resize_up" => Ok(windows_resize_command("up")),
        "resize_down" => Ok(windows_resize_command("down")),
        "restart_focused_pane" => Ok(plugin_command("bmux.windows", "restart-pane", [])),
        "close_focused_pane" => Ok(RuntimeAction::CloseFocusedPane),
        "zoom_pane" => Ok(plugin_command("bmux.windows", "zoom-pane", [])),
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
        "exit_mode" => Ok(RuntimeAction::ExitMode),
        "window_prev" => Ok(plugin_command("bmux.windows", "prev-window", [])),
        "window_next" => Ok(plugin_command("bmux.windows", "next-window", [])),
        "window_goto_1" => Ok(plugin_command("bmux.windows", "goto-window", ["1"])),
        "window_goto_2" => Ok(plugin_command("bmux.windows", "goto-window", ["2"])),
        "window_goto_3" => Ok(plugin_command("bmux.windows", "goto-window", ["3"])),
        "window_goto_4" => Ok(plugin_command("bmux.windows", "goto-window", ["4"])),
        "window_goto_5" => Ok(plugin_command("bmux.windows", "goto-window", ["5"])),
        "window_goto_6" => Ok(plugin_command("bmux.windows", "goto-window", ["6"])),
        "window_goto_7" => Ok(plugin_command("bmux.windows", "goto-window", ["7"])),
        "window_goto_8" => Ok(plugin_command("bmux.windows", "goto-window", ["8"])),
        "window_goto_9" => Ok(plugin_command("bmux.windows", "goto-window", ["9"])),
        "window_close" => Ok(plugin_command("bmux.windows", "close-current-window", [])),
        unknown => bail!("unknown keymap action '{unknown}'"),
    }
}

fn plugin_command<const N: usize>(
    plugin_id: &str,
    command_name: &str,
    args: [&str; N],
) -> RuntimeAction {
    RuntimeAction::PluginCommand {
        plugin_id: plugin_id.to_string(),
        command_name: command_name.to_string(),
        args: args.into_iter().map(ToString::to_string).collect(),
    }
}

fn windows_focus_command(direction: &str) -> RuntimeAction {
    plugin_command(
        "bmux.windows",
        "focus-pane-in-direction",
        ["--direction", direction],
    )
}

fn windows_resize_command(direction: &str) -> RuntimeAction {
    plugin_command("bmux.windows", "resize-pane", ["--direction", direction])
}

fn windows_split_command(direction: &str) -> RuntimeAction {
    plugin_command("bmux.windows", "split-pane", ["--direction", direction])
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
