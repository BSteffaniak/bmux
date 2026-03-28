//! Doc page functions — one per route, embedding markdown via `include_str!`.

use hyperchad::markdown::markdown_to_container;
use hyperchad::template::Containers;

use bmux_config::{
    AppearanceConfig, BehaviorConfig, ConfigDocSchema, GeneralConfig, KeyBindingConfig,
    MultiClientConfig, PluginConfig, RecordingConfig, StatusBarConfig,
};

use crate::layout;
use crate::theme;

/// Helper: convert markdown string to dark-themed containers.
fn md(markdown: &str) -> Containers {
    let mut container = markdown_to_container(markdown);
    theme::apply_dark_theme(&mut container);
    vec![container]
}

// ── Getting Started ─────────────────────────────────────────────────────────

#[must_use]
pub fn overview() -> Containers {
    layout::docs_layout(
        "/docs",
        "Overview",
        &md(include_str!("../../../../../README.md")),
    )
}

#[must_use]
pub fn installation() -> Containers {
    let readme = include_str!("../../../../../README.md");
    let content = extract_section(readme, "## Installation", Some("## "));
    layout::docs_layout("/docs/installation", "Installation", &md(&content))
}

#[must_use]
pub fn quickstart() -> Containers {
    let readme = include_str!("../../../../../README.md");
    let content = extract_section(readme, "## Current CLI Workflow", Some("## "));
    layout::docs_layout("/docs/quickstart", "Quick Start", &md(&content))
}

// ── Reference ───────────────────────────────────────────────────────────────

#[must_use]
pub fn cli() -> Containers {
    layout::docs_layout(
        "/docs/cli",
        "CLI Reference",
        &md(include_str!("../../../../../packages/cli/README.md")),
    )
}

#[must_use]
pub fn playbooks() -> Containers {
    layout::docs_layout(
        "/docs/playbooks",
        "Playbooks",
        &md(include_str!("../../../../../docs/playbooks.md")),
    )
}

#[must_use]
pub fn config() -> Containers {
    layout::docs_layout(
        "/docs/config",
        "Configuration",
        &md(&generate_config_reference()),
    )
}

// ── Plugins ─────────────────────────────────────────────────────────────────

#[must_use]
pub fn plugins() -> Containers {
    layout::docs_layout(
        "/docs/plugins",
        "Plugin Architecture",
        &md(include_str!("../../../../../docs/plugins.md")),
    )
}

#[must_use]
pub fn plugin_sdk() -> Containers {
    layout::docs_layout(
        "/docs/plugin-sdk",
        "Plugin SDK",
        &md(include_str!("../../../../../packages/plugin/README.md")),
    )
}

#[must_use]
pub fn plugin_example() -> Containers {
    layout::docs_layout(
        "/docs/plugin-example",
        "Example Plugin",
        &md(include_str!(
            "../../../../../examples/native-plugin/README.md"
        )),
    )
}

// ── Development ─────────────────────────────────────────────────────────────

#[must_use]
pub fn testing() -> Containers {
    layout::docs_layout(
        "/docs/testing",
        "Testing",
        &md(include_str!("../../../../../TESTING.md")),
    )
}

// ── Not Found ───────────────────────────────────────────────────────────────

#[must_use]
pub fn not_found() -> Containers {
    layout::docs_layout(
        "/not-found",
        "404",
        &md("# Page not found\n\nThe page you are looking for does not exist."),
    )
}

// ── Helpers ─────────────────────────────────────────────────────────────────

/// Extract a section from a markdown document by heading.
fn extract_section(markdown: &str, start_heading: &str, end_prefix: Option<&str>) -> String {
    let lines: Vec<&str> = markdown.lines().collect();
    let mut start_idx = None;
    let mut end_idx = lines.len();

    for (i, line) in lines.iter().enumerate() {
        if *line == start_heading {
            start_idx = Some(i + 1);
            continue;
        }
        if let Some(start) = start_idx {
            if i > start {
                if let Some(prefix) = end_prefix {
                    if line.starts_with(prefix) && *line != start_heading {
                        end_idx = i;
                        break;
                    }
                }
            }
        }
    }

    match start_idx {
        Some(start) => lines[start..end_idx].join("\n"),
        None => markdown.to_string(),
    }
}

// ── Config reference generation from schema ─────────────────────────────────

/// Generate the full configuration reference markdown from the `ConfigDocSchema`
/// trait implementations on each config struct. This is always in sync with the
/// actual code because descriptions come from doc comments and defaults come
/// from `Default::default()` serialized at compile time.
fn generate_config_reference() -> String {
    let mut doc = String::from(
        "bmux is configured via a `bmux.toml` file. If no config file exists, \
         bmux uses sensible defaults for all options.\n\n\
         ## Config File Location\n\n\
         bmux looks for `bmux.toml` in the standard XDG config directory:\n\n\
         ```\n~/.config/bmux/bmux.toml\n```\n\n\
         ---\n\n",
    );

    doc.push_str(&render_section::<GeneralConfig>());
    doc.push_str(&render_section::<AppearanceConfig>());
    doc.push_str(&render_section::<BehaviorConfig>());
    doc.push_str(&render_section::<MultiClientConfig>());
    doc.push_str(&render_section::<KeyBindingConfig>());
    doc.push_str(&render_section::<PluginConfig>());
    doc.push_str(&render_section::<StatusBarConfig>());
    doc.push_str(&render_section::<RecordingConfig>());

    doc
}

fn render_section<T: ConfigDocSchema>() -> String {
    let defaults = T::default_values();
    let mut s = format!(
        "## `[{}]`\n\n{}\n\n",
        T::section_name(),
        T::section_description()
    );

    s.push_str("| Option | Type | Default | Description |\n");
    s.push_str("|--------|------|---------|-------------|\n");

    for field in T::field_docs() {
        let default_val = defaults
            .get(field.toml_key)
            .map(|v| {
                if v.is_empty() {
                    "*(empty)*".to_string()
                } else {
                    format!("`{v}`")
                }
            })
            .unwrap_or_else(|| "—".to_string());

        let type_info = match field.enum_values {
            Some(vals) => {
                let joined = vals.join("`, `");
                format!("{} (`{joined}`)", field.type_display)
            }
            None => field.type_display.to_string(),
        };

        s.push_str(&format!(
            "| `{}` | {} | {} | {} |\n",
            field.toml_key, type_info, default_val, field.description
        ));
    }

    s.push_str("\n---\n\n");
    s
}
