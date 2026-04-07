//! Doc page functions — one per route, embedding markdown via `include_str!`.

use clap::CommandFactory;
use clap::builder::ValueHint;
use hyperchad::markdown::markdown_to_container;
use hyperchad::template::Containers;

use bmux_config::{BmuxConfig, ConfigDocSchema, ENV_OVERRIDE_DOCS, ThemeConfig};
use std::collections::BTreeMap;

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
    layout::docs_layout("/docs", None, &md(include_str!("../../../../../README.md")))
}

#[must_use]
pub fn installation() -> Containers {
    let readme = include_str!("../../../../../README.md");
    let content = extract_section(readme, "## Installation", Some("## "));
    layout::docs_layout("/docs/installation", Some("Installation"), &md(&content))
}

#[must_use]
pub fn quickstart() -> Containers {
    let readme = include_str!("../../../../../README.md");
    let content = extract_section(readme, "## Current CLI Workflow", Some("## "));
    layout::docs_layout("/docs/quickstart", Some("Quick Start"), &md(&content))
}

// ── Reference ───────────────────────────────────────────────────────────────

#[must_use]
pub fn cli() -> Containers {
    layout::docs_layout(
        "/docs/cli",
        Some("CLI Reference"),
        &md(&generate_cli_reference()),
    )
}

#[must_use]
pub fn playbooks() -> Containers {
    layout::docs_layout(
        "/docs/playbooks",
        None,
        &md(include_str!("../../../../../docs/playbooks.md")),
    )
}

#[must_use]
pub fn images() -> Containers {
    layout::docs_layout(
        "/docs/images",
        None,
        &md(include_str!("../../../../../docs/images.md")),
    )
}

#[must_use]
pub fn config() -> Containers {
    layout::docs_layout(
        "/docs/config",
        Some("Configuration"),
        &md(&generate_config_reference()),
    )
}

// ── Plugins ─────────────────────────────────────────────────────────────────

#[must_use]
pub fn plugins() -> Containers {
    layout::docs_layout(
        "/docs/plugins",
        None,
        &md(include_str!("../../../../../docs/plugins.md")),
    )
}

#[must_use]
pub fn plugin_sdk() -> Containers {
    layout::docs_layout(
        "/docs/plugin-sdk",
        None,
        &md(include_str!("../../../../../packages/plugin-sdk/README.md")),
    )
}

#[must_use]
pub fn plugin_example() -> Containers {
    layout::docs_layout(
        "/docs/plugin-example",
        None,
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
        None,
        &md(include_str!("../../../../../TESTING.md")),
    )
}

// ── Not Found ───────────────────────────────────────────────────────────────

#[must_use]
pub fn not_found() -> Containers {
    layout::docs_layout(
        "/not-found",
        Some("404"),
        &md("The page you are looking for does not exist."),
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
        if let Some(start) = start_idx
            && i > start
            && let Some(prefix) = end_prefix
            && line.starts_with(prefix)
            && *line != start_heading
        {
            end_idx = i;
            break;
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
         bmux resolves `bmux.toml` from the configured config directory candidate \
         chain. Use the path/env override table below to pin exact locations.\n\n\
         ---\n\n",
    );

    doc.push_str(&render_path_env_overrides_section());

    for section in root_config_sections() {
        doc.push_str(&render_section_with_fields(
            &section.section_name,
            &section.section_description,
            section.fields,
            section.defaults,
        ));

        if section.section_name == "appearance" {
            doc.push_str(&render_theme_file_reference());
        }

        if section.section_name == "status_bar" {
            doc.push_str(&render_status_preset_examples());
        }
    }

    doc
}

struct RootConfigSection {
    section_name: String,
    section_description: String,
    fields: Vec<RenderField>,
    defaults: BTreeMap<String, String>,
}

fn root_config_sections() -> Vec<RootConfigSection> {
    let mut sections = Vec::new();

    for field in BmuxConfig::field_docs() {
        if let Some(bmux_config_doc::NestedFieldDoc::Inline { fields, defaults }) = field.nested {
            let (flattened_fields, flattened_defaults) = flatten_field_docs(fields, defaults, "");
            sections.push(RootConfigSection {
                section_name: field.toml_key.to_string(),
                section_description: field.description.to_string(),
                fields: flattened_fields,
                defaults: flattened_defaults,
            });
        }
    }

    sections
}

fn render_theme_file_reference() -> String {
    let mut s = String::from(
        "## `themes/<name>.toml`\n\n\
         Named theme files live under the bmux config directory (for example, \
         `~/.config/bmux/themes/solarized.toml`) and are selected via \
         `appearance.theme = \"solarized\"`.\n\n\
         Keys below are top-level fields in the theme file.\n\n",
    );

    let (fields, defaults) =
        flatten_field_docs(ThemeConfig::field_docs(), ThemeConfig::default_values(), "");
    s.push_str(&render_fields_table(
        "theme_file",
        "theme",
        fields,
        defaults,
    ));
    s
}

fn render_path_env_overrides_section() -> String {
    let mut s = String::from(
        "## Path & Env Overrides\n\n\
         bmux supports environment-variable overrides for config/runtime/data \
         directories and recording storage.\n\n",
    );

    s.push_str("| Variable | Scope | Behavior |\n");
    s.push_str("|----------|-------|----------|\n");
    for override_doc in ENV_OVERRIDE_DOCS {
        let variable = format!("`{}`", escape_markdown_table_cell(override_doc.variable));
        let scope = escape_markdown_table_cell(override_doc.scope);
        let behavior = escape_markdown_table_cell(override_doc.description);
        s.push_str(&format!("| {variable} | {scope} | {behavior} |\n"));
    }
    s.push_str("\n---\n\n");

    s
}

fn render_status_preset_examples() -> String {
    let mut s = String::from("## Status Bar Preset Examples\n\n");
    s.push_str("### Tab Rail (recommended)\n\n");
    s.push_str(
        "```toml\n\
[status_bar]\n\
enabled = true\n\
preset = \"tab_rail\"\n\
tab_scope = \"all_contexts\"\n\
tab_order = \"stable\"\n\
max_tabs = 14\n\
tab_label_max_width = 22\n\
show_tab_index = true\n\
show_mode = true\n\
show_role = true\n\
show_follow = true\n\
show_hint = true\n\
hint_policy = \"scroll_only\"\n\
\n\
[status_bar.layout]\n\
density = \"cozy\"\n\
left_padding = 1\n\
right_padding = 1\n\
tab_gap = 1\n\
module_gap = 1\n\
overflow_style = \"arrows\"\n\
align_active = \"keep_visible\"\n\
\n\
[status_bar.style]\n\
separator_set = \"angled_segments\"\n\
prefer_unicode = true\n\
force_ascii = false\n\
dim_inactive = true\n\
bold_active = true\n\
underline_active = false\n\
```\n\n",
    );
    s.push_str("### Minimal\n\n");
    s.push_str(
        "```toml\n\
[status_bar]\n\
enabled = true\n\
preset = \"minimal\"\n\
tab_scope = \"all_contexts\"\n\
tab_order = \"stable\"\n\
show_tab_index = false\n\
show_follow = false\n\
show_hint = true\n\
hint_policy = \"scroll_only\"\n\
\n\
[status_bar.layout]\n\
density = \"compact\"\n\
tab_gap = 1\n\
module_gap = 1\n\
overflow_style = \"count\"\n\
align_active = \"keep_visible\"\n\
\n\
[status_bar.style]\n\
separator_set = \"plain\"\n\
prefer_unicode = false\n\
force_ascii = true\n\
dim_inactive = true\n\
bold_active = false\n\
underline_active = false\n\
```\n\n",
    );
    s.push_str("### Status Theme Override (partial)\n\n");
    s.push_str(
        "```toml\n\
[status_bar.theme]\n\
# Unset fields inherit from the global appearance theme defaults\n\
tab_active_bg = \"#7aa2f7\"\n\
tab_active_fg = \"#1a1b26\"\n\
tab_inactive_bg = \"#2a2f45\"\n\
module_bg = \"#343a55\"\n\
```\n\n",
    );
    s
}

fn dotted_key(prefix: &str, key: &str) -> String {
    if prefix.is_empty() {
        key.to_string()
    } else {
        format!("{prefix}.{key}")
    }
}

fn flatten_field_docs(
    fields: Vec<bmux_config::FieldDoc>,
    defaults: BTreeMap<String, String>,
    prefix: &str,
) -> (Vec<RenderField>, BTreeMap<String, String>) {
    let mut flattened_fields = Vec::new();
    let mut flattened_defaults = BTreeMap::new();

    for field in fields {
        let full_key = dotted_key(prefix, field.toml_key);

        match field.nested {
            Some(bmux_config_doc::NestedFieldDoc::Inline {
                fields: nested_fields,
                defaults: nested_defaults,
            }) => {
                let (child_fields, child_defaults) =
                    flatten_field_docs(nested_fields, nested_defaults, &full_key);
                flattened_fields.extend(child_fields);
                flattened_defaults.extend(child_defaults);
            }
            Some(bmux_config_doc::NestedFieldDoc::Map {
                key_placeholder,
                value_fields,
                value_defaults,
            }) => {
                let map_prefix = dotted_key(&full_key, key_placeholder);
                let (child_fields, child_defaults) =
                    flatten_field_docs(value_fields, value_defaults, &map_prefix);
                flattened_fields.extend(child_fields);
                flattened_defaults.extend(child_defaults);
            }
            Some(bmux_config_doc::NestedFieldDoc::List {
                index_placeholder,
                item_fields,
                item_defaults,
            }) => {
                let list_prefix = dotted_key(&full_key, index_placeholder);
                let (child_fields, child_defaults) =
                    flatten_field_docs(item_fields, item_defaults, &list_prefix);
                flattened_fields.extend(child_fields);
                flattened_defaults.extend(child_defaults);
            }
            None => {
                if let Some(default) = defaults.get(field.toml_key) {
                    flattened_defaults.insert(full_key.clone(), default.clone());
                }

                flattened_fields.push(RenderField::from_field_doc(full_key, field));
            }
        }
    }

    (flattened_fields, flattened_defaults)
}

struct RenderField {
    toml_key: String,
    type_display: String,
    description: String,
    enum_values: Option<Vec<String>>,
}

impl RenderField {
    fn from_field_doc(toml_key: String, value: bmux_config::FieldDoc) -> Self {
        Self {
            toml_key,
            type_display: value.type_display.to_string(),
            description: value.description.to_string(),
            enum_values: value
                .enum_values
                .map(|values| values.iter().map(|v| (*v).to_string()).collect()),
        }
    }
}

fn slugify_anchor_fragment(input: &str) -> String {
    let mut slug = String::new();
    let mut emitted_dash = false;

    for ch in input.chars() {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch.to_ascii_lowercase());
            emitted_dash = false;
        } else if !emitted_dash {
            slug.push('-');
            emitted_dash = true;
        }
    }

    let trimmed = slug.trim_matches('-').to_string();
    if trimmed.is_empty() {
        "default".to_string()
    } else {
        trimmed
    }
}

fn default_anchor_id(section_name: &str, toml_key: &str) -> String {
    format!(
        "default-{}-{}",
        slugify_anchor_fragment(section_name),
        slugify_anchor_fragment(toml_key)
    )
}

fn escape_markdown_table_cell(input: &str) -> String {
    input
        .replace('\\', "\\\\")
        .replace('|', "\\|")
        .replace('\n', "<br>")
}

fn escape_inline_code(input: &str) -> String {
    input.replace('`', "\\`")
}

fn render_section_with_fields(
    section_name: &str,
    section_description: &str,
    fields: Vec<RenderField>,
    defaults: BTreeMap<String, String>,
) -> String {
    let mut s = format!("## `[{section_name}]`\n\n{section_description}\n\n",);
    s.push_str(&render_fields_table(
        section_name,
        section_name,
        fields,
        defaults,
    ));
    s
}

fn render_fields_table(
    anchor_namespace: &str,
    default_section_name: &str,
    fields: Vec<RenderField>,
    defaults: BTreeMap<String, String>,
) -> String {
    let mut s = String::new();

    s.push_str("| Option | Type | Default | Description |\n");
    s.push_str("|--------|------|---------|-------------|\n");

    // Collect table-typed fields with non-empty defaults for rendering after
    // the table as collapsible code blocks.
    let mut deferred_tables: Vec<(String, String, bool, String)> = Vec::new();

    for field in fields {
        let raw_default = defaults.get(&field.toml_key).map(String::as_str);
        let is_table = field.type_display == "table";
        let is_multiline_default = raw_default.is_some_and(|v| v.contains('\n'));

        let default_val = if is_table || is_multiline_default {
            match raw_default {
                Some(v) if v.is_empty() || v == "{}" => "*(empty)*".to_string(),
                Some(v) => {
                    let anchor_id = default_anchor_id(anchor_namespace, &field.toml_key);
                    deferred_tables.push((
                        field.toml_key.clone(),
                        v.to_string(),
                        is_table,
                        anchor_id.clone(),
                    ));
                    format!("[*(see defaults below)*](#{anchor_id})")
                }
                None => "*(empty)*".to_string(),
            }
        } else {
            match raw_default {
                Some("") => "*(empty)*".to_string(),
                Some(v) => format!("`{}`", escape_inline_code(v)),
                None => "—".to_string(),
            }
        };

        let type_info = match &field.enum_values {
            Some(vals) if !vals.is_empty() => {
                let joined = vals.join("`, `");
                format!("{} (`{joined}`)", field.type_display)
            }
            None => field.type_display.to_string(),
            Some(_) => field.type_display.to_string(),
        };

        let escaped_option = escape_markdown_table_cell(&field.toml_key);
        let escaped_type = escape_markdown_table_cell(&type_info);
        let escaped_default = if default_val.starts_with("[*(see defaults below)*](#") {
            default_val
        } else {
            escape_markdown_table_cell(&default_val)
        };
        let escaped_description = escape_markdown_table_cell(&field.description);

        s.push_str(&format!(
            "| `{}` | {} | {} | {} |\n",
            escaped_option, escaped_type, escaped_default, escaped_description
        ));
    }

    s.push('\n');

    // Render deferred table defaults as code blocks with anchor IDs
    for (toml_key, default_val, is_table, anchor_id) in &deferred_tables {
        let heading = if *is_table {
            format!("Default `{toml_key}` bindings")
        } else {
            format!("Default `{toml_key}` value")
        };
        let body = if *is_table {
            format!("[{default_section_name}.{toml_key}]\n{default_val}")
        } else {
            format!("[{default_section_name}]\n{toml_key} = {default_val}")
        };
        s.push_str(&format!(
            "<div id=\"{anchor_id}\"></div>\n\n\
             ### {heading}\n\n\
             ```toml\n{body}\n```\n\n"
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
            if num_vals.is_some_and(|r| r.min_values() > 0 || r.max_values() > 0) {
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

            // Determine values/types from clap metadata first, then value-name heuristics.
            let possible = arg_possible_values(flag);
            let repeatable = arg_is_repeatable(flag);

            let values_display = if !is_bool_possible_values(&possible) {
                possible
                    .iter()
                    .map(|v| format!("`{}`", escape_inline_code(v)))
                    .collect::<Vec<_>>()
                    .join(", ")
            } else {
                infer_arg_value_type(flag)
            };
            let values_display = if repeatable {
                format!("{values_display} (repeatable)")
            } else {
                values_display
            };

            let default = flag
                .get_default_values()
                .iter()
                .map(|v| v.to_string_lossy().to_string())
                .collect::<Vec<_>>()
                .join(", ");
            let default_display = if !default.is_empty() {
                format!("`{}`", escape_inline_code(&default))
            } else if matches!(flag.get_action(), clap::ArgAction::SetTrue) {
                "`false`".to_string()
            } else if matches!(flag.get_action(), clap::ArgAction::SetFalse) {
                "`true`".to_string()
            } else {
                String::new()
            };

            let escaped_flag = escape_markdown_table_cell(&flag_str);
            let escaped_desc = escape_markdown_table_cell(&desc);
            let escaped_values = escape_markdown_table_cell(&values_display);
            let escaped_default = escape_markdown_table_cell(&default_display);

            doc.push_str(&format!(
                "| `{escaped_flag}` | {escaped_desc} | {escaped_values} | {escaped_default} |\n"
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

fn arg_possible_values(flag: &clap::Arg) -> Vec<String> {
    flag.get_possible_values()
        .iter()
        .filter(|v| !v.is_hide_set())
        .map(|v| v.get_name().to_string())
        .collect()
}

fn is_bool_possible_values(possible: &[String]) -> bool {
    possible.is_empty() || possible == ["true", "false"] || possible == ["false", "true"]
}

fn arg_is_repeatable(flag: &clap::Arg) -> bool {
    matches!(flag.get_action(), clap::ArgAction::Append)
        || flag
            .get_num_args()
            .is_some_and(|range| range.max_values() > 1 || range.min_values() > 1)
}

/// Infer a human-readable type name from clap arg metadata, falling back to
/// value-name heuristics when clap metadata is non-specific.
fn infer_arg_value_type(flag: &clap::Arg) -> String {
    match flag.get_action() {
        clap::ArgAction::SetTrue | clap::ArgAction::SetFalse => return "boolean".to_string(),
        clap::ArgAction::Count => return "integer".to_string(),
        _ => {}
    }

    match flag.get_value_hint() {
        ValueHint::AnyPath
        | ValueHint::FilePath
        | ValueHint::DirPath
        | ValueHint::ExecutablePath => return "path".to_string(),
        ValueHint::CommandString
        | ValueHint::CommandName
        | ValueHint::CommandWithArguments
        | ValueHint::Username
        | ValueHint::Hostname
        | ValueHint::Url
        | ValueHint::EmailAddress
        | ValueHint::Other => return "string".to_string(),
        ValueHint::Unknown => {}
        _ => {}
    }

    let val_name = flag
        .get_value_names()
        .and_then(|names| names.first())
        .map(|name| name.as_str().to_lowercase())
        .unwrap_or_default();

    infer_value_type_from_name(&val_name)
}

/// Infer a human-readable type name from a clap value name.
fn infer_value_type_from_name(val_name: &str) -> String {
    // Integer-like value names
    if matches!(
        val_name,
        "lines"
            | "limit"
            | "fps"
            | "n"
            | "days"
            | "secs"
            | "seconds"
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

#[cfg(test)]
mod tests {
    use super::{generate_cli_reference, generate_config_reference};
    use bmux_config::{BmuxConfig, ConfigDocSchema};

    #[test]
    fn config_reference_includes_connections_nested_dotted_keys() {
        let doc = generate_config_reference();

        assert!(doc.contains("## `[connections]`"));
        assert!(doc.contains("targets.<name>.transport"));
        assert!(doc.contains("iroh_ssh_access.enabled"));
        assert!(doc.contains("iroh_ssh_access.allowlist.<fingerprint>.public_key"));
        assert!(doc.contains("iroh_ssh_access.allowlist.<fingerprint>.added_at_unix"));
    }

    #[test]
    fn config_reference_keeps_existing_nested_sections_dotted() {
        let doc = generate_config_reference();

        assert!(doc.contains("mouse.enabled"));
        assert!(doc.contains("images.decode_mode"));
        assert!(doc.contains("compression.remote"));
        assert!(doc.contains("export.cursor"));
        assert!(doc.contains("layout.density"));
        assert!(doc.contains("style.separator_set"));
        assert!(doc.contains("theme.tab_active_bg"));
        assert!(doc.contains("routing.conflict_mode"));
        assert!(doc.contains("routing.required_paths.<index>.path"));
        assert!(doc.contains("routing.required_namespaces.<index>.namespace"));
        assert!(doc.contains("rolling_event_kinds"));
        assert!(doc.contains("pane_input_raw"));
    }

    #[test]
    fn config_reference_documents_theme_file_schema() {
        let doc = generate_config_reference();

        assert!(doc.contains("## `themes/<name>.toml`"));
        assert!(doc.contains("border.active"));
        assert!(doc.contains("status.mode_indicator"));
    }

    #[test]
    fn config_reference_renders_all_root_sections() {
        let doc = generate_config_reference();

        for field in BmuxConfig::field_docs() {
            let heading = format!("## `[{}]`", field.toml_key);
            assert!(doc.contains(&heading), "missing section heading: {heading}");
        }
    }

    #[test]
    fn config_reference_documents_env_overrides() {
        let doc = generate_config_reference();

        assert!(doc.contains("## Path & Env Overrides"));
        assert!(doc.contains("BMUX_CONFIG_DIR"));
        assert!(doc.contains("BMUX_RUNTIME_NAME"));
        assert!(doc.contains("BMUX_RECORDINGS_DIR"));
    }

    #[test]
    fn cli_reference_hides_internal_flags_and_renders_enums_types_and_repeatability() {
        let doc = generate_cli_reference();

        assert!(!doc.contains("core-builtins-only"));
        assert!(doc.contains("--record-profile"));
        assert!(doc.contains("`full`, `functional`, `visual`"));

        let recordings_dir_line = doc
            .lines()
            .find(|line| line.contains("--recordings-dir"))
            .expect("missing --recordings-dir line");
        assert!(recordings_dir_line.contains("path"));

        let record_event_kind_line = doc
            .lines()
            .find(|line| line.contains("--record-event-kind"))
            .expect("missing --record-event-kind line");
        assert!(record_event_kind_line.contains("repeatable"));
    }
}
