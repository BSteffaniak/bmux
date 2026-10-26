//! Explicit selection and fail-closed legacy recovery boundaries.
use bmux_plugin_sdk::ServiceResponse;
use bmux_terminal_protocol::pointer_shape::PointerShapeState;
use serde::Serialize;
use serde_json::Value;

pub(super) fn select(version: u32) -> Result<(), ServiceResponse> {
    if version == 1 {
        Ok(())
    } else {
        Err(ServiceResponse::error(
            "unsupported_representation",
            "pointer-grid requires format_version=1",
        ))
    }
}

// Only the service boundary decides whether a legacy consumer can represent the
// complete response. Never mutate canonical state or discard stack entries.
pub(super) fn legacy<T: Serialize>(value: T) -> Result<T, ServiceResponse> {
    let encoded = serde_json::to_value(&value)
        .map_err(|error| ServiceResponse::error("encode_failed", error.to_string()))?;
    check(&encoded)
        .map_err(|reason| ServiceResponse::error("unsupported_representation", reason))?;
    Ok(value)
}

fn check(value: &Value) -> Result<(), &'static str> {
    match value {
        Value::Object(fields) => {
            if let Some(state) = fields.get("pointer_shapes") {
                let state: PointerShapeState = serde_json::from_value(state.clone())
                    .map_err(|_| "invalid pointer recovery state")?;
                if state != PointerShapeState::default() {
                    return Err("pointer stacks require pane-runtime-pointer-grid-v1");
                }
            }
            for (key, child) in fields {
                if matches!(key.as_str(), "encoded" | "encoded_grid" | "pending_bytes") {
                    let bytes = child.as_array().and_then(|items| {
                        items
                            .iter()
                            .map(|item| item.as_u64().and_then(|n| u8::try_from(n).ok()))
                            .collect::<Option<Vec<_>>>()
                    });
                    if let Some(bytes) = bytes {
                        if key == "pending_bytes" {
                            let osc22 = b"\x1b]22;";
                            let c1_osc22 = b"\x9d22;";
                            if !bytes.is_empty()
                                && (bytes.starts_with(osc22)
                                    || osc22.starts_with(&bytes)
                                    || bytes.starts_with(c1_osc22)
                                    || c1_osc22.starts_with(&bytes))
                            {
                                return Err("pending OSC 22 requires pane-runtime-pointer-grid-v1");
                            }
                        } else if !bytes.is_empty() {
                            let payload: Value = serde_json::from_slice(&bytes)
                                .map_err(|_| "invalid structured grid representation")?;
                            check(&payload)?;
                        }
                    }
                } else {
                    check(child)?;
                }
            }
        }
        Value::Array(items) => {
            for item in items {
                check(item)?;
            }
        }
        _ => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use bmux_terminal_grid::{GridLimits, TerminalGridStream};

    #[test]
    fn selection_rejects_unknown_versions() {
        assert!(select(1).is_ok());
        assert!(select(0).is_err());
        assert!(select(2).is_err());
    }

    #[test]
    fn legacy_accepts_empty_state_but_rejects_hidden_stack_and_pending_query() {
        let mut grid = TerminalGridStream::new(10, 2, GridLimits::default()).unwrap();
        assert!(legacy(grid.snapshot(0, 2)).is_ok());
        grid.process(b"\x1b]22;pointer\x07\x1b]22;>crosshair\x07\x1b]22;\x07");
        // An unset top still has a lower entry that a later pop can expose.
        assert!(legacy(grid.snapshot(0, 2)).is_err());
        grid.process(b"\x1bc\x1b]22;?__cur");
        assert!(legacy(grid.snapshot(0, 2)).is_err());
    }

    #[test]
    fn legacy_checks_execution_and_delta_opaque_payloads() {
        let mut grid = TerminalGridStream::new(10, 2, GridLimits::default()).unwrap();
        let delta = grid.process_delta(b"\x1b]22;pointer\x07").unwrap();
        for (key, payload) in [
            ("encoded", serde_json::to_vec(&vec![delta]).unwrap()),
            (
                "encoded_grid",
                serde_json::to_vec(&grid.snapshot(0, 2)).unwrap(),
            ),
        ] {
            let value = serde_json::json!({(key): payload});
            assert!(legacy(value).is_err());
        }
    }
}
