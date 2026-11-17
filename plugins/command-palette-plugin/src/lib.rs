#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

/// Plugin declaration generated from the BPDL world and `plugin.toml`.
pub mod declaration {
    bmux_plugin_schema_macros::schema! {
        source: "bpdl/command-palette-plugin.bpdl",
        manifest: "plugin.toml",
    }
}

use bmux_keybind::{
    BindableActionArgument, RuntimeAction, action_to_config_name, bindable_action_catalog,
    parse_action,
};
mod activation_ports;

use bmux_plugin::{action_dispatch, prompt};
use bmux_plugin_sdk::author::{self, Activation, Call, Deactivation, Declarations, Plugin};
use bmux_plugin_sdk::prelude::*;
use bmux_plugin_sdk::prompt::PromptOrderedMatchMode;
use bmux_plugin_sdk::{
    ActiveKeybinding, PluginCommand, PluginCommandArgument, PluginCommandArgumentKind,
    PromptFormField, PromptFormSection, PromptFormValue, PromptOption, PromptRequest,
    PromptResponse, PromptSearchMatchMode, PromptValidation, PromptValue, RegisteredPluginInfo,
};
use tracing::{debug, warn};

#[derive(Default)]
pub struct CommandPalettePlugin {
    binding: activation_ports::Binding,
    task: std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl Drop for CommandPalettePlugin {
    fn drop(&mut self) {
        if let Ok(task) = self.task.get_mut()
            && let Some(task) = task.take()
        {
            task.abort();
        }
    }
}

impl CommandPalettePlugin {
    /// Supply ports for one subsequent scoped activation; no channels are live
    /// until `PluginInstance::activate` binds that activation.
    #[must_use]
    pub fn with_ports(
        prompt: prompt::PromptPort,
        actions: action_dispatch::ActionDispatchPort,
    ) -> Self {
        Self {
            binding: activation_ports::Binding::fixed(prompt, actions),
            task: std::sync::Mutex::default(),
        }
    }

    fn show(&self, context: &NativeCommandContext) -> Result<i32, PluginCommandError> {
        let (prompt, actions) = self.binding.ports()?;
        show_command_palette(context, &self.task, prompt, actions)
    }
}

fn show_command_palette(
    context: &NativeCommandContext,
    task: &std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
    prompt: prompt::PromptPort,
    actions: action_dispatch::ActionDispatchPort,
) -> Result<i32, PluginCommandError> {
    let handle = tokio::runtime::Handle::try_current().map_err(|_| {
        PluginCommandError::unavailable("command palette requires a frontend async runtime")
    })?;
    let entries = build_palette_entries(context);
    if entries.is_empty() {
        warn!("command palette: no actions available");
        return Ok(EXIT_OK);
    }
    let options = entries
        .iter()
        .enumerate()
        .map(|(index, entry)| palette_option(index, entry))
        .collect::<Vec<_>>();
    let request = PromptRequest::search_select("Command Palette", options)
        .message("Type to fuzzy-search actions and commands")
        .modal_id("command-palette")
        .owner_plugin_id("bmux.command_palette")
        .width_range(64, 120)
        .search_match_mode(PromptSearchMatchMode::OrderedV1 {
            matching: PromptOrderedMatchMode::Fuzzy,
            relevance: true,
        })
        .search_placeholder("Search commands");
    let mut task = task.lock().map_err(|_| {
        PluginCommandError::unavailable("command palette task owner requires recovery")
    })?;
    if task.as_ref().is_some_and(|task| !task.is_finished()) {
        return Err(PluginCommandError::unavailable(
            "command palette is already open",
        ));
    }
    let response = prompt.submit(request).map_err(|error| {
        PluginCommandError::unavailable(format!("command palette prompt unavailable: {error}"))
    })?;
    *task = Some(handle.spawn(handle_command_palette_response(
        entries, response, prompt, actions,
    )));
    drop(task);
    Ok(EXIT_OK)
}

fn palette_option(index: usize, entry: &PaletteEntry) -> PromptOption {
    let (primary, identifier) = match &entry.kind {
        PaletteEntryKind::BindableAction { action, .. } => (entry.label.as_str(), action.clone()),
        PaletteEntryKind::PluginCommand {
            plugin_id,
            command_name,
            schema,
        } => (
            if schema.summary.is_empty() {
                command_name.as_str()
            } else {
                schema.summary.as_str()
            },
            format!("plugin:{plugin_id}:{command_name}"),
        ),
    };
    let mut option = PromptOption::new(index.to_string(), entry.label.clone())
        .detail(entry.detail.clone())
        .search_text(format!("{} {identifier}", entry.label));
    option.search_primary = Some(primary.to_string());
    option.key_hint.clone_from(&entry.key_hint);
    option
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
    prompt: prompt::PromptPort,
    actions: action_dispatch::ActionDispatchPort,
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
                let Some(value) =
                    prompt_for_bindable_action_argument(&prompt, &action, argument).await
                else {
                    return;
                };
                dispatch_action(&actions, &format!("{action} {value}"));
            } else {
                dispatch_action(&actions, &action);
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
                let Some(args) = collect_command_arguments(&prompt, &schema).await else {
                    return;
                };
                args
            };
            let action = plugin_action_string(&plugin_id, &command_name, &args);
            dispatch_action(&actions, &action);
        }
    }
}

fn dispatch_action(actions: &action_dispatch::ActionDispatchPort, action: &str) {
    debug!(%action, "command palette: dispatching action");
    if let Err(error) = actions.dispatch(action) {
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
    prompt: &prompt::PromptPort,
    action: &str,
    argument: BindableActionArgument,
) -> Option<String> {
    let response = prompt
        .request(
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

/// Collect every argument of `command` with one form.
async fn collect_command_arguments(
    prompt: &prompt::PromptPort,
    command: &PluginCommand,
) -> Option<Vec<String>> {
    if command.arguments.is_empty() {
        return Some(Vec::new());
    }
    match prompt.request(argument_form(command)).await.ok()? {
        PromptResponse::Submitted(PromptValue::Form(values)) => {
            Some(arguments_from_form(command, &values))
        }
        PromptResponse::Cancelled | PromptResponse::RejectedBusy | PromptResponse::Submitted(_) => {
            None
        }
    }
}

fn argument_field_id(index: usize) -> String {
    format!("arg{index}")
}

fn argument_field(index: usize, argument: &PluginCommandArgument) -> PromptFormField {
    let id = argument_field_id(index);
    let label = argument_display_name(argument);
    let mut field = match argument.kind {
        PluginCommandArgumentKind::Boolean => PromptFormField::toggle(id, label, false),
        PluginCommandArgumentKind::Choice => {
            let none = (!argument.required).then(|| PromptOption::new(String::new(), "none"));
            let options = none
                .into_iter()
                .chain(
                    argument
                        .choice_values
                        .iter()
                        .map(|value| PromptOption::new(value.clone(), value.clone())),
                )
                .collect();
            PromptFormField::choice(id, label, options, 0)
        }
        PluginCommandArgumentKind::Integer
        | PluginCommandArgumentKind::String
        | PluginCommandArgumentKind::Path => {
            let placeholder = if argument.multiple || argument.trailing_var_arg {
                "space-separated values".to_owned()
            } else {
                argument.value_name.clone().unwrap_or_else(|| {
                    match argument.kind {
                        PluginCommandArgumentKind::Integer => "integer",
                        PluginCommandArgumentKind::Path => "path",
                        _ => "value",
                    }
                    .to_owned()
                })
            };
            let field = PromptFormField::text(id, label, "").placeholder(placeholder);
            if matches!(argument.kind, PluginCommandArgumentKind::Integer)
                && !argument.multiple
                && !argument.trailing_var_arg
            {
                field.validation(PromptValidation::Integer)
            } else {
                field
            }
        }
    }
    .required(argument.required && !matches!(argument.kind, PluginCommandArgumentKind::Boolean));
    if let Some(summary) = &argument.summary {
        field = field.description(summary.as_str());
    }
    field
}

/// One form with a field per command argument, in declaration order.
fn argument_form(command: &PluginCommand) -> PromptRequest {
    let fields = command
        .arguments
        .iter()
        .enumerate()
        .map(|(index, argument)| argument_field(index, argument))
        .collect();
    let mut request = PromptRequest::form(
        command.name.clone(),
        vec![PromptFormSection::new("arguments", "Arguments", fields)],
    )
    .submit_label("Run")
    .modal_id("command-palette-argument");
    if !command.summary.trim().is_empty() {
        request = request.message(command.summary.clone());
    }
    request
}

fn arguments_from_form(
    command: &PluginCommand,
    values: &std::collections::BTreeMap<String, PromptFormValue>,
) -> Vec<String> {
    let mut args = Vec::new();
    for (index, argument) in command.arguments.iter().enumerate() {
        let collected = match values.get(&argument_field_id(index)) {
            Some(PromptFormValue::Bool(true)) => vec!["true".to_owned()],
            Some(PromptFormValue::Single(value)) if !value.is_empty() => vec![value.clone()],
            Some(PromptFormValue::Text(value)) => {
                let value = value.trim();
                if value.is_empty() {
                    Vec::new()
                } else if argument.multiple || argument.trailing_var_arg {
                    value.split_whitespace().map(ToString::to_string).collect()
                } else {
                    vec![value.to_owned()]
                }
            }
            _ => Vec::new(),
        };
        append_argument_values(&mut args, argument, collected);
    }
    args
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

impl Plugin for CommandPalettePlugin {
    fn declare(d: &mut Declarations<Self>) {
        d.native_command("show", Self::command);
    }

    async fn activate(&self, cx: &mut Activation) -> author::Result<()> {
        self.binding.activate(cx.lifecycle())
    }

    async fn deactivate(&self, cx: &mut Deactivation) -> author::Result<()> {
        self.binding.close(cx.lifecycle())?;
        let task = self
            .task
            .lock()
            .map_err(|_| {
                PluginCommandError::unavailable("command palette task owner requires recovery")
            })?
            .take();
        if let Some(task) = task {
            task.abort();
            let _ = task.await;
        }
        self.binding.drained(cx.lifecycle())
    }
}

impl CommandPalettePlugin {
    async fn command(&self, cx: &Call, context: NativeCommandContext) -> author::Result<i32> {
        if cx
            .cancellation()
            .is_some_and(bmux_plugin_sdk::ServiceCancellation::is_cancelled)
        {
            return Err(PluginCommandError::unavailable(
                "command palette invocation cancelled",
            ));
        }
        let entry = self.binding.enter(
            cx.invocation()
                .and_then(|call| call.callback_execution.as_ref()),
        )?;
        let result = self.show(&context);
        drop(entry);
        result
    }
}

/// Define a frontend whose channels bind at activation time.
///
/// The trusted host must create both ports from the activation belonging to
/// this lifecycle context, retain its inboxes, and never select global channels.
#[must_use]
pub fn plugin_definition_with_activation_ports(
    bind: impl Fn(
        &bmux_plugin_sdk::definition::PluginLifecycleContext,
    ) -> Result<
        (
            bmux_plugin::prompt::PromptPort,
            bmux_plugin::action_dispatch::ActionDispatchPort,
        ),
        PluginCommandError,
    > + Send
    + Sync
    + 'static,
) -> bmux_plugin_sdk::definition::PluginDefinition {
    let bind: activation_ports::Binder = std::sync::Arc::new(bind);
    bmux_plugin_sdk::author::definition::<CommandPalettePlugin>(
        crate::declaration::PLUGIN_MANIFEST,
        crate::declaration::SERVICE_CONTRACTS,
        move |scope| {
            Ok(CommandPalettePlugin {
                binding: activation_ports::Binding::deferred(scope.clone(), bind.clone()),
                task: std::sync::Mutex::default(),
            })
        },
    )
    .expect("embedded plugin declaration must be valid")
}

/// Retain compatibility with the create-context injector signature.
///
/// Callers must supply the exact activation's channels. The injector now runs
/// at activation, never at creation.
#[must_use]
pub fn plugin_definition_with_ports(
    inject: impl Fn(
        bmux_plugin_sdk::definition::PluginCreateContext,
    ) -> Result<
        (
            bmux_plugin::prompt::PromptPort,
            bmux_plugin::action_dispatch::ActionDispatchPort,
        ),
        PluginCommandError,
    > + Send
    + Sync
    + 'static,
) -> bmux_plugin_sdk::definition::PluginDefinition {
    plugin_definition_with_activation_ports(move |context| inject(context.scope.clone()))
}

bmux_plugin_sdk::plugin!(
    CommandPalettePlugin,
    create = |scope| Ok(CommandPalettePlugin {
        binding: activation_ports::Binding::from_activation(scope.clone()),
        task: std::sync::Mutex::default(),
    })
);

#[cfg(test)]
mod tests {
    use super::{
        PaletteEntry, PaletteEntryKind, argument_form, arguments_from_form,
        bindable_action_key_hint, collect_command_arguments, plugin_action_string,
        plugin_command_entries, plugin_command_key_hint, shell_quote,
        uses_default_palette_arguments,
    };
    use bmux_plugin_sdk::{ActiveKeybinding, PluginCommand, PromptOption};

    #[test]
    fn embedded_declaration_preserves_frontend_only_identity() {
        let definition = super::plugin_definition();
        let manifest = bmux_plugin::PluginManifest::from_toml_str(
            definition
                .metadata
                .manifest_toml
                .as_deref()
                .expect("embedded declaration"),
        )
        .expect("valid embedded declaration");
        assert_eq!(
            manifest.component_roles,
            vec![bmux_plugin_sdk::definition::ComponentRole::Frontend]
        );
        assert_eq!(definition.metadata.roles, manifest.component_roles);
        assert_eq!(definition.metadata.contracts, manifest.contracts);
        assert_eq!(definition.metadata.plugin_id, manifest.id);
        assert_eq!(definition.metadata.version, manifest.version);
    }

    #[test]
    fn palette_searches_primary_names_and_identifiers_not_shortcuts() {
        let mut entry = PaletteEntry {
            label: "Output: Pause output".into(),
            detail: "bmux.output:pause".into(),
            key_hint: Some("Ctrl+P".into()),
            kind: PaletteEntryKind::PluginCommand {
                plugin_id: "bmux.output".into(),
                command_name: "pause".into(),
                schema: PluginCommand::new("pause", "Pause output"),
            },
        };
        let option = super::palette_option(7, &entry);
        assert_eq!(option.value, "7");
        assert_eq!(option.search_primary.as_deref(), Some("Pause output"));
        assert_eq!(
            option.search_text.as_deref(),
            Some("Output: Pause output plugin:bmux.output:pause")
        );
        assert_eq!(option.key_hint.as_deref(), Some("Ctrl+P"));
        if let PaletteEntryKind::PluginCommand { schema, .. } = &mut entry.kind {
            schema.summary.clear();
        }
        assert_eq!(
            super::palette_option(0, &entry).search_primary.as_deref(),
            Some("pause")
        );
        entry.kind = PaletteEntryKind::BindableAction {
            action: "pause-output".into(),
            argument: None,
        };
        assert_eq!(
            super::palette_option(0, &entry).search_text.as_deref(),
            Some("Output: Pause output pause-output")
        );
    }

    #[test]
    fn named_broadcast_commands_are_discovered_only_when_enabled() {
        let names = ["start", "pause", "resume", "stop", "setup", "doctor"];
        let plugin = bmux_plugin_sdk::RegisteredPluginInfo {
            id: "bmux.broadcast".into(),
            display_name: "Broadcast".into(),
            version: "1".into(),
            bundled_static: false,
            required_capabilities: Vec::new(),
            provided_capabilities: Vec::new(),
            commands: names.iter().map(ToString::to_string).collect(),
            command_schemas: names
                .iter()
                .map(|name| PluginCommand::new(*name, format!("Broadcast {name}")))
                .collect(),
        };
        assert!(plugin_command_entries(std::slice::from_ref(&plugin), &[], &[]).is_empty());
        assert!(plugin_command_entries(&[], std::slice::from_ref(&plugin.id), &[]).is_empty());
        let entries = plugin_command_entries(
            std::slice::from_ref(&plugin),
            std::slice::from_ref(&plugin.id),
            &[],
        );
        assert_eq!(entries.len(), names.len());
        for (entry, name) in entries.iter().zip(names) {
            let PaletteEntryKind::PluginCommand {
                plugin_id,
                command_name,
                schema,
            } = &entry.kind
            else {
                panic!("expected normal plugin command path");
            };
            assert_eq!(command_name, name);
            assert_eq!(
                schema.arguments,
                [] as [bmux_plugin_sdk::PluginCommandArgument; 0]
            );
            assert_eq!(
                plugin_action_string(plugin_id, command_name, &[]),
                format!("plugin:bmux.broadcast:{name}")
            );
        }
    }

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

        let instance = bmux_plugin_runtime::InstanceScope::new(
            bmux_plugin_runtime::PluginRuntimeScopeId::default(),
        );
        let owner = instance
            .begin_activation(bmux_plugin_runtime::PluginConcurrencyConfig::Concurrent)
            .unwrap();
        owner.mark_active().unwrap();
        let (prompt, _inbox) = bmux_plugin::prompt::PromptPort::channel(owner, 1).unwrap();
        assert_eq!(
            collect_command_arguments(&prompt, &command).await,
            Some(Vec::new())
        );
    }

    fn sample_command() -> PluginCommand {
        use bmux_plugin_sdk::{PluginCommandArgument as Arg, PluginCommandArgumentKind as Kind};
        let mut command = PluginCommand::new("recording-cut", "Export part of a recording");
        command.arguments = vec![
            Arg::option("last-seconds", Kind::Integer)
                .required(true)
                .summary("Seconds to keep"),
            Arg::option("format", Kind::Choice).choice_values(["gif", "mp4"]),
            Arg::flag("open"),
            Arg::positional("tags", Kind::String).multiple(true),
        ];
        command
    }

    #[test]
    fn command_arguments_become_one_form() {
        use bmux_plugin_sdk::{PromptField, PromptFormFieldKind as Kind};
        let request = argument_form(&sample_command());
        assert_eq!(request.title, "recording-cut");
        assert_eq!(
            request.message.as_deref(),
            Some("Export part of a recording")
        );
        let PromptField::Form { sections, .. } = &request.field else {
            panic!("expected a form");
        };
        let fields = &sections[0].fields;
        assert_eq!(fields.len(), 4);
        assert_eq!(fields[0].label, "--last-seconds");
        assert!(fields[0].required);
        assert_eq!(fields[0].description.as_deref(), Some("Seconds to keep"));
        assert!(matches!(
            fields[0].kind,
            Kind::Text {
                validation: Some(bmux_plugin_sdk::PromptValidation::Integer),
                ..
            }
        ));
        assert!(matches!(
            &fields[1].kind,
            Kind::SingleSelect { options, default_index: 0 }
                if options.len() == 3 && options[0].value.is_empty()
        ));
        assert!(matches!(fields[2].kind, Kind::Bool { default: false }));
        assert_eq!(fields[3].label, "tags");
    }

    #[test]
    fn submitted_form_values_become_command_arguments() {
        use bmux_plugin_sdk::PromptFormValue as Value;
        let command = sample_command();
        let values = std::collections::BTreeMap::from([
            ("arg0".to_owned(), Value::Text(" 30 ".into())),
            ("arg1".to_owned(), Value::Single(String::new())),
            ("arg2".to_owned(), Value::Bool(true)),
            ("arg3".to_owned(), Value::Text("a b".into())),
        ]);
        assert_eq!(
            arguments_from_form(&command, &values),
            ["--last-seconds", "30", "--open", "a", "b"]
        );
        let values = std::collections::BTreeMap::from([
            ("arg0".to_owned(), Value::Text("5".into())),
            ("arg1".to_owned(), Value::Single("gif".into())),
            ("arg2".to_owned(), Value::Bool(false)),
            ("arg3".to_owned(), Value::Text(String::new())),
        ]);
        assert_eq!(
            arguments_from_form(&command, &values),
            ["--last-seconds", "5", "--format", "gif"]
        );
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
            action: "plugin:bmux.tabs:split-pane --direction vertical".to_string(),
        }];

        assert_eq!(
            plugin_command_key_hint(&active, "bmux.tabs", "split-pane"),
            Some("Ctrl-A %".to_string())
        );
    }
}

#[cfg(test)]
mod embedded_declaration_identity_tests {
    #[test]
    fn definition_and_selected_manifest_have_one_identity() {
        let definition = super::plugin_definition();
        definition.validate().expect("definition validates");
        let embedded = definition
            .metadata
            .manifest_toml
            .as_deref()
            .expect("embedded declaration");
        let manifest =
            bmux_plugin::PluginManifest::from_toml_str(embedded).expect("valid manifest");
        assert_eq!(definition.metadata.plugin_id, manifest.id);
        assert_eq!(definition.metadata.version, manifest.version);
        assert_eq!(definition.metadata.roles, manifest.component_roles);
        assert_eq!(definition.metadata.contracts, manifest.contracts);
        assert_eq!(
            definition.metadata.required_capabilities,
            manifest
                .required_capabilities
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
        );

        // Deterministic verifier fixtures, not claims about built executable bytes.
        // Real selected artifact hashing remains the loader's responsibility.
        let expected_digest = "a".repeat(64);
        let wrong_digest = "b".repeat(64);
        let pinned = format!("artifact_sha256 = \"{expected_digest}\"\n{embedded}");
        let mut inventory = bmux_plugin::PluginCandidateInventory::new();
        inventory
            .add_bundled_manifest(&pinned)
            .expect("inventory declaration");
        for role in &manifest.component_roles {
            let selected = inventory
                .select(&bmux_plugin::PluginSelectionRequest {
                    plugin_id: manifest.id.clone(),
                    role: *role,
                    preference: bmux_plugin::PluginCandidatePreference::Bundled,
                    plugin_api: bmux_plugin_sdk::ApiVersion::new(1, 1),
                    native_abi: bmux_plugin_sdk::ApiVersion::new(1, 1),
                    contracts: manifest.contracts.clone(),
                })
                .expect("select declared role and contracts");
            selected
                .validate_embedded_metadata(&definition.metadata, &expected_digest)
                .expect("selected declaration and embedded identity match");
            assert!(
                selected
                    .validate_embedded_metadata(&definition.metadata, &wrong_digest)
                    .is_err()
            );
            let mut conflicting = definition.metadata.clone();
            conflicting.version.push_str("-conflict");
            assert!(
                selected
                    .validate_embedded_metadata(&conflicting, &expected_digest)
                    .is_err()
            );
        }
    }
}
