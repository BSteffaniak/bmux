use bmux_clients_plugin_api::clients_state;
use bmux_pane_runtime_plugin_api::pane_runtime_state::PanePaddingState;
use bmux_plugin::ServiceCallerDispatchClient;
use bmux_plugin_sdk::{
    COMMAND_OUTCOME_STATUS_MESSAGE_KEY, NativeCommandContext, NativeCommandInvocationSource,
    PluginCommandError, record_command_outcome_metadata,
};
use bmux_sessions_plugin_api::sessions_state;
use uuid::Uuid;

use crate::padding::{HorizontalAlignment, PanePaddingSpec, VerticalAlignment};
use crate::runtime::PanePaddingRuntimeHandle;
use bmux_session_models::SessionId;

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

fn selected_session(context: &NativeCommandContext) -> Result<SessionId, PluginCommandError> {
    let mut client = ServiceCallerDispatchClient::new(context);
    if let Some(selector) = option_value(&context.arguments, "session") {
        let selector = selector.trim();
        if selector.is_empty() {
            return Err(PluginCommandError::invalid_arguments(
                "--session expects a session UUID or name",
            ));
        }
        let parsed_id = Uuid::parse_str(selector).ok();
        let result = bmux_plugin::block_on_typed_dispatch(sessions_state::client::get_session(
            &mut client,
            sessions_state::SessionSelector {
                id: parsed_id,
                name: parsed_id.is_none().then(|| selector.to_string()),
            },
        ))
        .map_err(|error| PluginCommandError::failed(format!("session lookup failed: {error}")))?
        .map_err(|error| {
            PluginCommandError::failed(format!("session selector did not resolve: {error:?}"))
        })?;
        return Ok(SessionId(result.id));
    }
    let current =
        bmux_plugin::block_on_typed_dispatch(clients_state::client::current_client(&mut client))
            .map_err(|error| {
                PluginCommandError::failed(format!("current client lookup failed: {error}"))
            })?
            .map_err(|error| {
                PluginCommandError::failed(format!("current client unavailable: {error:?}"))
            })?;
    current
        .selected_session_id
        .map(SessionId)
        .ok_or_else(|| PluginCommandError::failed("current client has no selected session"))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn option_precedence_prefers_individual_edges() {
        let args = vec![
            "--all".into(),
            "1".into(),
            "--horizontal".into(),
            "2".into(),
            "--left".into(),
            "3".into(),
        ];
        let spec = set_spec(&args, PanePaddingSpec::default()).unwrap();
        assert_eq!((spec.left, spec.right, spec.top, spec.bottom), (3, 2, 1, 1));
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
