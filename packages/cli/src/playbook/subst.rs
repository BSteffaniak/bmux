//! Variable substitution for playbook values.
//!
//! Supports two phases:
//! - **Parse-time**: `@var` directives and environment variables
//! - **Runtime**: dynamic variables like `${SESSION_ID}`, `${PANE_COUNT}`

use std::collections::BTreeMap;
use uuid::Uuid;

/// Runtime variable context, updated as the playbook executes.
#[derive(Debug, Clone)]
pub struct RuntimeVars {
    /// Static variables from `@var` directives and environment, resolved at parse time.
    pub static_vars: BTreeMap<String, String>,
    /// Current session ID (set after `new-session`).
    pub session_id: Option<Uuid>,
    /// Current session name.
    pub session_name: Option<String>,
    /// Current pane count.
    pub pane_count: u32,
    /// Currently focused pane index.
    pub focused_pane: u32,
}

impl RuntimeVars {
    pub fn new(static_vars: BTreeMap<String, String>) -> Self {
        Self {
            static_vars,
            session_id: None,
            session_name: None,
            pane_count: 0,
            focused_pane: 0,
        }
    }

    /// Resolve all `${NAME}` references in a template string.
    ///
    /// Resolution order:
    /// 1. Runtime variables (SESSION_ID, SESSION_NAME, PANE_COUNT, FOCUSED_PANE)
    /// 2. Static variables (`@var` directives)
    /// 3. Environment variables
    /// 4. Unresolved references are left as-is
    pub fn resolve(&self, template: &str) -> String {
        let mut result = String::with_capacity(template.len());
        let bytes = template.as_bytes();
        let mut i = 0;

        while i < bytes.len() {
            if i + 1 < bytes.len() && bytes[i] == b'$' && bytes[i + 1] == b'{' {
                // Find closing brace
                if let Some(close) = template[i + 2..].find('}') {
                    let var_name = &template[i + 2..i + 2 + close];
                    if let Some(value) = self.lookup(var_name) {
                        result.push_str(&value);
                    } else {
                        // Leave unresolved
                        result.push_str(&template[i..i + 2 + close + 1]);
                    }
                    i += 2 + close + 1;
                    continue;
                }
            }
            result.push(bytes[i] as char);
            i += 1;
        }

        result
    }

    /// Resolve a string only if it contains `${`.
    /// Returns the original string unchanged if no substitution markers are present.
    pub fn resolve_opt(&self, value: &str) -> String {
        if value.contains("${") {
            self.resolve(value)
        } else {
            value.to_string()
        }
    }

    /// Resolve bytes (for send-keys): substitute in the string representation,
    /// then re-encode. Only works if the bytes are valid UTF-8 containing `${`.
    pub fn resolve_bytes(&self, bytes: &[u8]) -> Vec<u8> {
        if let Ok(s) = std::str::from_utf8(bytes) {
            if s.contains("${") {
                return self.resolve(s).into_bytes();
            }
        }
        bytes.to_vec()
    }

    fn lookup(&self, name: &str) -> Option<String> {
        // 1. Runtime variables
        match name {
            "SESSION_ID" => {
                return self.session_id.map(|id| id.to_string());
            }
            "SESSION_NAME" => {
                return self.session_name.clone();
            }
            "PANE_COUNT" => {
                return Some(self.pane_count.to_string());
            }
            "FOCUSED_PANE" => {
                return Some(self.focused_pane.to_string());
            }
            _ => {}
        }

        // 2. Static variables (@var directives)
        if let Some(value) = self.static_vars.get(name) {
            return Some(value.clone());
        }

        // 3. Environment variables
        std::env::var(name).ok()
    }
}

/// Resolve static variables (parse-time substitution).
/// Used for the first pass before execution starts.
#[allow(dead_code)]
pub fn resolve_static(template: &str, vars: &BTreeMap<String, String>) -> String {
    let runtime = RuntimeVars::new(vars.clone());
    // Only resolve static vars and env vars (runtime vars won't match anything yet)
    runtime.resolve(template)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_static_var() {
        let mut vars = BTreeMap::new();
        vars.insert("GREETING".to_string(), "hello".to_string());
        let rv = RuntimeVars::new(vars);
        assert_eq!(rv.resolve("echo ${GREETING}"), "echo hello");
    }

    #[test]
    fn resolve_env_var() {
        // SAFETY: test-only, single-threaded test runner.
        unsafe { std::env::set_var("BMUX_TEST_VAR_XYZ", "from-env") };
        let rv = RuntimeVars::new(BTreeMap::new());
        assert_eq!(rv.resolve("val=${BMUX_TEST_VAR_XYZ}"), "val=from-env");
        unsafe { std::env::remove_var("BMUX_TEST_VAR_XYZ") };
    }

    #[test]
    fn resolve_runtime_session_id() {
        let mut rv = RuntimeVars::new(BTreeMap::new());
        let id = Uuid::nil();
        rv.session_id = Some(id);
        assert_eq!(rv.resolve("session=${SESSION_ID}"), format!("session={id}"));
    }

    #[test]
    fn resolve_runtime_pane_count() {
        let mut rv = RuntimeVars::new(BTreeMap::new());
        rv.pane_count = 3;
        assert_eq!(rv.resolve("panes=${PANE_COUNT}"), "panes=3");
    }

    #[test]
    fn unresolved_left_as_is() {
        let rv = RuntimeVars::new(BTreeMap::new());
        assert_eq!(
            rv.resolve("val=${UNKNOWN_VAR_ABC}"),
            "val=${UNKNOWN_VAR_ABC}"
        );
    }

    #[test]
    fn no_substitution_markers_unchanged() {
        let rv = RuntimeVars::new(BTreeMap::new());
        assert_eq!(rv.resolve("plain text"), "plain text");
    }

    #[test]
    fn multiple_substitutions() {
        let mut vars = BTreeMap::new();
        vars.insert("A".to_string(), "1".to_string());
        vars.insert("B".to_string(), "2".to_string());
        let rv = RuntimeVars::new(vars);
        assert_eq!(rv.resolve("${A}+${B}"), "1+2");
    }

    #[test]
    fn static_takes_priority_over_env() {
        // SAFETY: test-only, single-threaded test runner.
        unsafe { std::env::set_var("BMUX_TEST_PRIORITY", "env-val") };
        let mut vars = BTreeMap::new();
        vars.insert("BMUX_TEST_PRIORITY".to_string(), "static-val".to_string());
        let rv = RuntimeVars::new(vars);
        assert_eq!(rv.resolve("${BMUX_TEST_PRIORITY}"), "static-val");
        unsafe { std::env::remove_var("BMUX_TEST_PRIORITY") };
    }

    #[test]
    fn resolve_bytes_with_substitution() {
        let mut vars = BTreeMap::new();
        vars.insert("CMD".to_string(), "ls".to_string());
        let rv = RuntimeVars::new(vars);
        let input = b"echo ${CMD}\r";
        let output = rv.resolve_bytes(input);
        assert_eq!(output, b"echo ls\r");
    }
}
