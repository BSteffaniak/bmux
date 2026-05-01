#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]
#![cfg_attr(feature = "static-bundled", allow(dead_code))]

use bmux_plugin::{action_dispatch, prompt};
use bmux_plugin_sdk::prelude::*;
use bmux_plugin_sdk::{
    PluginCommand, PluginCommandArgument, PluginCommandArgumentKind, PromptOption, PromptRequest,
    PromptResponse, PromptValidation, PromptValue, RegisteredPluginInfo,
};
use tracing::{debug, warn};

#[derive(Default)]
pub struct CommandPalettePlugin;

impl RustPlugin for CommandPalettePlugin {
    fn run_command(&mut self, context: NativeCommandContext) -> Result<i32, PluginCommandError> {
        bmux_plugin_sdk::route_command!(context, {
            "show" => show_command_palette(&context),
        })
    }
}

fn show_command_palette(context: &NativeCommandContext) -> Result<i32, PluginCommandError> {
    let handle = tokio::runtime::Handle::try_current().map_err(|_| {
        PluginCommandError::unavailable(
            "no tokio runtime available — command palette requires the attach runtime",
        )
    })?;
    handle.spawn(run_command_palette(context.clone()));
    Ok(EXIT_OK)
}

#[derive(Debug, Clone)]
enum PaletteEntryKind {
    BuiltIn {
        action: String,
    },
    PluginCommand {
        plugin_id: String,
        command_name: String,
        schema: PluginCommand,
    },
}

#[derive(Debug, Clone)]
struct PaletteEntry {
    label: String,
    detail: String,
    kind: PaletteEntryKind,
}

async fn run_command_palette(context: NativeCommandContext) {
    let entries = build_palette_entries(&context);
    if entries.is_empty() {
        warn!("command palette: no actions available");
        return;
    }

    let options = entries
        .iter()
        .enumerate()
        .map(|(index, entry)| {
            PromptOption::new(
                index.to_string(),
                format!("{}  —  {}", entry.label, entry.detail),
            )
        })
        .collect::<Vec<_>>();

    let request = PromptRequest::search_select("Command Palette", options)
        .message("Type to fuzzy-search actions and commands")
        .modal_id("command-palette")
        .owner_plugin_id("bmux.command_palette")
        .width_range(64, 120)
        .search_placeholder("Search commands");

    let selected = match prompt::request(request).await {
        Ok(PromptResponse::Submitted(PromptValue::Single(value))) => value,
        Ok(
            PromptResponse::Cancelled | PromptResponse::RejectedBusy | PromptResponse::Submitted(_),
        ) => return,
        Err(error) => {
            warn!(%error, "command palette: prompt failed");
            return;
        }
    };

    let Ok(index) = selected.parse::<usize>() else {
        warn!(selected, "command palette: invalid selection value");
        return;
    };
    let Some(entry) = entries.get(index).cloned() else {
        warn!(index, "command palette: selection out of range");
        return;
    };

    match entry.kind {
        PaletteEntryKind::BuiltIn { action } => dispatch_action(&action),
        PaletteEntryKind::PluginCommand {
            plugin_id,
            command_name,
            schema,
        } => {
            let Some(args) = collect_command_arguments(&schema).await else {
                return;
            };
            let action = plugin_action_string(&plugin_id, &command_name, &args);
            dispatch_action(&action);
        }
    }
}

fn dispatch_action(action: &str) {
    debug!(%action, "command palette: dispatching action");
    if let Err(error) = action_dispatch::dispatch(action) {
        warn!(%action, %error, "command palette: dispatch failed");
    }
}

fn build_palette_entries(context: &NativeCommandContext) -> Vec<PaletteEntry> {
    let mut entries = built_in_entries();
    entries.extend(plugin_command_entries(
        &context.registered_plugins,
        &context.enabled_plugins,
    ));
    entries.sort_by(|left, right| {
        left.label
            .cmp(&right.label)
            .then_with(|| left.detail.cmp(&right.detail))
    });
    entries
}

fn built_in_entries() -> Vec<PaletteEntry> {
    [
        ("No Op", "no_op", "no_op"),
        ("Quit", "quit", "quit"),
        ("Detach", "detach", "detach"),
        ("Show Help", "show_help", "show_help"),
        (
            "Enter Scroll Mode",
            "enter_scroll_mode",
            "enter_scroll_mode",
        ),
        ("Exit Scroll Mode", "exit_scroll_mode", "exit_scroll_mode"),
        ("Scroll Up Line", "scroll_up_line", "scroll_up_line"),
        ("Scroll Down Line", "scroll_down_line", "scroll_down_line"),
        ("Scroll Up Page", "scroll_up_page", "scroll_up_page"),
        ("Scroll Down Page", "scroll_down_page", "scroll_down_page"),
        ("Scroll Top", "scroll_top", "scroll_top"),
        ("Scroll Bottom", "scroll_bottom", "scroll_bottom"),
        ("Begin Selection", "begin_selection", "begin_selection"),
        ("Move Cursor Left", "move_cursor_left", "move_cursor_left"),
        (
            "Move Cursor Right",
            "move_cursor_right",
            "move_cursor_right",
        ),
        ("Move Cursor Up", "move_cursor_up", "move_cursor_up"),
        ("Move Cursor Down", "move_cursor_down", "move_cursor_down"),
        ("Copy Scrollback", "copy_scrollback", "copy_scrollback"),
        (
            "Confirm Scrollback",
            "confirm_scrollback",
            "confirm_scrollback",
        ),
        ("Exit Mode", "exit_mode", "exit_mode"),
    ]
    .into_iter()
    .map(|(label, detail, action)| PaletteEntry {
        label: label.to_string(),
        detail: format!("built-in: {detail}"),
        kind: PaletteEntryKind::BuiltIn {
            action: action.to_string(),
        },
    })
    .collect()
}

fn plugin_command_entries(
    plugins: &[RegisteredPluginInfo],
    enabled_plugins: &[String],
) -> Vec<PaletteEntry> {
    let enabled = enabled_plugins
        .iter()
        .collect::<std::collections::BTreeSet<_>>();
    plugins
        .iter()
        .filter(|plugin| enabled.contains(&plugin.id))
        .flat_map(|plugin| {
            plugin.command_schemas.iter().map(|command| PaletteEntry {
                label: command_label(plugin, command),
                detail: format!("{}:{}", plugin.id, command.name),
                kind: PaletteEntryKind::PluginCommand {
                    plugin_id: plugin.id.clone(),
                    command_name: command.name.clone(),
                    schema: command.clone(),
                },
            })
        })
        .collect()
}

fn command_label(plugin: &RegisteredPluginInfo, command: &PluginCommand) -> String {
    if command.summary.is_empty() {
        format!("{}: {}", plugin.display_name, command.name)
    } else {
        format!("{}: {}", plugin.display_name, command.summary)
    }
}

async fn collect_command_arguments(command: &PluginCommand) -> Option<Vec<String>> {
    let mut args = Vec::new();
    for argument in &command.arguments {
        let values = prompt_for_argument(command, argument).await?;
        append_argument_values(&mut args, argument, values);
    }
    Some(args)
}

async fn prompt_for_argument(
    command: &PluginCommand,
    argument: &PluginCommandArgument,
) -> Option<Vec<String>> {
    match argument.kind {
        PluginCommandArgumentKind::Boolean => prompt_boolean_argument(command, argument).await,
        PluginCommandArgumentKind::Choice => prompt_choice_argument(command, argument).await,
        PluginCommandArgumentKind::Integer => {
            prompt_text_argument(
                command,
                argument,
                Some(PromptValidation::Integer),
                "integer value",
            )
            .await
        }
        PluginCommandArgumentKind::String | PluginCommandArgumentKind::Path => {
            prompt_text_argument(command, argument, None, "value").await
        }
    }
}

async fn prompt_boolean_argument(
    command: &PluginCommand,
    argument: &PluginCommandArgument,
) -> Option<Vec<String>> {
    let response = prompt::request(
        PromptRequest::confirm(argument_prompt_title(command, argument))
            .message(optional_argument_message(argument))
            .confirm_default(false)
            .confirm_labels("Yes", "No")
            .modal_id("command-palette-argument"),
    )
    .await
    .ok()?;
    match response {
        PromptResponse::Submitted(PromptValue::Confirm(true)) => Some(vec!["true".to_string()]),
        PromptResponse::Submitted(PromptValue::Confirm(false)) => Some(Vec::new()),
        PromptResponse::Cancelled | PromptResponse::RejectedBusy | PromptResponse::Submitted(_) => {
            None
        }
    }
}

async fn prompt_choice_argument(
    command: &PluginCommand,
    argument: &PluginCommandArgument,
) -> Option<Vec<String>> {
    let mut options = Vec::new();
    if !argument.required {
        options.push(PromptOption::new(String::new(), "Skip"));
    }
    options.extend(
        argument
            .choice_values
            .iter()
            .map(|value| PromptOption::new(value.clone(), value.clone())),
    );
    let response = prompt::request(
        PromptRequest::single_select(argument_prompt_title(command, argument), options)
            .message(optional_argument_message(argument))
            .modal_id("command-palette-argument"),
    )
    .await
    .ok()?;
    match response {
        PromptResponse::Submitted(PromptValue::Single(value)) if value.is_empty() => {
            Some(Vec::new())
        }
        PromptResponse::Submitted(PromptValue::Single(value)) => Some(vec![value]),
        PromptResponse::Cancelled | PromptResponse::RejectedBusy | PromptResponse::Submitted(_) => {
            None
        }
    }
}

async fn prompt_text_argument(
    command: &PluginCommand,
    argument: &PluginCommandArgument,
    validation: Option<PromptValidation>,
    placeholder: &str,
) -> Option<Vec<String>> {
    let mut request = PromptRequest::text_input(argument_prompt_title(command, argument))
        .message(optional_argument_message(argument))
        .input_placeholder(placeholder)
        .input_required(argument.required)
        .modal_id("command-palette-argument");
    if let Some(validation) = validation {
        request = request.input_validation(validation);
    }
    let response = prompt::request(request).await.ok()?;
    match response {
        PromptResponse::Submitted(PromptValue::Text(value)) => {
            if value.is_empty() && !argument.required {
                Some(Vec::new())
            } else if argument.multiple || argument.trailing_var_arg {
                Some(value.split_whitespace().map(ToString::to_string).collect())
            } else {
                Some(vec![value])
            }
        }
        PromptResponse::Cancelled | PromptResponse::RejectedBusy | PromptResponse::Submitted(_) => {
            None
        }
    }
}

fn argument_prompt_title(command: &PluginCommand, argument: &PluginCommandArgument) -> String {
    format!("{}: {}", command.name, argument_display_name(argument))
}

fn optional_argument_message(argument: &PluginCommandArgument) -> String {
    let requirement = if argument.required {
        "Required argument."
    } else {
        "Optional argument; leave blank or choose Skip to omit."
    };
    argument.summary.as_ref().map_or_else(
        || requirement.to_string(),
        |summary| format!("{summary}\n{requirement}"),
    )
}

fn argument_display_name(argument: &PluginCommandArgument) -> String {
    argument
        .long
        .as_ref()
        .map_or_else(|| argument.name.clone(), |long| format!("--{long}"))
}

fn append_argument_values(
    args: &mut Vec<String>,
    argument: &PluginCommandArgument,
    values: Vec<String>,
) {
    if values.is_empty() {
        return;
    }
    if matches!(argument.kind, PluginCommandArgumentKind::Boolean) {
        if let Some(long) = &argument.long {
            args.push(format!("--{long}"));
        }
        return;
    }
    if let Some(long) = &argument.long {
        for value in values {
            args.push(format!("--{long}"));
            args.push(value);
        }
    } else {
        args.extend(values);
    }
}

fn plugin_action_string(plugin_id: &str, command_name: &str, args: &[String]) -> String {
    let mut action = format!("plugin:{plugin_id}:{command_name}");
    if !args.is_empty() {
        action.push(' ');
        action.push_str(
            &args
                .iter()
                .map(|arg| shell_quote(arg))
                .collect::<Vec<_>>()
                .join(" "),
        );
    }
    action
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

bmux_plugin_sdk::export_plugin!(CommandPalettePlugin, include_str!("../plugin.toml"));

#[cfg(test)]
mod tests {
    use super::{plugin_action_string, shell_quote};

    #[test]
    fn shell_quote_leaves_simple_values_unquoted() {
        assert_eq!(shell_quote("--name"), "--name");
        assert_eq!(shell_quote("dev/session"), "dev/session");
    }

    #[test]
    fn shell_quote_quotes_spaces() {
        assert_eq!(shell_quote("hello world"), "'hello world'");
    }

    #[test]
    fn plugin_action_string_quotes_arguments() {
        assert_eq!(
            plugin_action_string("bmux.test", "run", &["hello world".to_string()]),
            "plugin:bmux.test:run 'hello world'"
        );
    }
}
