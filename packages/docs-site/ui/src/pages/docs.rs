//! Doc page functions — one per route, embedding markdown via `include_str!`.

use clap::CommandFactory;
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
    layout::docs_layout("/docs/cli", "CLI Reference", &md(&generate_cli_reference()))
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
    let section_name = T::section_name();
    let defaults = T::default_values();
    let mut s = format!(
        "## `[{}]`\n\n{}\n\n",
        section_name,
        T::section_description()
    );

    s.push_str("| Option | Type | Default | Description |\n");
    s.push_str("|--------|------|---------|-------------|\n");

    // Collect table-typed fields with non-empty defaults for rendering after
    // the table as collapsible code blocks.
    let mut deferred_tables: Vec<(&str, &str)> = Vec::new();

    for field in T::field_docs() {
        let raw_default = defaults.get(field.toml_key).map(String::as_str);
        let is_table = field.type_display == "table";

        let default_val = if is_table {
            match raw_default {
                Some(v) if v.is_empty() || v == "{}" => "*(empty)*".to_string(),
                Some(v) => {
                    deferred_tables.push((field.toml_key, v));
                    format!(
                        "[*(see defaults below)*](#default-{section_name}-{})",
                        field.toml_key
                    )
                }
                None => "*(empty)*".to_string(),
            }
        } else {
            match raw_default {
                Some(v) if v.is_empty() => "*(empty)*".to_string(),
                Some(v) => format!("`{v}`"),
                None => "—".to_string(),
            }
        };

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

    s.push('\n');

    // Render deferred table defaults as code blocks with anchor IDs
    for (toml_key, default_val) in &deferred_tables {
        s.push_str(&format!(
            "<div id=\"default-{section_name}-{toml_key}\"></div>\n\n\
             ### Default `{toml_key}` bindings\n\n\
             ```toml\n[{section_name}.{toml_key}]\n{default_val}\n```\n\n"
        ));
    }

    s.push_str("---\n\n");
    s
}

// ── CLI reference generation from clap Command tree ─────────────────────────

/// Generate the full CLI reference markdown by walking the clap `Command` tree
/// from `bmux_cli_schema::Cli`. Descriptions come from `///` doc comments on
/// the derive structs and are always in sync with the actual binary.
fn generate_cli_reference() -> String {
    let cmd = bmux_cli_schema::Cli::command();
    let mut doc = String::new();
    render_command(&mut doc, &cmd, &["bmux"], 0);
    doc
}

fn render_command(doc: &mut String, cmd: &clap::Command, path: &[&str], depth: usize) {
    let full_path = path.join(" ");

    // Heading level: depth 0 = ##, depth 1 = ###, depth 2+ = ####
    let heading = match depth {
        0 => "##",
        1 => "###",
        _ => "####",
    };
    doc.push_str(&format!("{heading} `{full_path}`\n\n"));

    // Description
    if let Some(about) = cmd.get_about() {
        doc.push_str(&format!("{about}\n\n"));
    }

    // Collect visible arguments (skip hidden ones and positionals handled separately)
    let options: Vec<_> = cmd
        .get_arguments()
        .filter(|a| !a.is_hide_set() && a.get_id() != "help" && a.get_id() != "version")
        .collect();

    // Split into positional args and flags/options
    let positionals: Vec<_> = options.iter().filter(|a| a.is_positional()).collect();
    let flags: Vec<_> = options.iter().filter(|a| !a.is_positional()).collect();

    // Render usage line
    if !positionals.is_empty() || !flags.is_empty() {
        let mut usage = format!("`{full_path}");
        for pos in &positionals {
            let name = pos.get_id().as_str().to_uppercase();
            if pos.is_required_set() {
                usage.push_str(&format!(" <{name}>"));
            } else {
                usage.push_str(&format!(" [{name}]"));
            }
        }
        if !flags.is_empty() {
            usage.push_str(" [OPTIONS]");
        }
        usage.push('`');
        doc.push_str(&format!("**Usage:** {usage}\n\n"));
    }

    // Render positional arguments
    if !positionals.is_empty() {
        doc.push_str("**Arguments:**\n\n");
        for pos in &positionals {
            let name = pos.get_id().as_str().to_uppercase();
            let desc = pos.get_help().map(|h| h.to_string()).unwrap_or_default();
            let required = if pos.is_required_set() {
                " *(required)*"
            } else {
                ""
            };
            doc.push_str(&format!("- `<{name}>`{required} — {desc}\n"));
        }
        doc.push('\n');
    }

    // Render flags/options table
    if !flags.is_empty() {
        doc.push_str("| Flag | Description | Values | Default |\n");
        doc.push_str("|------|-------------|--------|--------|\n");

        for flag in &flags {
            let mut flag_str = String::new();
            if let Some(short) = flag.get_short() {
                flag_str.push_str(&format!("-{short}, "));
            }
            if let Some(long) = flag.get_long() {
                flag_str.push_str(&format!("--{long}"));
            }

            // Show value name for non-bool flags
            let num_vals = flag.get_num_args();
            if num_vals.map_or(false, |r| r.min_values() > 0 || r.max_values() > 0) {
                let val_names = flag.get_value_names().unwrap_or_default();
                if !val_names.is_empty() {
                    let names = val_names
                        .iter()
                        .map(|n| n.as_str())
                        .collect::<Vec<_>>()
                        .join(" ");
                    flag_str.push_str(&format!(" <{names}>"));
                }
            }

            let desc = flag.get_help().map(|h| h.to_string()).unwrap_or_default();

            // Determine the Values column content:
            // - ValueEnum args: show the valid choices
            // - Boolean flags: show "boolean"
            // - Otherwise: infer type from value name heuristic
            let possible: Vec<String> = flag
                .get_possible_values()
                .iter()
                .filter(|v| !v.is_hide_set())
                .map(|v| v.get_name().to_string())
                .collect();

            let is_bool_values = possible.is_empty()
                || possible == ["true", "false"]
                || possible == ["false", "true"];

            let values_display = if !is_bool_values {
                // Real ValueEnum — show the choices
                possible
                    .iter()
                    .map(|v| format!("`{v}`"))
                    .collect::<Vec<_>>()
                    .join(", ")
            } else if matches!(
                flag.get_action(),
                clap::ArgAction::SetTrue | clap::ArgAction::SetFalse | clap::ArgAction::Count
            ) {
                // Boolean flag (SetTrue/SetFalse) or count flag (-vvv)
                "boolean".to_string()
            } else {
                // Takes a value — infer type from value name
                let val_name = flag
                    .get_value_names()
                    .and_then(|n| n.first())
                    .map(|n| n.as_str().to_lowercase())
                    .unwrap_or_default();
                infer_value_type(&val_name)
            };

            let default = flag
                .get_default_values()
                .iter()
                .map(|v| v.to_string_lossy().to_string())
                .collect::<Vec<_>>()
                .join(", ");
            let default_display = if !default.is_empty() {
                format!("`{default}`")
            } else if matches!(flag.get_action(), clap::ArgAction::SetTrue) {
                "`false`".to_string()
            } else if matches!(flag.get_action(), clap::ArgAction::SetFalse) {
                "`true`".to_string()
            } else {
                String::new()
            };

            doc.push_str(&format!(
                "| `{flag_str}` | {desc} | {values_display} | {default_display} |\n"
            ));
        }
        doc.push('\n');
    }

    // Collect visible subcommands
    let subcommands: Vec<_> = cmd.get_subcommands().filter(|s| !s.is_hide_set()).collect();

    // List subcommands with descriptions
    if !subcommands.is_empty() && depth < 2 {
        doc.push_str("**Subcommands:**\n\n");
        for sub in &subcommands {
            let desc = sub.get_about().map(|a| a.to_string()).unwrap_or_default();
            doc.push_str(&format!("- `{}` — {desc}\n", sub.get_name()));
        }
        doc.push('\n');
    }

    doc.push_str("---\n\n");

    // Recurse into subcommands
    for sub in subcommands {
        let mut sub_path = path.to_vec();
        sub_path.push(sub.get_name());
        render_command(doc, sub, &sub_path, depth + 1);
    }
}

/// Infer a human-readable type name from a clap value name.
fn infer_value_type(val_name: &str) -> String {
    // Integer-like value names
    if matches!(
        val_name,
        "lines"
            | "limit"
            | "fps"
            | "n"
            | "days"
            | "secs"
            | "ms"
            | "timeout"
            | "threshold"
            | "px"
            | "cell_size"
            | "max_frames"
            | "max_duration"
            | "max_verify_duration"
            | "verify_start_timeout"
            | "older_than"
            | "timing_threshold"
            | "trace_limit"
            | "trace_pane"
    ) {
        return "integer".to_string();
    }

    // Float-like value names
    if matches!(val_name, "speed" | "line_height" | "font_size") {
        return "number".to_string();
    }

    // Path-like value names
    if val_name.contains("path")
        || val_name.contains("file")
        || val_name.contains("dir")
        || val_name == "output"
        || val_name == "source"
    {
        return "path".to_string();
    }

    "string".to_string()
}
