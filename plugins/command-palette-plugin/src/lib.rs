#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]
#![cfg_attr(feature = "static-bundled", allow(dead_code))]

use bmux_keybind::{
    BindableActionArgument, RuntimeAction, action_to_config_name, bindable_action_catalog,
    parse_action,
};
use bmux_plugin::{action_dispatch, prompt};
use bmux_plugin_sdk::prelude::*;
use bmux_plugin_sdk::{
    ActiveKeybinding, PluginCommand, PluginCommandArgument, PluginCommandArgumentKind,
    PromptOption, PromptRequest, PromptResponse, PromptValidation, PromptValue,
    RegisteredPluginInfo,
};
use tracing::{debug, warn};

#[derive(Default)]
pub struct CommandPalettePlugin;

impl RustPlugin for CommandPalettePlugin {
    type Contract = bmux_plugin_sdk::NoPluginContract;

    fn run_command(&mut self, context: NativeCommandContext) -> Result<i32, PluginCommandError> {
        bmux_plugin_sdk::route_command!(context, {
            "show" => show_command_palette(&context),
        })
    }
}

fn show_command_palette(context: &NativeCommandContext) -> Result<i32, PluginCommandError> {
    let entries = build_palette_entries(context);
    if entries.is_empty() {
        warn!("command palette: no actions available");
        return Ok(EXIT_OK);
    }
    let options = entries
        .iter()
        .enumerate()
        .map(|(index, entry)| {
            let option = PromptOption::new(index.to_string(), entry.label.clone())
                .detail(entry.detail.clone());
            if let Some(key_hint) = &entry.key_hint {
                option.key_hint(key_hint.clone())
            } else {
                option
            }
        })
        .collect::<Vec<_>>();
    let request = PromptRequest::search_select("Command Palette", options)
        .message("Type to fuzzy-search actions and commands")
        .modal_id("command-palette")
        .owner_plugin_id("bmux.command_palette")
        .width_range(64, 120)
        .search_placeholder("Search commands");
    let response = prompt::submit(request).map_err(|error| {
        PluginCommandError::unavailable(format!("command palette prompt unavailable: {error}"))
    })?;
    let handle = tokio::runtime::Handle::try_current().map_err(|_| {
        PluginCommandError::unavailable(
            "no tokio runtime available — command palette requires the attach runtime",
        )
    })?;
    handle.spawn(handle_command_palette_response(entries, response));
    Ok(EXIT_OK)
}

#[derive(Debug, Clone)]
enum PaletteEntryKind {
    BindableAction {
        action: String,
        argument: Option<BindableActionArgument>,
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
    key_hint: Option<String>,
    kind: PaletteEntryKind,
}

async fn handle_command_palette_response(
    entries: Vec<PaletteEntry>,
    response: tokio::sync::oneshot::Receiver<PromptResponse>,
) {
    let selected = match response.await {
        Ok(PromptResponse::Submitted(PromptValue::Single(value))) => value,
        Ok(
            PromptResponse::Cancelled | PromptResponse::RejectedBusy | PromptResponse::Submitted(_),
        )
        | Err(_) => return,
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
        PaletteEntryKind::BindableAction { action, argument } => {
            if let Some(argument) = argument {
                let Some(value) = prompt_for_bindable_action_argument(&action, argument).await
                else {
                    return;
                };
                dispatch_action(&format!("{action} {value}"));
            } else {
                dispatch_action(&action);
            }
        }
        PaletteEntryKind::PluginCommand {
            plugin_id,
            command_name,
            schema,
        } => {
            let args = if uses_default_palette_arguments(&plugin_id, &command_name) {
                Vec::new()
            } else {
                let Some(args) = collect_command_arguments(&schema).await else {
                    return;
                };
                args
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
    let mut entries = bindable_action_entries(&context.active_keybindings);
    entries.extend(plugin_command_entries(
        &context.registered_plugins,
        &context.enabled_plugins,
        &context.active_keybindings,
    ));
    entries.sort_by(|left, right| {
        left.label
            .cmp(&right.label)
            .then_with(|| left.detail.cmp(&right.detail))
    });
    entries
}

fn bindable_action_entries(active_keybindings: &[ActiveKeybinding]) -> Vec<PaletteEntry> {
    bindable_action_catalog()
        .into_iter()
        .map(|action| PaletteEntry {
            label: action.label.to_string(),
            detail: action.detail.to_string(),
            key_hint: bindable_action_key_hint(active_keybindings, action.action, action.argument),
            kind: PaletteEntryKind::BindableAction {
                action: action.action.to_string(),
                argument: action.argument,
            },
        })
        .collect()
}

fn plugin_command_entries(
    plugins: &[RegisteredPluginInfo],
    enabled_plugins: &[String],
    active_keybindings: &[ActiveKeybinding],
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
                key_hint: plugin_command_key_hint(active_keybindings, &plugin.id, &command.name),
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

fn bindable_action_key_hint(
    active_keybindings: &[ActiveKeybinding],
    action: &str,
    argument: Option<BindableActionArgument>,
) -> Option<String> {
    if argument.is_some() {
        return key_hint_for_actions(active_keybindings, |active_action| {
            active_action == action
                || active_action
                    .strip_prefix(action)
                    .is_some_and(|suffix| suffix.starts_with(' '))
        });
    }
    let canonical = parse_action(action)
        .ok()
        .map(|runtime_action| action_to_config_name(&runtime_action))?;
    key_hint_for_actions(active_keybindings, |active_action| {
        active_action == canonical
    })
}

fn plugin_command_key_hint(
    active_keybindings: &[ActiveKeybinding],
    plugin_id: &str,
    command_name: &str,
) -> Option<String> {
    key_hint_for_actions(active_keybindings, |active_action| {
        matches!(
            parse_action(active_action),
            Ok(RuntimeAction::PluginCommand {
                plugin_id: active_plugin_id,
                command_name: active_command_name,
                ..
            }) if active_plugin_id == plugin_id && active_command_name == command_name
        )
    })
}

fn key_hint_for_actions(
    active_keybindings: &[ActiveKeybinding],
    matches_action: impl Fn(&str) -> bool,
) -> Option<String> {
    let chords = active_keybindings
        .iter()
        .filter(|binding| matches_action(&binding.action))
        .map(|binding| binding.chord.as_str())
        .collect::<std::collections::BTreeSet<_>>();
    if chords.is_empty() {
        None
    } else {
        Some(chords.into_iter().collect::<Vec<_>>().join(", "))
    }
}

async fn prompt_for_bindable_action_argument(
    action: &str,
    argument: BindableActionArgument,
) -> Option<String> {
    let response = prompt::request(
        PromptRequest::text_input(format!("{action}: {}", argument.label))
            .message(format!(
                "Required keybinding action argument: {}",
                argument.name
            ))
            .input_placeholder(argument.placeholder)
            .input_required(true)
            .input_validation(PromptValidation::NonEmpty)
            .modal_id("command-palette-action-argument"),
    )
    .await
    .ok()?;
    match response {
        PromptResponse::Submitted(PromptValue::Text(value)) => Some(value.trim().to_string()),
        PromptResponse::Cancelled | PromptResponse::RejectedBusy | PromptResponse::Submitted(_) => {
            None
        }
    }
}

fn uses_default_palette_arguments(plugin_id: &str, command_name: &str) -> bool {
    plugin_id == "bmux.pane_runtime" && command_name == "pane-padding-configure"
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
    use super::{
        PaletteEntry, PaletteEntryKind, bindable_action_key_hint, collect_command_arguments,
        plugin_action_string, plugin_command_key_hint, shell_quote, uses_default_palette_arguments,
    };
    use bmux_plugin_sdk::{ActiveKeybinding, PluginCommand, PromptOption};

    #[test]
    fn pane_padding_configurator_uses_default_palette_arguments() {
        assert!(uses_default_palette_arguments(
            "bmux.pane_runtime",
            "pane-padding-configure"
        ));
        assert!(!uses_default_palette_arguments(
            "bmux.pane_runtime",
            "pane-padding-set"
        ));
    }

    #[tokio::test]
    async fn argumentless_plugin_command_collects_no_arguments() {
        let command = PluginCommand::new("configure", "Open configurator");

        assert_eq!(collect_command_arguments(&command).await, Some(Vec::new()));
    }

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

    #[test]
    fn prompt_option_preserves_palette_metadata() {
        let entry = PaletteEntry {
            label: "Quit".to_string(),
            detail: "keybind: quit".to_string(),
            key_hint: Some("Ctrl-A q".to_string()),
            kind: PaletteEntryKind::BindableAction {
                action: "quit".to_string(),
                argument: None,
            },
        };
        let option = PromptOption::new("0", entry.label.clone())
            .detail(entry.detail.clone())
            .key_hint(entry.key_hint.expect("key hint"));

        assert_eq!(option.label, "Quit");
        assert_eq!(option.detail.as_deref(), Some("keybind: quit"));
        assert_eq!(option.key_hint.as_deref(), Some("Ctrl-A q"));
    }

    #[test]
    fn bindable_action_key_hint_matches_canonical_static_action() {
        let active = vec![ActiveKeybinding {
            scope: "normal".to_string(),
            chord: "Ctrl-A q".to_string(),
            action: "quit".to_string(),
        }];

        assert_eq!(
            bindable_action_key_hint(&active, "quit_destroy", None),
            Some("Ctrl-A q".to_string())
        );
    }

    #[test]
    fn bindable_action_key_hint_matches_parameterized_action_prefix() {
        let active = vec![ActiveKeybinding {
            scope: "normal".to_string(),
            chord: "i".to_string(),
            action: "enter_mode insert".to_string(),
        }];

        assert_eq!(
            bindable_action_key_hint(
                &active,
                "enter_mode",
                Some(bmux_keybind::BindableActionArgument {
                    name: "mode",
                    label: "Mode",
                    placeholder: "normal",
                })
            ),
            Some("i".to_string())
        );
    }

    #[test]
    fn plugin_command_key_hint_matches_command_binding() {
        let active = vec![ActiveKeybinding {
            scope: "normal".to_string(),
            chord: "Ctrl-A %".to_string(),
            action: "plugin:bmux.windows:split-pane --direction vertical".to_string(),
        }];

        assert_eq!(
            plugin_command_key_hint(&active, "bmux.windows", "split-pane"),
            Some("Ctrl-A %".to_string())
        );
    }
}
