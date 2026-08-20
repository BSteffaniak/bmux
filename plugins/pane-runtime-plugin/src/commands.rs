use bmux_clients_plugin_api::clients_state;
use bmux_pane_runtime_plugin_api::{
    pane_runtime_commands,
    pane_runtime_state::{self, PanePaddingState},
};
use bmux_plugin::{ServiceCallerDispatchClient, prompt};
use bmux_plugin_sdk::{
    COMMAND_OUTCOME_STATUS_MESSAGE_KEY, NativeCommandContext, NativeCommandInvocationSource,
    PluginCommandError, PromptEvent, PromptFormField, PromptFormFieldKind, PromptFormSection,
    PromptFormValue, PromptOption, PromptPolicy, PromptRequest, PromptResponse, PromptValue,
    record_command_outcome_metadata,
};
use bmux_sessions_plugin_api::sessions_state;
use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use uuid::Uuid;

#[cfg(test)]
thread_local! {
    static TEST_ATOMIC_TEMP_PATH: std::cell::RefCell<Option<PathBuf>> = const { std::cell::RefCell::new(None) };
}

use crate::padding::{HorizontalAlignment, PanePaddingSpec, VerticalAlignment};
use crate::runtime::PanePaddingRuntimeHandle;
use bmux_session_models::{ClientId, SessionId};

fn option_value(arguments: &[String], name: &str) -> Option<String> {
    let long = format!("--{name}");
    arguments.iter().enumerate().find_map(|(index, value)| {
        value
            .strip_prefix(&format!("{long}="))
            .map(ToOwned::to_owned)
            .or_else(|| {
                (value == &long)
                    .then(|| arguments.get(index + 1).cloned())
                    .flatten()
            })
    })
}

fn parse_u16(arguments: &[String], name: &str) -> Result<Option<u16>, PluginCommandError> {
    option_value(arguments, name)
        .map(|value| {
            value.parse::<u16>().map_err(|_| {
                PluginCommandError::invalid_arguments(format!("--{name} expects 0..65535"))
            })
        })
        .transpose()
}

enum LimitUpdate {
    Unchanged,
    Clear,
    Set(u16),
}

fn parse_limit(arguments: &[String], name: &str) -> Result<LimitUpdate, PluginCommandError> {
    let Some(value) = option_value(arguments, name) else {
        return Ok(LimitUpdate::Unchanged);
    };
    if value == "none" {
        return Ok(LimitUpdate::Clear);
    }
    let parsed = value.parse::<u16>().map_err(|_| {
        PluginCommandError::invalid_arguments(format!(
            "--{name} expects a positive integer or none"
        ))
    })?;
    if parsed == 0 {
        return Err(PluginCommandError::invalid_arguments(format!(
            "--{name} expects a positive integer or none"
        )));
    }
    Ok(LimitUpdate::Set(parsed))
}

fn target_pane(arguments: &[String]) -> Result<Option<Uuid>, PluginCommandError> {
    option_value(arguments, "pane-id")
        .map(|value| {
            Uuid::parse_str(&value)
                .map_err(|_| PluginCommandError::invalid_arguments("--pane-id expects a UUID"))
        })
        .transpose()
}

fn session_selector(
    arguments: &[String],
) -> Result<Option<sessions_state::SessionSelector>, PluginCommandError> {
    let Some(selector) = option_value(arguments, "session") else {
        return Ok(None);
    };
    let selector = selector.trim();
    if selector.is_empty() {
        return Err(PluginCommandError::invalid_arguments(
            "--session expects a session UUID or name",
        ));
    }
    let parsed_id = Uuid::parse_str(selector).ok();
    Ok(Some(sessions_state::SessionSelector {
        id: parsed_id,
        name: parsed_id.is_none().then(|| selector.to_string()),
    }))
}

fn selected_session(context: &NativeCommandContext) -> Result<SessionId, PluginCommandError> {
    let mut client = ServiceCallerDispatchClient::new(context);
    if let Some(selector) = session_selector(&context.arguments)? {
        let result = bmux_plugin::block_on_typed_dispatch(sessions_state::client::get_session(
            &mut client,
            selector,
        ))
        .map_err(|error| PluginCommandError::failed(format!("session lookup failed: {error}")))?
        .map_err(|error| {
            PluginCommandError::failed(format!("session selector did not resolve: {error:?}"))
        })?;
        return Ok(SessionId(result.id));
    }
    let selected =
        bmux_plugin::block_on_typed_dispatch(clients_state::client::current_client(&mut client))
            .ok()
            .and_then(Result::ok)
            .and_then(|current| current.selected_session_id)
            .map(SessionId);
    if let Some(session_id) = selected {
        return Ok(session_id);
    }
    if let Some(client_id) = context.caller_client_id
        && let Ok(session_id) = handle()?.session_for_client(ClientId(client_id))
    {
        return Ok(session_id);
    }
    Err(PluginCommandError::failed(
        "current client has no selected or attached session",
    ))
}

fn handle() -> Result<PanePaddingRuntimeHandle, PluginCommandError> {
    bmux_plugin::global_plugin_state_registry()
        .get::<PanePaddingRuntimeHandle>()
        .and_then(|entry| entry.read().ok().map(|guard| (*guard).clone()))
        .ok_or_else(|| PluginCommandError::unavailable("pane runtime unavailable"))
}

fn status_text(state: &PanePaddingState) -> String {
    format!(
        "pane padding: edges {}/{}/{}/{}, max {}x{}, align {}/{}",
        state.effective.left,
        state.effective.right,
        state.effective.top,
        state.effective.bottom,
        state
            .effective
            .max_content_width
            .map_or_else(|| "none".to_string(), |value| value.to_string()),
        state
            .effective
            .max_content_height
            .map_or_else(|| "none".to_string(), |value| value.to_string()),
        state.effective.horizontal_alignment,
        state.effective.vertical_alignment,
    )
}

fn emit_result(context: &NativeCommandContext, state: PanePaddingState) {
    let status = status_text(&state);
    record_command_outcome_metadata(COMMAND_OUTCOME_STATUS_MESSAGE_KEY, status.clone().into());
    if matches!(
        context.invocation_source,
        NativeCommandInvocationSource::Cli
    ) {
        println!("{status}");
        println!("session: {}", state.session_id);
        println!("pane: {}", state.pane_id);
        println!("source: {}", state.source);
        if let Some(rect) = state.effective_content_rect {
            println!("content: {}x{} at {},{}", rect.w, rect.h, rect.x, rect.y);
        }
    }
}

fn set_spec(
    arguments: &[String],
    mut spec: PanePaddingSpec,
) -> Result<PanePaddingSpec, PluginCommandError> {
    let mut changed = false;
    if let Some(value) = parse_u16(arguments, "all")? {
        spec.left = value;
        spec.right = value;
        spec.top = value;
        spec.bottom = value;
        changed = true;
    }
    if let Some(value) = parse_u16(arguments, "horizontal")? {
        spec.left = value;
        spec.right = value;
        changed = true;
    }
    if let Some(value) = parse_u16(arguments, "vertical")? {
        spec.top = value;
        spec.bottom = value;
        changed = true;
    }
    for (name, target) in [
        ("left", &mut spec.left),
        ("right", &mut spec.right),
        ("top", &mut spec.top),
        ("bottom", &mut spec.bottom),
    ] {
        if let Some(value) = parse_u16(arguments, name)? {
            *target = value;
            changed = true;
        }
    }
    match parse_limit(arguments, "max-content-width")? {
        LimitUpdate::Unchanged => {}
        LimitUpdate::Clear => {
            spec.max_content_width = None;
            changed = true;
        }
        LimitUpdate::Set(value) => {
            spec.max_content_width = Some(value);
            changed = true;
        }
    }
    match parse_limit(arguments, "max-content-height")? {
        LimitUpdate::Unchanged => {}
        LimitUpdate::Clear => {
            spec.max_content_height = None;
            changed = true;
        }
        LimitUpdate::Set(value) => {
            spec.max_content_height = Some(value);
            changed = true;
        }
    }
    if let Some(value) = option_value(arguments, "horizontal-alignment") {
        spec.horizontal_alignment =
            HorizontalAlignment::parse(&value).map_err(PluginCommandError::invalid_arguments)?;
        changed = true;
    }
    if let Some(value) = option_value(arguments, "vertical-alignment") {
        spec.vertical_alignment =
            VerticalAlignment::parse(&value).map_err(PluginCommandError::invalid_arguments)?;
        changed = true;
    }
    if !changed {
        return Err(PluginCommandError::invalid_arguments(
            "pane-padding set requires at least one padding option",
        ));
    }
    Ok(spec)
}

pub(crate) fn run(context: &NativeCommandContext) -> Result<(), PluginCommandError> {
    let session_id = selected_session(context)?;
    let pane_id = target_pane(&context.arguments)?;
    let handle = handle()?;
    match context.command.as_str() {
        "pane-padding-show" => {
            let state = handle
                .state(session_id, pane_id)
                .map_err(|error| PluginCommandError::failed(error.to_string()))?;
            emit_result(context, state.into_api());
        }
        "pane-padding-set" => {
            let current = handle
                .state(session_id, pane_id)
                .map_err(|error| PluginCommandError::failed(error.to_string()))?;
            let spec = set_spec(&context.arguments, current.effective)?;
            let state = handle
                .set_override(session_id, pane_id, Some(spec))
                .map_err(|error| PluginCommandError::failed(error.to_string()))?;
            emit_result(context, state.into_api());
        }
        "pane-padding-configure" => {
            configure(context)?;
        }
        "pane-padding-reset" => {
            let state = handle
                .set_override(session_id, pane_id, None)
                .map_err(|error| PluginCommandError::failed(error.to_string()))?;
            emit_result(context, state.into_api());
        }
        command => return Err(PluginCommandError::unknown_command(command)),
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ConfigureScope {
    Pane,
    Window,
    Session,
    All,
    Global,
}

impl ConfigureScope {
    fn parse(value: &str) -> Result<Self, PluginCommandError> {
        match value {
            "pane" => Ok(Self::Pane),
            "window" => Ok(Self::Window),
            "session" => Ok(Self::Session),
            "all" => Ok(Self::All),
            "global" => Ok(Self::Global),
            _ => Err(PluginCommandError::invalid_arguments(
                "scope must be pane, window, session, all, or global",
            )),
        }
    }
}

fn select_options(values: &[(&str, &str)]) -> Vec<PromptOption> {
    values
        .iter()
        .map(|(value, label)| PromptOption::new(*value, *label))
        .collect()
}

#[allow(clippy::too_many_lines)] // Keeping the fixed form schema together makes field defaults and sections reviewable.
fn configure_request(
    spec: PanePaddingSpec,
    windows_available: bool,
    target_summary: Option<String>,
) -> PromptRequest {
    let edge = |id, label, value| {
        PromptFormField::new(
            id,
            label,
            PromptFormFieldKind::Integer {
                initial_value: i64::from(value),
                min: Some(0),
                max: Some(i64::from(u16::MAX)),
            },
        )
    };
    let mut scope_options = vec![
        PromptOption::new("pane", "Current pane"),
        PromptOption::new("session", "Current session"),
        PromptOption::new("all", "All open panes"),
        PromptOption::new("global", "Global default"),
    ];
    if windows_available {
        scope_options.insert(1, PromptOption::new("window", "Current window"));
    }
    let mut request = PromptRequest::form(
        "Pane Padding",
        vec![
            PromptFormSection::new(
                "target",
                "Target",
                vec![
                    PromptFormField::new(
                        "scope",
                        "Scope",
                        PromptFormFieldKind::SingleSelect {
                            options: scope_options,
                            default_index: 0,
                        },
                    ),
                    PromptFormField::new(
                        "refresh-targets",
                        "Refresh target selection",
                        PromptFormFieldKind::Bool { default: false },
                    )
                    .description("Toggle to re-resolve the selected scope's open panes"),
                    PromptFormField::new(
                        "use-declarative",
                        "Use declarative settings on apply",
                        PromptFormFieldKind::Bool { default: false },
                    )
                    .description("Clear pane overrides instead of committing this draft"),
                    PromptFormField::new(
                        "lifetime",
                        "Lifetime",
                        PromptFormFieldKind::SingleSelect {
                            options: select_options(&[
                                ("runtime", "Runtime only"),
                                ("snapshot", "Restore with pane"),
                            ]),
                            default_index: 0,
                        },
                    ),
                ],
            ),
            PromptFormSection::new(
                "padding",
                "Padding",
                vec![
                    PromptFormField::new(
                        "preset",
                        "Preset",
                        PromptFormFieldKind::SingleSelect {
                            options: select_options(&[
                                ("custom", "Custom"),
                                ("none", "None"),
                                ("comfortable", "Comfortable"),
                                ("centered-120", "Centered 120"),
                                ("presentation", "Presentation"),
                            ]),
                            default_index: 0,
                        },
                    ),
                    PromptFormField::new(
                        "link-horizontal",
                        "Link left/right",
                        PromptFormFieldKind::Bool { default: false },
                    ),
                    PromptFormField::new(
                        "link-vertical",
                        "Link top/bottom",
                        PromptFormFieldKind::Bool { default: false },
                    ),
                    edge("left", "Left", spec.left),
                    edge("right", "Right", spec.right),
                    edge("top", "Top", spec.top),
                    edge("bottom", "Bottom", spec.bottom),
                    PromptFormField::new(
                        "limit-width",
                        "Limit content width",
                        PromptFormFieldKind::Bool {
                            default: spec.max_content_width.is_some(),
                        },
                    ),
                    PromptFormField::new(
                        "max-content-width",
                        "Maximum content width",
                        PromptFormFieldKind::Integer {
                            initial_value: i64::from(spec.max_content_width.unwrap_or(120)),
                            min: Some(1),
                            max: Some(i64::from(u16::MAX)),
                        },
                    ),
                    PromptFormField::new(
                        "limit-height",
                        "Limit content height",
                        PromptFormFieldKind::Bool {
                            default: spec.max_content_height.is_some(),
                        },
                    ),
                    PromptFormField::new(
                        "max-content-height",
                        "Maximum content height",
                        PromptFormFieldKind::Integer {
                            initial_value: i64::from(spec.max_content_height.unwrap_or(40)),
                            min: Some(1),
                            max: Some(i64::from(u16::MAX)),
                        },
                    ),
                    PromptFormField::new(
                        "horizontal-alignment",
                        "Horizontal alignment",
                        PromptFormFieldKind::SingleSelect {
                            options: select_options(&[
                                ("left", "Left"),
                                ("center", "Center"),
                                ("right", "Right"),
                            ]),
                            default_index: match spec.horizontal_alignment {
                                HorizontalAlignment::Left => 0,
                                HorizontalAlignment::Center => 1,
                                HorizontalAlignment::Right => 2,
                            },
                        },
                    ),
                    PromptFormField::new(
                        "vertical-alignment",
                        "Vertical alignment",
                        PromptFormFieldKind::SingleSelect {
                            options: select_options(&[
                                ("top", "Top"),
                                ("center", "Center"),
                                ("bottom", "Bottom"),
                            ]),
                            default_index: match spec.vertical_alignment {
                                VerticalAlignment::Top => 0,
                                VerticalAlignment::Center => 1,
                                VerticalAlignment::Bottom => 2,
                            },
                        },
                    ),
                ],
            ),
        ],
    );
    request = request
        .message("Changes preview live. Enter applies; Esc restores the previous geometry.")
        .policy(PromptPolicy::RejectIfBusy)
        .width_range(52, 88)
        .form_live_preview(true)
        .form_resettable(true)
        .form_paged_on_small(true);
    if let Some(summary) = target_summary {
        request = request.message(summary);
    }
    request
}

fn integer_value(values: &BTreeMap<String, PromptFormValue>, key: &str) -> Option<u16> {
    let PromptFormValue::Integer(value) = values.get(key)? else {
        return None;
    };
    u16::try_from(*value).ok()
}

fn bool_value(values: &BTreeMap<String, PromptFormValue>, key: &str) -> Option<bool> {
    let PromptFormValue::Bool(value) = values.get(key)? else {
        return None;
    };
    Some(*value)
}

fn single_value<'a>(values: &'a BTreeMap<String, PromptFormValue>, key: &str) -> Option<&'a str> {
    let PromptFormValue::Single(value) = values.get(key)? else {
        return None;
    };
    Some(value)
}

fn apply_preset(
    values: &BTreeMap<String, PromptFormValue>,
    spec: PanePaddingSpec,
) -> PanePaddingSpec {
    match single_value(values, "preset") {
        Some("none") => PanePaddingSpec {
            left: 0,
            right: 0,
            top: 0,
            bottom: 0,
            max_content_width: None,
            max_content_height: None,
            horizontal_alignment: HorizontalAlignment::Left,
            vertical_alignment: VerticalAlignment::Top,
        },
        Some("comfortable") => PanePaddingSpec {
            left: 1,
            right: 1,
            top: 1,
            bottom: 1,
            ..spec
        },
        Some("centered-120") => PanePaddingSpec {
            max_content_width: Some(120),
            horizontal_alignment: HorizontalAlignment::Center,
            ..spec
        },
        Some("presentation") => PanePaddingSpec {
            max_content_width: Some(100),
            max_content_height: Some(34),
            horizontal_alignment: HorizontalAlignment::Center,
            vertical_alignment: VerticalAlignment::Center,
            ..spec
        },
        _ => {
            // Preserve the current draft for the custom option.
            spec
        }
    }
}

fn spec_from_form(
    values: &BTreeMap<String, PromptFormValue>,
    spec: PanePaddingSpec,
) -> Result<PanePaddingSpec, PluginCommandError> {
    let mut spec = apply_preset(values, spec);
    if !matches!(single_value(values, "preset"), Some("custom") | None) {
        return Ok(spec);
    }
    for (key, target) in [
        ("left", &mut spec.left),
        ("right", &mut spec.right),
        ("top", &mut spec.top),
        ("bottom", &mut spec.bottom),
    ] {
        if let Some(value) = integer_value(values, key) {
            *target = value;
        }
    }
    if let Some(enabled) = bool_value(values, "limit-width") {
        spec.max_content_width = if enabled {
            Some(integer_value(values, "max-content-width").ok_or_else(|| {
                PluginCommandError::invalid_arguments("maximum content width must be 1..65535")
            })?)
        } else {
            None
        };
    }
    if let Some(enabled) = bool_value(values, "limit-height") {
        spec.max_content_height = if enabled {
            Some(integer_value(values, "max-content-height").ok_or_else(|| {
                PluginCommandError::invalid_arguments("maximum content height must be 1..65535")
            })?)
        } else {
            None
        };
    }
    if let Some(value) = single_value(values, "horizontal-alignment") {
        spec.horizontal_alignment =
            HorizontalAlignment::parse(value).map_err(PluginCommandError::invalid_arguments)?;
    }
    if let Some(value) = single_value(values, "vertical-alignment") {
        spec.vertical_alignment =
            VerticalAlignment::parse(value).map_err(PluginCommandError::invalid_arguments)?;
    }
    Ok(spec)
}

fn apply_linked_edge(
    values: &BTreeMap<String, PromptFormValue>,
    changed_field: &str,
    mut spec: PanePaddingSpec,
) -> PanePaddingSpec {
    if bool_value(values, "link-horizontal") == Some(true) {
        match changed_field {
            "left" => spec.right = spec.left,
            "right" => spec.left = spec.right,
            _ => {}
        }
    }
    if bool_value(values, "link-vertical") == Some(true) {
        match changed_field {
            "top" => spec.bottom = spec.top,
            "bottom" => spec.top = spec.bottom,
            _ => {}
        }
    }
    spec
}

fn non_empty_targets(targets: Vec<Uuid>) -> Result<Vec<Uuid>, PluginCommandError> {
    if targets.is_empty() {
        Err(PluginCommandError::failed(
            "selected pane-padding scope contains no open panes",
        ))
    } else {
        Ok(targets)
    }
}

fn resolve_scope(
    context: &NativeCommandContext,
    session_id: SessionId,
    pane_id: Uuid,
    scope: ConfigureScope,
) -> Result<Vec<Uuid>, PluginCommandError> {
    let handle = handle()?;
    match scope {
        ConfigureScope::Pane => non_empty_targets(vec![pane_id]),
        ConfigureScope::Session => handle
            .pane_ids(session_id)
            .map_err(|error| PluginCommandError::failed(error.to_string()))
            .and_then(non_empty_targets),
        ConfigureScope::All | ConfigureScope::Global => handle
            .all_pane_ids()
            .map_err(|error| PluginCommandError::failed(error.to_string()))
            .and_then(non_empty_targets),
        ConfigureScope::Window => {
            if !context
                .available_capabilities
                .iter()
                .any(|capability| capability == "bmux.windows.read")
            {
                return Err(PluginCommandError::unavailable(
                    "current-window scope requires the windows plugin",
                ));
            }
            let mut window_context = context.clone();
            if !window_context
                .required_capabilities
                .iter()
                .any(|capability| capability == "bmux.windows.read")
            {
                window_context
                    .required_capabilities
                    .push("bmux.windows.read".to_string());
            }
            let mut client = ServiceCallerDispatchClient::new(&window_context);
            let result = bmux_plugin::block_on_typed_dispatch(
                bmux_windows_plugin_api::windows_state::client::active_window_panes(&mut client),
            )
            .map_err(|error| {
                PluginCommandError::failed(format!("window target lookup failed: {error}"))
            })?
            .map_err(|error| {
                PluginCommandError::failed(format!("window target unavailable: {error:?}"))
            })?;
            non_empty_targets(result.pane_ids)
        }
    }
}

fn padding_settings_value(existing: Option<&toml::Value>, spec: PanePaddingSpec) -> toml::Value {
    let mut settings = existing
        .and_then(toml::Value::as_table)
        .cloned()
        .unwrap_or_default();
    let mut padding = settings
        .remove("padding")
        .and_then(|value| value.as_table().cloned())
        .unwrap_or_default();
    padding.insert(
        "left".to_string(),
        toml::Value::Integer(i64::from(spec.left)),
    );
    padding.insert(
        "right".to_string(),
        toml::Value::Integer(i64::from(spec.right)),
    );
    padding.insert("top".to_string(), toml::Value::Integer(i64::from(spec.top)));
    padding.insert(
        "bottom".to_string(),
        toml::Value::Integer(i64::from(spec.bottom)),
    );
    if let Some(value) = spec.max_content_width {
        padding.insert(
            "max_content_width".to_string(),
            toml::Value::Integer(i64::from(value)),
        );
    }
    if let Some(value) = spec.max_content_height {
        padding.insert(
            "max_content_height".to_string(),
            toml::Value::Integer(i64::from(value)),
        );
    }
    padding.insert(
        "horizontal_alignment".to_string(),
        toml::Value::String(spec.horizontal_alignment.as_str().to_string()),
    );
    padding.insert(
        "vertical_alignment".to_string(),
        toml::Value::String(spec.vertical_alignment.as_str().to_string()),
    );
    settings.insert("padding".to_string(), toml::Value::Table(padding));
    toml::Value::Table(settings)
}

fn set_padding_document(
    source: &str,
    spec: PanePaddingSpec,
) -> Result<toml_edit::DocumentMut, PluginCommandError> {
    let mut document = if source.trim().is_empty() {
        toml_edit::DocumentMut::new()
    } else {
        source.parse::<toml_edit::DocumentMut>().map_err(|error| {
            PluginCommandError::failed(format!("cannot parse bmux.toml: {error}"))
        })?
    };
    let padding = &mut document["plugins"]["settings"]["bmux.pane_runtime"]["padding"];
    padding["left"] = toml_edit::value(i64::from(spec.left));
    padding["right"] = toml_edit::value(i64::from(spec.right));
    padding["top"] = toml_edit::value(i64::from(spec.top));
    padding["bottom"] = toml_edit::value(i64::from(spec.bottom));
    if let Some(value) = spec.max_content_width {
        padding["max_content_width"] = toml_edit::value(i64::from(value));
    } else {
        padding
            .as_table_like_mut()
            .map(|table| table.remove("max_content_width"));
    }
    if let Some(value) = spec.max_content_height {
        padding["max_content_height"] = toml_edit::value(i64::from(value));
    } else {
        padding
            .as_table_like_mut()
            .map(|table| table.remove("max_content_height"));
    }
    padding["horizontal_alignment"] = toml_edit::value(spec.horizontal_alignment.as_str());
    padding["vertical_alignment"] = toml_edit::value(spec.vertical_alignment.as_str());
    Ok(document)
}

fn config_path(context: &NativeCommandContext) -> PathBuf {
    context
        .connection
        .probe_config_file("bmux.toml")
        .unwrap_or_else(|| PathBuf::from(&context.connection.config_dir).join("bmux.toml"))
}

fn atomic_replace(path: &Path, contents: &[u8]) -> Result<(), PluginCommandError> {
    let parent = path
        .parent()
        .ok_or_else(|| PluginCommandError::failed("invalid config path"))?;
    fs::create_dir_all(parent).map_err(|error| {
        PluginCommandError::failed(format!("cannot create config dir: {error}"))
    })?;
    #[cfg(test)]
    let temporary = TEST_ATOMIC_TEMP_PATH
        .with(|slot| slot.borrow_mut().take())
        .unwrap_or_else(|| parent.join(format!(".bmux.toml.{}.tmp", Uuid::new_v4())));
    #[cfg(not(test))]
    let temporary = parent.join(format!(".bmux.toml.{}.tmp", Uuid::new_v4()));
    let result = (|| {
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temporary)
            .map_err(|error| {
                PluginCommandError::failed(format!("cannot create config temp file: {error}"))
            })?;
        file.write_all(contents)
            .and_then(|()| file.sync_all())
            .map_err(|error| PluginCommandError::failed(format!("cannot flush config: {error}")))?;
        fs::rename(&temporary, path)
            .map_err(|error| PluginCommandError::failed(format!("cannot replace config: {error}")))
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn persist_and_install_global_padding(
    path: &Path,
    original: &[u8],
    edited: &[u8],
    install: impl FnOnce() -> Result<(), PluginCommandError>,
) -> Result<(), PluginCommandError> {
    atomic_replace(path, edited)?;
    if let Err(error) = install() {
        atomic_replace(path, original).map_err(|rollback| {
            PluginCommandError::failed(format!(
                "{error}; additionally failed restoring bmux.toml: {rollback}"
            ))
        })?;
        return Err(error);
    }
    Ok(())
}

fn verify_config_unchanged(path: &Path, original: &[u8]) -> Result<(), PluginCommandError> {
    if fs::read(path).unwrap_or_default() == original {
        Ok(())
    } else {
        Err(PluginCommandError::failed(
            "bmux.toml changed while pane padding was being applied",
        ))
    }
}

fn install_global_padding(
    context: &NativeCommandContext,
    spec: PanePaddingSpec,
) -> Result<(), PluginCommandError> {
    let settings = padding_settings_value(context.settings.as_ref(), spec);
    let config = crate::padding::PanePaddingConfig::parse(Some(&settings))
        .map_err(PluginCommandError::invalid_arguments)?;
    let path = config_path(context);
    let original = fs::read(&path).unwrap_or_default();
    let source = String::from_utf8(original.clone())
        .map_err(|_| PluginCommandError::failed("bmux.toml is not valid UTF-8"))?;
    let edited = set_padding_document(&source, spec)?
        .to_string()
        .into_bytes();
    verify_config_unchanged(&path, &original)?;
    persist_and_install_global_padding(&path, &original, &edited, || {
        handle()?.replace_padding_config(config).map_err(|error| {
            PluginCommandError::failed(format!("failed installing live pane padding: {error}"))
        })
    })
}

const NUMERIC_PREVIEW_DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(60);

fn numeric_preview_deadline(now: tokio::time::Instant) -> tokio::time::Instant {
    now + NUMERIC_PREVIEW_DEBOUNCE
}

fn is_numeric_configure_field(field_id: &str) -> bool {
    matches!(
        field_id,
        "left" | "right" | "top" | "bottom" | "max-content-width" | "max-content-height"
    )
}

fn clear_target_overrides(targets: &[Uuid]) -> Result<(), PluginCommandError> {
    handle()?
        .clear_overrides(targets)
        .map_err(|error| PluginCommandError::failed(error.to_string()))?;
    Ok(())
}

fn preview_feedback(states: &[crate::padding_api::RuntimePanePaddingState]) -> String {
    let Some(first) = states.first() else {
        return "Targets: 0 panes".to_string();
    };
    let mixed = states
        .iter()
        .skip(1)
        .any(|state| state.effective != first.effective);
    let constrained = states
        .iter()
        .filter(|state| state.effective_content_rect != state.base_content_rect)
        .count();
    let clamped = states
        .iter()
        .filter(|state| state.effective_content_rect.w <= 1 || state.effective_content_rect.h <= 1)
        .count();
    format!(
        "Targets: {} pane{} | {} constrained | {} unchanged | {} clamped | Values: {}",
        states.len(),
        if states.len() == 1 { "" } else { "s" },
        constrained,
        states.len().saturating_sub(constrained),
        clamped,
        if mixed { "mixed" } else { "uniform" },
    )
}

pub(crate) struct PreviewCancelGuard {
    handle: PanePaddingRuntimeHandle,
    owner: ClientId,
    token: Uuid,
    armed: bool,
}

impl PreviewCancelGuard {
    pub(crate) fn new(handle: PanePaddingRuntimeHandle, owner: ClientId, token: Uuid) -> Self {
        Self {
            handle,
            owner,
            token,
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for PreviewCancelGuard {
    fn drop(&mut self) {
        if self.armed {
            let _ = self.handle.cancel_preview(self.owner, self.token);
        }
    }
}

fn preview_spec(spec: PanePaddingSpec) -> pane_runtime_state::PanePaddingSpec {
    crate::padding_api::spec_to_api(spec)
}

#[allow(clippy::too_many_lines)] // The prompt transaction loop keeps submit, cancel, and live-update ownership visibly together.
fn configure(context: &NativeCommandContext) -> Result<(), PluginCommandError> {
    let session_id = selected_session(context)?;
    let explicit_pane = target_pane(&context.arguments)?;
    let current = handle()?
        .state(session_id, explicit_pane)
        .map_err(|error| PluginCommandError::failed(error.to_string()))?;
    let pane_id = current.pane_id;
    let initial_spec = current.effective;
    let windows_available = context
        .available_capabilities
        .iter()
        .any(|capability| capability == "bmux.windows.read");
    let target_summary = format!(
        "Targets: 1 pane | Outer: {}x{} | Base: {}x{} | Content/PTY: {}x{} | Source: {}",
        current.outer_rect.w,
        current.outer_rect.h,
        current.base_content_rect.w,
        current.base_content_rect.h,
        current.effective_content_rect.w,
        current.effective_content_rect.h,
        if current.runtime_override.is_some() {
            "runtime override"
        } else if current.matched_rule_index.is_some() {
            "rule"
        } else {
            "global"
        },
    );
    let prompt = configure_request(initial_spec, windows_available, Some(target_summary));
    let (mut response_rx, mut event_rx) = prompt::submit_with_events(prompt)
        .map_err(|error| PluginCommandError::unavailable(error.to_string()))?;
    let context = context.clone();
    tokio::spawn(async move {
        let mut scope = ConfigureScope::Pane;
        let mut spec = initial_spec;
        let Ok(initial_targets) = resolve_scope(&context, session_id, pane_id, scope) else {
            return;
        };
        let mut targets = initial_targets.clone();
        let mut client = ServiceCallerDispatchClient::new(&context);
        let Ok(Ok(started)) = bmux_plugin::block_on_typed_dispatch(
            pane_runtime_commands::client::begin_pane_padding_preview(
                &mut client,
                initial_targets,
                preview_spec(spec),
            ),
        ) else {
            return;
        };
        let token = started.token;
        record_command_outcome_metadata(
            COMMAND_OUTCOME_STATUS_MESSAGE_KEY,
            preview_feedback(
                &started
                    .panes
                    .into_iter()
                    .filter_map(|state| {
                        handle()
                            .ok()?
                            .state(SessionId(state.session_id), Some(state.pane_id))
                            .ok()
                    })
                    .collect::<Vec<_>>(),
            )
            .into(),
        );
        let owner = context
            .caller_client_id
            .map(ClientId)
            .expect("preview begin requires the caller client id");
        let mut cancel_guard =
            PreviewCancelGuard::new(handle().expect("runtime handle"), owner, token);
        let mut lifetime = pane_runtime_commands::PanePaddingPersistence::Runtime;
        let mut refresh_value = false;
        let mut pending_numeric: Option<(String, BTreeMap<String, PromptFormValue>)> = None;
        let numeric_debounce = tokio::time::sleep(std::time::Duration::MAX);
        tokio::pin!(numeric_debounce);
        loop {
            tokio::select! {
                response = &mut response_rx => {
                    match response {
                        Ok(PromptResponse::Submitted(PromptValue::Form(values))) => {
                            let final_spec = spec_from_form(&values, spec).unwrap_or(spec);
                            let final_scope = single_value(&values, "scope")
                                .and_then(|value| ConfigureScope::parse(value).ok())
                                .unwrap_or(scope);
                            let final_targets = resolve_scope(
                                &context,
                                session_id,
                                pane_id,
                                final_scope,
                            )
                            .unwrap_or_else(|_| targets.clone());
                            if final_targets != targets || final_spec != spec
                            {
                                let updated = bmux_plugin::block_on_typed_dispatch(
                                    pane_runtime_commands::client::update_pane_padding_preview(
                                        &mut client,
                                        token,
                                        final_targets.clone(),
                                        preview_spec(final_spec),
                                    ),
                                );
                                if !matches!(updated, Ok(Ok(_))) {
                                    let _ = bmux_plugin::block_on_typed_dispatch(
                                        pane_runtime_commands::client::cancel_pane_padding_preview(
                                            &mut client, token,
                                        ),
                                    );
                                    break;
                                }
                            }
                            if let Some(value) = single_value(&values, "lifetime") {
                                lifetime = if value == "snapshot" {
                                    pane_runtime_commands::PanePaddingPersistence::Snapshot
                                } else {
                                    pane_runtime_commands::PanePaddingPersistence::Runtime
                                };
                            }
                            let use_declarative =
                                bool_value(&values, "use-declarative") == Some(true);
                            if use_declarative {
                                let cancelled = bmux_plugin::block_on_typed_dispatch(
                                    pane_runtime_commands::client::cancel_pane_padding_preview(
                                        &mut client, token,
                                    ),
                                );
                                if matches!(cancelled, Ok(Ok(_))) {
                                    let _ = clear_target_overrides(&final_targets);
                                }
                            } else if final_scope == ConfigureScope::Global {
                                let cancelled = bmux_plugin::block_on_typed_dispatch(
                                    pane_runtime_commands::client::cancel_pane_padding_preview(
                                        &mut client, token,
                                    ),
                                );
                                if matches!(cancelled, Ok(Ok(_))) {
                                    let _ = install_global_padding(&context, final_spec);
                                }
                            } else {
                                let _ = bmux_plugin::block_on_typed_dispatch(
                                    pane_runtime_commands::client::commit_pane_padding_preview(
                                        &mut client, token, lifetime,
                                    ),
                                );
                            }
                        }
                        _ => {
                            let _ = bmux_plugin::block_on_typed_dispatch(
                                pane_runtime_commands::client::cancel_pane_padding_preview(
                                    &mut client, token,
                                ),
                            );
                        }
                    }
                    cancel_guard.disarm();
                    break;
                }
                () = &mut numeric_debounce, if pending_numeric.is_some() => {
                    let Some((field_id, values)) = pending_numeric.take() else {
                        continue;
                    };
                    let Ok(parsed_spec) = spec_from_form(&values, spec) else {
                        continue;
                    };
                    let next_spec = apply_linked_edge(&values, &field_id, parsed_spec);
                    let result = bmux_plugin::block_on_typed_dispatch(
                        pane_runtime_commands::client::update_pane_padding_preview(
                            &mut client, token, targets.clone(), preview_spec(next_spec),
                        ),
                    );
                    if matches!(result, Ok(Ok(_))) {
                        spec = next_spec;
                    }
                }
                event = event_rx.recv() => {
                    let Some(PromptEvent::FormChanged {
                        field_id, values, ..
                    }) = event
                    else {
                        continue;
                    };
                    if is_numeric_configure_field(&field_id) {
                        pending_numeric = Some((field_id, values));
                        numeric_debounce.as_mut().reset(numeric_preview_deadline(tokio::time::Instant::now()));
                        continue;
                    }
                    pending_numeric = None;
                    let Ok(parsed_spec) = spec_from_form(&values, spec) else {
                        continue;
                    };
                    let next_spec = apply_linked_edge(&values, &field_id, parsed_spec);
                    let next_scope = single_value(&values, "scope")
                        .and_then(|value| ConfigureScope::parse(value).ok())
                        .unwrap_or(scope);
                    let next_refresh = bool_value(&values, "refresh-targets").unwrap_or(refresh_value);
                    let refresh_requested = next_refresh != refresh_value;
                    let next_targets = if next_scope == scope && !refresh_requested {
                        targets.clone()
                    } else {
                        let Ok(resolved) = resolve_scope(&context, session_id, pane_id, next_scope)
                        else {
                            continue;
                        };
                        resolved
                    };
                    let result = bmux_plugin::block_on_typed_dispatch(
                        pane_runtime_commands::client::update_pane_padding_preview(
                            &mut client, token, next_targets.clone(), preview_spec(next_spec),
                        ),
                    );
                    if matches!(result, Ok(Ok(_))) {
                        scope = next_scope;
                        refresh_value = next_refresh;
                        spec = next_spec;
                        targets = next_targets;
                    }
                }
            }
        }
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attach_outcome_records_status_without_cli_output_path() {
        let state = PanePaddingState {
            session_id: Uuid::new_v4(),
            pane_id: Uuid::new_v4(),
            declarative: bmux_pane_runtime_plugin_api::pane_runtime_state::PanePaddingSpec {
                left: 0,
                right: 0,
                top: 0,
                bottom: 0,
                max_content_width: None,
                max_content_height: None,
                horizontal_alignment: "left".to_string(),
                vertical_alignment: "top".to_string(),
            },
            matched_rule_index: None,
            runtime_override: None,
            effective: bmux_pane_runtime_plugin_api::pane_runtime_state::PanePaddingSpec {
                left: 1,
                right: 1,
                top: 0,
                bottom: 0,
                max_content_width: Some(80),
                max_content_height: None,
                horizontal_alignment: "center".to_string(),
                vertical_alignment: "top".to_string(),
            },
            source: "global".to_string(),
            outer_rect: None,
            base_content_rect: None,
            effective_content_rect: None,
            persist_runtime_overrides: true,
        };
        let context = test_context(NativeCommandInvocationSource::AttachKeybinding);
        bmux_plugin_sdk::begin_command_outcome_capture();

        emit_result(&context, state);

        let outcome = bmux_plugin_sdk::finish_command_outcome_capture();
        assert_eq!(
            outcome
                .metadata
                .get(COMMAND_OUTCOME_STATUS_MESSAGE_KEY)
                .and_then(serde_json::Value::as_str),
            Some("pane padding: edges 1/1/0/0, max 80xnone, align center/top")
        );
    }

    fn test_context(invocation_source: NativeCommandInvocationSource) -> NativeCommandContext {
        use bmux_plugin_sdk::{
            CURRENT_PLUGIN_ABI_VERSION, CURRENT_PLUGIN_API_VERSION, HostConnectionInfo,
            HostMetadata,
        };
        NativeCommandContext {
            plugin_id: "bmux.pane_runtime".to_string(),
            command: "pane-padding-show".to_string(),
            arguments: Vec::new(),
            required_capabilities: Vec::new(),
            provided_capabilities: Vec::new(),
            services: Vec::new(),
            available_capabilities: Vec::new(),
            enabled_plugins: Vec::new(),
            plugin_search_roots: Vec::new(),
            registered_plugins: Vec::new(),
            active_keybindings: Vec::new(),
            host: HostMetadata {
                product_name: "bmux".to_string(),
                product_version: "test".to_string(),
                plugin_api_version: CURRENT_PLUGIN_API_VERSION,
                plugin_abi_version: CURRENT_PLUGIN_ABI_VERSION,
            },
            connection: HostConnectionInfo {
                config_dir: "/tmp".to_string(),
                config_dir_candidates: Vec::new(),
                runtime_dir: "/tmp".to_string(),
                data_dir: "/tmp".to_string(),
                state_dir: "/tmp".to_string(),
            },
            settings: None,
            plugin_settings_map: std::collections::BTreeMap::new(),
            caller_client_id: None,
            invocation_source,
            host_kernel_bridge: None,
        }
    }

    #[test]
    fn preview_feedback_reports_mixed_constraint_and_clamp_counts() {
        let session_id = Uuid::new_v4();
        let first = crate::padding_api::RuntimePanePaddingState {
            session_id,
            pane_id: Uuid::new_v4(),
            declarative: PanePaddingSpec::default(),
            matched_rule_index: None,
            runtime_override: None,
            effective: PanePaddingSpec::default(),
            outer_rect: bmux_attach_layout_protocol::AttachRect {
                x: 0,
                y: 0,
                w: 100,
                h: 40,
            },
            base_content_rect: bmux_attach_layout_protocol::AttachRect {
                x: 1,
                y: 1,
                w: 98,
                h: 38,
            },
            effective_content_rect: bmux_attach_layout_protocol::AttachRect {
                x: 1,
                y: 1,
                w: 98,
                h: 38,
            },
            persist_runtime_overrides: true,
        };
        let second = crate::padding_api::RuntimePanePaddingState {
            pane_id: Uuid::new_v4(),
            effective: PanePaddingSpec {
                left: 20,
                ..PanePaddingSpec::default()
            },
            effective_content_rect: bmux_attach_layout_protocol::AttachRect {
                x: 20,
                y: 1,
                w: 1,
                h: 38,
            },
            ..first
        };

        let feedback = preview_feedback(&[first, second]);

        assert_eq!(
            feedback,
            "Targets: 2 panes | 1 constrained | 1 unchanged | 1 clamped | Values: mixed"
        );
    }

    #[tokio::test]
    async fn numeric_preview_deadline_waits_for_the_settle_window() {
        let start = tokio::time::Instant::now();
        let deadline = numeric_preview_deadline(start);
        assert_eq!(deadline.duration_since(start), NUMERIC_PREVIEW_DEBOUNCE);
        tokio::time::sleep_until(deadline).await;
        assert!(tokio::time::Instant::now() >= deadline);
    }

    #[test]
    fn external_global_config_edit_is_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("bmux.toml");
        let original = b"# original\n";
        fs::write(&path, original).unwrap();
        fs::write(&path, b"# externally changed\n").unwrap();

        let error = verify_config_unchanged(&path, original).expect_err("conflict must fail");

        assert!(error.to_string().contains("changed while pane padding"));
        assert_eq!(fs::read(&path).unwrap(), b"# externally changed\n");
    }

    #[test]
    fn missing_global_config_creates_focused_padding_table() {
        let edited = set_padding_document(
            "",
            PanePaddingSpec {
                left: 2,
                max_content_width: Some(90),
                ..PanePaddingSpec::default()
            },
        )
        .unwrap()
        .to_string();

        assert!(edited.contains("plugins = { settings ="));
        assert!(edited.contains("\"bmux.pane_runtime\""));
        assert!(edited.contains("padding = { left = 2"));
        assert!(edited.contains("max_content_width = 90"));
    }

    #[test]
    fn invalid_global_config_is_not_overwritten() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("bmux.toml");
        let invalid = b"[plugins.settings\n";
        fs::write(&path, invalid).unwrap();

        let source = String::from_utf8(fs::read(&path).unwrap()).unwrap();
        set_padding_document(&source, PanePaddingSpec::default())
            .expect_err("invalid TOML must fail before persistence");

        assert_eq!(fs::read(&path).unwrap(), invalid);
    }

    #[test]
    fn temporary_write_failure_preserves_original_config() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("bmux.toml");
        let original = b"# original\n";
        fs::write(&path, original).unwrap();
        let blocked_temp = temp.path().join("blocked-temp");
        fs::create_dir(&blocked_temp).unwrap();
        TEST_ATOMIC_TEMP_PATH.with(|slot| *slot.borrow_mut() = Some(blocked_temp));

        atomic_replace(&path, b"# edited\n").expect_err("temp creation must fail");

        assert_eq!(fs::read(&path).unwrap(), original);
    }

    #[test]
    fn atomic_replace_creates_and_replaces_config() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("bmux.toml");

        atomic_replace(&path, b"first\n").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"first\n");
        atomic_replace(&path, b"second\n").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"second\n");
    }

    #[test]
    fn global_padding_install_restores_file_when_live_install_fails() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("bmux.toml");
        let original = b"# original\n";
        fs::write(&path, original).unwrap();

        let error = persist_and_install_global_padding(&path, original, b"# edited\n", || {
            Err(PluginCommandError::failed("injected install failure"))
        })
        .expect_err("install should fail");

        assert!(error.to_string().contains("injected install failure"));
        assert_eq!(fs::read(&path).unwrap(), original);
    }

    #[test]
    fn global_padding_edit_preserves_unrelated_content() {
        let source = r#"# keep this comment
[ui]
status = true

[plugins.settings."bmux.pane_runtime".padding]
left = 9 # old value

[plugins.settings."bmux.other"]
enabled = true
"#;
        let spec = PanePaddingSpec {
            left: 2,
            right: 3,
            top: 1,
            bottom: 4,
            max_content_width: Some(100),
            horizontal_alignment: HorizontalAlignment::Center,
            ..PanePaddingSpec::default()
        };

        let edited = set_padding_document(source, spec).unwrap().to_string();

        assert!(edited.contains("# keep this comment"));
        assert!(edited.contains("[ui]"));
        assert!(edited.contains("[plugins.settings.\"bmux.other\"]"));
        assert!(edited.contains("left = 2"));
        assert!(edited.contains("max_content_width = 100"));
        assert!(edited.contains("horizontal_alignment = \"center\""));
    }

    #[test]
    fn form_limits_and_presets_produce_complete_specs() {
        let values = BTreeMap::from([
            (
                "preset".to_string(),
                PromptFormValue::Single("custom".to_string()),
            ),
            ("limit-width".to_string(), PromptFormValue::Bool(true)),
            (
                "max-content-width".to_string(),
                PromptFormValue::Integer(88),
            ),
            ("limit-height".to_string(), PromptFormValue::Bool(false)),
        ]);
        let spec = spec_from_form(&values, PanePaddingSpec::default()).unwrap();
        assert_eq!(spec.max_content_width, Some(88));
        assert_eq!(spec.max_content_height, None);

        let preset = BTreeMap::from([(
            "preset".to_string(),
            PromptFormValue::Single("presentation".to_string()),
        )]);
        let spec = spec_from_form(&preset, PanePaddingSpec::default()).unwrap();
        assert_eq!((spec.left, spec.right, spec.top, spec.bottom), (0, 0, 0, 0));
        assert_eq!(spec.max_content_width, Some(100));
        assert_eq!(spec.max_content_height, Some(34));
        assert_eq!(spec.horizontal_alignment, HorizontalAlignment::Center);
        assert_eq!(spec.vertical_alignment, VerticalAlignment::Center);

        let centered = BTreeMap::from([(
            "preset".to_string(),
            PromptFormValue::Single("centered-120".to_string()),
        )]);
        let spec = spec_from_form(&centered, PanePaddingSpec::default()).unwrap();
        assert_eq!(spec.max_content_width, Some(120));
        assert_eq!(spec.horizontal_alignment, HorizontalAlignment::Center);
    }

    #[test]
    fn configurator_debounces_only_numeric_fields() {
        for field in [
            "left",
            "right",
            "top",
            "bottom",
            "max-content-width",
            "max-content-height",
        ] {
            assert!(is_numeric_configure_field(field), "{field}");
        }
        for field in [
            "scope",
            "lifetime",
            "preset",
            "link-horizontal",
            "limit-width",
            "horizontal-alignment",
        ] {
            assert!(!is_numeric_configure_field(field), "{field}");
        }
    }

    #[test]
    fn form_linked_edges_follow_the_changed_field() {
        let values = BTreeMap::from([
            (
                "preset".to_string(),
                PromptFormValue::Single("custom".to_string()),
            ),
            ("link-horizontal".to_string(), PromptFormValue::Bool(true)),
            ("link-vertical".to_string(), PromptFormValue::Bool(true)),
            ("left".to_string(), PromptFormValue::Integer(7)),
            ("right".to_string(), PromptFormValue::Integer(2)),
            ("top".to_string(), PromptFormValue::Integer(3)),
            ("bottom".to_string(), PromptFormValue::Integer(9)),
        ]);

        let horizontal = apply_linked_edge(
            &values,
            "left",
            spec_from_form(&values, PanePaddingSpec::default()).unwrap(),
        );
        assert_eq!((horizontal.left, horizontal.right), (7, 7));

        let vertical = apply_linked_edge(
            &values,
            "bottom",
            spec_from_form(&values, PanePaddingSpec::default()).unwrap(),
        );
        assert_eq!((vertical.top, vertical.bottom), (9, 9));
    }

    #[test]
    fn form_omits_window_scope_when_windows_capability_is_missing() {
        fn scope_values(request: &PromptRequest) -> Vec<&str> {
            let bmux_plugin_sdk::PromptField::Form { sections, .. } = &request.field else {
                panic!("configure request must be a form");
            };
            let PromptFormFieldKind::SingleSelect { options, .. } = &sections[0].fields[0].kind
            else {
                panic!("scope must be a single select");
            };
            options.iter().map(|option| option.value.as_str()).collect()
        }

        let without_windows = configure_request(PanePaddingSpec::default(), false, None);
        let with_windows = configure_request(PanePaddingSpec::default(), true, None);
        assert!(!scope_values(&without_windows).contains(&"window"));
        assert!(scope_values(&with_windows).contains(&"window"));

        let bmux_plugin_sdk::PromptField::Form { sections, .. } = with_windows.field else {
            panic!("configure request must be a form");
        };
        assert!(
            sections[0]
                .fields
                .iter()
                .any(|field| field.id == "refresh-targets")
        );
    }

    #[test]
    fn status_text_reports_effective_edges_limits_and_alignment() {
        let state = PanePaddingState {
            session_id: Uuid::new_v4(),
            pane_id: Uuid::new_v4(),
            declarative: bmux_pane_runtime_plugin_api::pane_runtime_state::PanePaddingSpec {
                left: 0,
                right: 0,
                top: 0,
                bottom: 0,
                max_content_width: None,
                max_content_height: None,
                horizontal_alignment: "left".to_string(),
                vertical_alignment: "top".to_string(),
            },
            matched_rule_index: None,
            runtime_override: None,
            effective: bmux_pane_runtime_plugin_api::pane_runtime_state::PanePaddingSpec {
                left: 1,
                right: 2,
                top: 3,
                bottom: 4,
                max_content_width: Some(120),
                max_content_height: None,
                horizontal_alignment: "center".to_string(),
                vertical_alignment: "bottom".to_string(),
            },
            source: "runtime_override".to_string(),
            outer_rect: None,
            base_content_rect: None,
            effective_content_rect: None,
            persist_runtime_overrides: true,
        };

        assert_eq!(
            status_text(&state),
            "pane padding: edges 1/2/3/4, max 120xnone, align center/bottom"
        );
    }

    #[test]
    fn manifest_owns_nested_padding_commands_and_declares_arguments() {
        let manifest = bmux_plugin::PluginManifest::from_toml_str(include_str!("../plugin.toml"))
            .expect("pane-runtime manifest parses");
        assert!(manifest.owns_namespaces.contains("pane-padding"));
        let commands = manifest
            .commands
            .iter()
            .map(|command| (command.name.as_str(), command))
            .collect::<std::collections::BTreeMap<_, _>>();
        for (name, leaf) in [
            ("pane-padding-show", "show"),
            ("pane-padding-set", "set"),
            ("pane-padding-reset", "reset"),
        ] {
            let command = commands.get(name).expect("padding command declared");
            assert_eq!(command.path, ["pane-padding", leaf]);
            assert!(command.expose_in_cli);
            assert!(
                command
                    .arguments
                    .iter()
                    .any(|argument| argument.name == "session")
            );
            assert!(
                command
                    .arguments
                    .iter()
                    .any(|argument| argument.name == "pane-id")
            );
        }
        let set = commands["pane-padding-set"];
        for argument in [
            "left",
            "right",
            "top",
            "bottom",
            "horizontal",
            "vertical",
            "all",
            "max-content-width",
            "max-content-height",
            "horizontal-alignment",
            "vertical-alignment",
        ] {
            assert!(set.arguments.iter().any(|entry| entry.name == argument));
        }
    }

    #[test]
    fn option_precedence_covers_axes_and_individual_edges() {
        let args = vec![
            "--all".into(),
            "1".into(),
            "--horizontal".into(),
            "2".into(),
            "--vertical=4".into(),
            "--left".into(),
            "3".into(),
            "--bottom=5".into(),
        ];
        let spec = set_spec(&args, PanePaddingSpec::default()).unwrap();
        assert_eq!((spec.left, spec.right, spec.top, spec.bottom), (3, 2, 4, 5));
    }

    #[test]
    fn session_selector_normalizes_uuid_name_and_rejects_blank() {
        let id = Uuid::new_v4();
        let by_id = session_selector(&[format!("--session={id}")])
            .unwrap()
            .expect("UUID selector");
        assert_eq!(by_id.id, Some(id));
        assert_eq!(by_id.name, None);
        let by_name = session_selector(&["--session".into(), "work".into()])
            .unwrap()
            .expect("name selector");
        assert_eq!(by_name.id, None);
        assert_eq!(by_name.name.as_deref(), Some("work"));
        assert!(session_selector(&["--session=  ".into()]).is_err());
        assert_eq!(session_selector(&[]).unwrap(), None);
    }

    #[test]
    fn target_and_numeric_arguments_are_normalized_and_validated() {
        let pane_id = Uuid::new_v4();
        assert_eq!(
            target_pane(&[format!("--pane-id={pane_id}")]).unwrap(),
            Some(pane_id)
        );
        assert!(target_pane(&["--pane-id".into(), "not-a-uuid".into()]).is_err());
        assert_eq!(
            parse_u16(&["--left=65535".into()], "left").unwrap(),
            Some(u16::MAX)
        );
        assert!(parse_u16(&["--left=-1".into()], "left").is_err());
        assert!(parse_limit(&["--max-content-width=0".into()], "max-content-width").is_err());
    }

    #[test]
    fn alignments_and_limits_update_complete_effective_spec() {
        let changed = set_spec(
            &[
                "--max-content-width=120".into(),
                "--max-content-height".into(),
                "40".into(),
                "--horizontal-alignment=center".into(),
                "--vertical-alignment".into(),
                "bottom".into(),
            ],
            PanePaddingSpec::default(),
        )
        .unwrap();
        assert_eq!(changed.max_content_width, Some(120));
        assert_eq!(changed.max_content_height, Some(40));
        assert_eq!(changed.horizontal_alignment, HorizontalAlignment::Center);
        assert_eq!(changed.vertical_alignment, VerticalAlignment::Bottom);
        assert!(set_spec(&["--horizontal-alignment=middle".into()], changed).is_err());
        assert!(set_spec(&["--vertical-alignment=middle".into()], changed).is_err());
    }

    #[test]
    fn none_clears_maximum_and_empty_set_is_rejected() {
        let spec = PanePaddingSpec {
            max_content_width: Some(100),
            ..PanePaddingSpec::default()
        };
        let changed = set_spec(&["--max-content-width=none".into()], spec).unwrap();
        assert_eq!(changed.max_content_width, None);
        assert!(set_spec(&[], spec).is_err());
    }
}
