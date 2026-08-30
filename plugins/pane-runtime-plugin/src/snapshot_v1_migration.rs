//! Migration of pane-runtime snapshot v1 viewport fields.

use serde_json::Value;

pub(super) fn migrate(bytes: &[u8]) -> serde_json::Result<Vec<u8>> {
    let mut payload: Value = serde_json::from_slice(bytes)?;
    if let Some(sessions) = payload.get_mut("sessions").and_then(Value::as_array_mut) {
        for session in sessions {
            let Some(viewport) = session
                .get_mut("attach_viewport")
                .and_then(Value::as_object_mut)
            else {
                continue;
            };
            rename_if_absent(viewport, "status_top_inset", "top_inset");
            rename_if_absent(viewport, "status_bottom_inset", "bottom_inset");
        }
    }
    serde_json::to_vec(&payload)
}

fn rename_if_absent(
    object: &mut serde_json::Map<String, Value>,
    legacy_name: &str,
    current_name: &str,
) {
    let legacy_value = object.remove(legacy_name);
    if !object.contains_key(current_name)
        && let Some(value) = legacy_value
    {
        object.insert(current_name.to_string(), value);
    }
}

#[cfg(test)]
mod tests {
    use super::migrate;
    use serde_json::Value;

    #[test]
    fn migrates_legacy_insets_and_preserves_current_values() {
        let mut viewport = serde_json::Map::from_iter([
            ("cols".to_string(), Value::from(120)),
            ("rows".to_string(), Value::from(40)),
            ("top_inset".to_string(), Value::from(3)),
        ]);
        viewport.insert(["status", "top", "inset"].join("_"), Value::from(2));
        viewport.insert(["status", "bottom", "inset"].join("_"), Value::from(1));
        let payload = serde_json::json!({
            "sessions": [{ "attach_viewport": viewport }]
        });

        let migrated: Value =
            serde_json::from_slice(&migrate(&serde_json::to_vec(&payload).unwrap()).unwrap())
                .unwrap();
        let viewport = &migrated["sessions"][0]["attach_viewport"];
        assert_eq!(viewport["top_inset"], 3);
        assert_eq!(viewport["bottom_inset"], 1);
        assert!(viewport.as_object().is_some_and(|object| object.len() == 4));
    }
}
