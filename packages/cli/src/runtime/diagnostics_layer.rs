use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use bmux_config::{DiagnosticTraceLevel, DiagnosticsConfig};
use serde::Serialize;
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::Layer;
use tracing_subscriber::layer::Context;
use tracing_subscriber::registry::LookupSpan;

#[derive(Debug)]
pub(super) struct OperationalDiagnosticsLayer {
    config: DiagnosticsConfig,
    path: PathBuf,
    writer: Mutex<Option<std::fs::File>>,
}

#[derive(Debug, Clone, Default)]
struct DiagnosticFields {
    values: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize)]
struct PersistedDiagnosticEvent {
    timestamp_ms: u128,
    level: String,
    target: String,
    component: String,
    message: Option<String>,
    fields: BTreeMap<String, String>,
}

impl OperationalDiagnosticsLayer {
    pub(super) fn new(config: DiagnosticsConfig, state_dir: &std::path::Path) -> Option<Self> {
        if !config.enabled || !config.persist {
            return None;
        }
        let path = state_dir.join("diagnostics").join("events.jsonl");
        Some(Self {
            config,
            path,
            writer: Mutex::new(None),
        })
    }

    fn component_for(&self, target: &str, fields: &DiagnosticFields) -> String {
        if let Some(component) = fields.values.get("bmux.component") {
            return component.clone();
        }
        self.config
            .targets
            .iter()
            .filter(|(prefix, rule)| {
                !rule.component.is_empty() && target.starts_with(prefix.as_str())
            })
            .max_by_key(|(prefix, _)| prefix.len())
            .map_or_else(|| target.to_string(), |(_, rule)| rule.component.clone())
    }

    fn min_level_for(&self, target: &str, component: &str) -> DiagnosticTraceLevel {
        if let Some(level) = self
            .config
            .targets
            .iter()
            .filter(|(prefix, _)| target.starts_with(prefix.as_str()))
            .max_by_key(|(prefix, _)| prefix.len())
            .and_then(|(_, rule)| rule.min_level)
        {
            return level;
        }
        self.config
            .components
            .get(component)
            .and_then(|policy| policy.min_level)
            .unwrap_or(self.config.min_level)
    }

    const fn should_capture(level: Level, min_level: DiagnosticTraceLevel) -> bool {
        level_rank(level) <= config_level_rank(min_level)
    }

    fn persist(&self, event: &PersistedDiagnosticEvent) {
        let Ok(line) = serde_json::to_string(event) else {
            return;
        };
        let Ok(mut guard) = self.writer.lock() else {
            return;
        };
        if guard.is_none() {
            if let Some(parent) = self.path.parent() {
                let _ = fs::create_dir_all(parent);
            }
            *guard = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.path)
                .ok();
        }
        if let Some(file) = guard.as_mut() {
            let _ = writeln!(file, "{line}");
        }
    }
}

impl<S> Layer<S> for OperationalDiagnosticsLayer
where
    S: Subscriber + for<'lookup> LookupSpan<'lookup>,
{
    fn on_new_span(
        &self,
        attrs: &tracing::span::Attributes<'_>,
        id: &tracing::Id,
        ctx: Context<'_, S>,
    ) {
        let Some(span) = ctx.span(id) else {
            return;
        };
        let mut fields = DiagnosticFields::default();
        attrs.record(&mut FieldVisitor::new(&mut fields.values));
        span.extensions_mut().insert(fields);
    }

    fn on_event(&self, event: &Event<'_>, ctx: Context<'_, S>) {
        let metadata = event.metadata();
        let mut fields = DiagnosticFields::default();
        if let Some(span) = ctx.event_span(event) {
            for scope_span in span.scope().from_root() {
                if let Some(span_fields) = scope_span.extensions().get::<DiagnosticFields>() {
                    fields.values.extend(span_fields.values.clone());
                }
            }
        }
        event.record(&mut FieldVisitor::new(&mut fields.values));
        let component = self.component_for(metadata.target(), &fields);
        let min_level = self.min_level_for(metadata.target(), &component);
        if !Self::should_capture(*metadata.level(), min_level) {
            return;
        }
        let persisted = PersistedDiagnosticEvent {
            timestamp_ms: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |duration| duration.as_millis()),
            level: metadata.level().to_string(),
            target: metadata.target().to_string(),
            component,
            message: fields.values.get("message").cloned(),
            fields: fields.values,
        };
        self.persist(&persisted);
    }
}

struct FieldVisitor<'a> {
    fields: &'a mut BTreeMap<String, String>,
}

impl<'a> FieldVisitor<'a> {
    const fn new(fields: &'a mut BTreeMap<String, String>) -> Self {
        Self { fields }
    }
}

impl tracing::field::Visit for FieldVisitor<'_> {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        self.fields
            .insert(field.name().to_string(), format!("{value:?}"));
    }

    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        self.fields
            .insert(field.name().to_string(), value.to_string());
    }

    fn record_i64(&mut self, field: &tracing::field::Field, value: i64) {
        self.fields
            .insert(field.name().to_string(), value.to_string());
    }

    fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
        self.fields
            .insert(field.name().to_string(), value.to_string());
    }

    fn record_bool(&mut self, field: &tracing::field::Field, value: bool) {
        self.fields
            .insert(field.name().to_string(), value.to_string());
    }
}

const fn level_rank(level: Level) -> u8 {
    match level {
        Level::ERROR => 1,
        Level::WARN => 2,
        Level::INFO => 3,
        Level::DEBUG => 4,
        Level::TRACE => 5,
    }
}

const fn config_level_rank(level: DiagnosticTraceLevel) -> u8 {
    match level {
        DiagnosticTraceLevel::Error => 1,
        DiagnosticTraceLevel::Warn => 2,
        DiagnosticTraceLevel::Info => 3,
        DiagnosticTraceLevel::Debug => 4,
        DiagnosticTraceLevel::Trace => 5,
    }
}
