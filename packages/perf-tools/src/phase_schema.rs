use serde_json::Value;

pub(crate) fn validate_phase_schema(events: &[Value]) -> Result<(), Vec<String>> {
    let mut errors = Vec::new();
    for (index, event) in events.iter().enumerate() {
        let Some(phase) = event.get("phase").and_then(Value::as_str) else {
            errors.push(format!("event[{index}] missing string phase"));
            continue;
        };
        let required = required_fields(phase);
        for field in required {
            if event.get(field).is_none() {
                errors.push(format!("event[{index}] phase '{phase}' missing '{field}'"));
            }
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

fn required_fields(phase: &str) -> &'static [&'static str] {
    if phase == "plugin.command" {
        &["plugin_id", "command_name", "total_us"]
    } else if phase.starts_with("service.") || phase == "plugin.native_service_invoke" {
        &[
            "capability",
            "kind",
            "interface_id",
            "operation",
            "total_us",
        ]
    } else if phase.starts_with("ipc.") {
        &["request", "response", "total_us"]
    } else if phase.starts_with("storage.") || phase.starts_with("volatile_state.") {
        &["plugin_id", "key", "total_us"]
    } else if phase.starts_with("attach.") {
        &["command_name", "plugin_id", "total_us"]
    } else {
        &["total_us"]
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn validates_service_required_fields() {
        let events = vec![json!({
            "phase": "service.client_invoke",
            "capability": "bmux.example",
            "kind": "Query",
            "interface_id": "example/v1",
            "operation": "get",
            "total_us": 1,
        })];
        assert!(validate_phase_schema(&events).is_ok());
    }

    #[test]
    fn validates_plugin_command_required_fields() {
        let events = vec![json!({
            "phase": "plugin.command",
            "plugin_id": "bmux.example",
            "command_name": "do-work",
            "total_us": 1,
        })];
        assert!(validate_phase_schema(&events).is_ok());
    }

    #[test]
    fn reports_missing_required_field() {
        let events = vec![json!({
            "phase": "ipc.client_request",
            "request": "invoke_service",
            "total_us": 1,
        })];
        let errors = validate_phase_schema(&events).unwrap_err();
        assert!(errors[0].contains("response"));
    }
}
