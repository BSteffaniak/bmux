//! Generation helpers for documentation pages.
//!
//! The per-page renderers and the routing registry live in
//! [`crate::doc_pages`]. This module retains only the logic that generates
//! markdown from in-process data (clap CLI metadata and the config schema)
//! plus small helpers shared by those generators.

use clap::CommandFactory;
use clap::builder::ValueHint;

use bmux_config::{BmuxConfig, ConfigDocSchema, ENV_OVERRIDE_DOCS, ThemeConfig};
use std::collections::BTreeMap;

// ── Helpers ─────────────────────────────────────────────────────────────────

/// Extract a section from a markdown document by heading.
pub(crate) fn extract_section_for(
    markdown: &str,
    start_heading: &str,
    end_prefix: Option<&str>,
) -> String {
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
pub(crate) fn generate_config_reference() -> String {
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
pub(crate) fn generate_cli_reference() -> String {
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
    use bmux_cli::playbook;
    use bmux_cli_schema::Cli;
    use bmux_config::{BmuxConfig, ConfigDocSchema};
    use clap::Parser;
    use serde::Serialize;
    use std::collections::{BTreeMap, BTreeSet};
    use std::env;
    use std::fs;
    use std::path::{Path, PathBuf};

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

    #[test]
    fn markdown_section_headings_used_by_docs_routes_exist() {
        let readme = include_str!("../../../../../README.md");
        assert!(
            readme.contains("## Installation"),
            "README heading '## Installation' missing"
        );
        assert!(
            readme.contains("## Current CLI Workflow"),
            "README heading '## Current CLI Workflow' missing"
        );
    }

    #[test]
    fn markdown_opt_in_snippets_are_valid() {
        let mut failures = Vec::new();
        let key_patterns = collect_config_key_patterns();

        for file in markdown_sources() {
            let content = fs::read_to_string(&file)
                .unwrap_or_else(|err| panic!("failed to read {}: {err}", file.display()));

            for block in parse_fenced_blocks(&content) {
                match block.language.as_str() {
                    "bmux-cli" => {
                        if let Err(error) = validate_cli_block(&block.content) {
                            failures.push(format!(
                                "{}:{} [{}] {error}",
                                file.display(),
                                block.start_line,
                                block.language
                            ));
                        }
                    }
                    "bmux-playbook" => {
                        if let Err(error) = validate_playbook_block(&block.content) {
                            failures.push(format!(
                                "{}:{} [{}] {error}",
                                file.display(),
                                block.start_line,
                                block.language
                            ));
                        }
                    }
                    "bmux-config" => {
                        if let Err(error) = validate_config_block(&block.content, &key_patterns) {
                            failures.push(format!(
                                "{}:{} [{}] {error}",
                                file.display(),
                                block.start_line,
                                block.language
                            ));
                        }
                    }
                    _ => {}
                }
            }
        }

        if failures.is_empty() {
            return;
        }

        let report = failures.join("\n");
        panic!("docs snippet validation failures:\n{report}");
    }

    #[test]
    fn markdown_snippet_coverage_report() {
        let coverage = collect_snippet_coverage();
        let report = render_coverage_report(&coverage);

        eprintln!("{report}");
        write_coverage_artifacts(&coverage, &report);

        assert!(
            coverage.total_opt_in > 0,
            "expected at least one opt-in snippet block; add a fenced block with one of: bmux-cli, bmux-playbook, bmux-config"
        );
    }

    #[derive(Debug, Serialize)]
    struct SnippetCoverageReport {
        total_fenced: usize,
        total_opt_in: usize,
        opt_in_percent: f64,
        tag_counts: BTreeMap<String, usize>,
        files: Vec<SnippetCoverageFileRow>,
    }

    #[derive(Debug, Serialize)]
    struct SnippetCoverageFileRow {
        path: String,
        fenced: usize,
        opt_in: usize,
    }

    #[derive(Debug)]
    struct FencedBlock {
        language: String,
        content: String,
        start_line: usize,
    }

    fn parse_fenced_blocks(markdown: &str) -> Vec<FencedBlock> {
        let mut blocks = Vec::new();
        let mut in_block = false;
        let mut block_language = String::new();
        let mut block_start = 0;
        let mut lines = Vec::new();

        for (line_index, raw_line) in markdown.lines().enumerate() {
            let line_number = line_index + 1;
            let trimmed = raw_line.trim_start();

            if !in_block {
                if let Some(rest) = trimmed.strip_prefix("```") {
                    in_block = true;
                    block_language = rest
                        .split_whitespace()
                        .next()
                        .unwrap_or_default()
                        .to_string();
                    block_start = line_number;
                    lines.clear();
                }
                continue;
            }

            if trimmed.starts_with("```") {
                blocks.push(FencedBlock {
                    language: block_language.clone(),
                    content: lines.join("\n"),
                    start_line: block_start,
                });
                in_block = false;
                block_language.clear();
                lines.clear();
                continue;
            }

            lines.push(raw_line.to_string());
        }

        blocks
    }

    fn is_opt_in_tag(language: &str) -> bool {
        matches!(language, "bmux-cli" | "bmux-playbook" | "bmux-config")
    }

    fn collect_snippet_coverage() -> SnippetCoverageReport {
        let mut file_rows = Vec::new();
        let mut total_fenced = 0usize;
        let mut total_opt_in = 0usize;
        let mut tag_counts: BTreeMap<String, usize> = BTreeMap::new();
        let workspace_root = workspace_root();

        for file in markdown_sources() {
            let content = fs::read_to_string(&file)
                .unwrap_or_else(|err| panic!("failed to read {}: {err}", file.display()));
            let blocks = parse_fenced_blocks(&content);
            let fenced = blocks.len();
            let opt_in = blocks
                .iter()
                .filter(|block| is_opt_in_tag(&block.language))
                .count();

            for block in &blocks {
                if is_opt_in_tag(&block.language) {
                    *tag_counts.entry(block.language.clone()).or_default() += 1;
                }
            }

            total_fenced += fenced;
            total_opt_in += opt_in;

            if fenced > 0 || opt_in > 0 {
                let relative_path = file
                    .strip_prefix(&workspace_root)
                    .unwrap_or(&file)
                    .display()
                    .to_string();
                file_rows.push(SnippetCoverageFileRow {
                    path: relative_path,
                    fenced,
                    opt_in,
                });
            }
        }

        file_rows.sort_by(|left, right| left.path.cmp(&right.path));

        let opt_in_percent = if total_fenced == 0 {
            0.0
        } else {
            (total_opt_in as f64 / total_fenced as f64) * 100.0
        };

        SnippetCoverageReport {
            total_fenced,
            total_opt_in,
            opt_in_percent,
            tag_counts,
            files: file_rows,
        }
    }

    fn render_coverage_report(coverage: &SnippetCoverageReport) -> String {
        let mut report = String::new();
        report.push_str("docs snippet coverage report\n");
        report.push_str(&format!(
            "opt-in validated: {}/{} ({:.1}%)\n",
            coverage.total_opt_in, coverage.total_fenced, coverage.opt_in_percent
        ));
        report.push_str("by tag:\n");
        for (tag, count) in &coverage.tag_counts {
            report.push_str(&format!("  - {tag}: {count}\n"));
        }
        report.push_str("by file:\n");
        for row in &coverage.files {
            report.push_str(&format!(
                "  - {}: {}/{}\n",
                row.path, row.opt_in, row.fenced
            ));
        }
        report
    }

    fn write_coverage_artifacts(coverage: &SnippetCoverageReport, markdown_report: &str) {
        let Ok(raw_output_dir) = env::var("BMUX_DOCS_COVERAGE_OUTPUT_DIR") else {
            return;
        };
        if raw_output_dir.trim().is_empty() {
            return;
        }

        let output_dir = PathBuf::from(raw_output_dir);
        if let Err(err) = fs::create_dir_all(&output_dir) {
            eprintln!(
                "warning: failed to create docs coverage output dir {}: {err}",
                output_dir.display()
            );
            return;
        }

        let markdown_path = output_dir.join("docs-snippet-coverage.md");
        if let Err(err) = fs::write(&markdown_path, markdown_report) {
            eprintln!(
                "warning: failed to write markdown coverage report {}: {err}",
                markdown_path.display()
            );
        }

        let json_path = output_dir.join("docs-snippet-coverage.json");
        match serde_json::to_string_pretty(coverage) {
            Ok(json) => {
                if let Err(err) = fs::write(&json_path, json) {
                    eprintln!(
                        "warning: failed to write json coverage report {}: {err}",
                        json_path.display()
                    );
                }
            }
            Err(err) => {
                eprintln!("warning: failed to serialize docs coverage report: {err}");
            }
        }
    }

    fn validate_cli_block(content: &str) -> Result<(), String> {
        for raw_line in content.lines() {
            let line = raw_line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }

            let command = line
                .strip_prefix("$ ")
                .or_else(|| line.strip_prefix("# "))
                .unwrap_or(line);

            if !command.starts_with("bmux") {
                return Err(format!("expected command to start with 'bmux': {command}"));
            }

            let args = shell_split(command)?;
            Cli::try_parse_from(args).map_err(|err| err.to_string())?;
        }

        Ok(())
    }

    fn validate_playbook_block(content: &str) -> Result<(), String> {
        if content.contains("[[step]]") || content.contains("[playbook]") {
            let (playbook, _includes) =
                playbook::parse_toml::parse_toml(content).map_err(|err| err.to_string())?;
            let errors = playbook::validate(&playbook, false);
            if errors.is_empty() {
                return Ok(());
            }
            return Err(errors.join("; "));
        }

        let (playbook, _includes) = playbook::parse_dsl::parse_dsl(content)
            .map_err(|err| format!("playbook parse error: {err}"))?;
        let errors = playbook::validate(&playbook, false);
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors.join("; "))
        }
    }

    fn validate_config_block(content: &str, key_patterns: &BTreeSet<String>) -> Result<(), String> {
        let value: toml::Value = toml::from_str(content)
            .map_err(|err| format!("failed to parse TOML snippet: {err}"))?;

        let _parsed: BmuxConfig = toml::from_str(content)
            .map_err(|err| format!("failed to deserialize BmuxConfig snippet: {err}"))?;

        let mut snippet_keys = Vec::new();
        collect_toml_leaf_keys(&value, "", &mut snippet_keys);

        for key in snippet_keys {
            if !key_matches_patterns(&key, key_patterns) {
                return Err(format!("unknown config key: {key}"));
            }
        }

        Ok(())
    }

    fn collect_toml_leaf_keys(value: &toml::Value, prefix: &str, out: &mut Vec<String>) {
        match value {
            toml::Value::Table(table) => {
                for (key, child) in table {
                    let next = if prefix.is_empty() {
                        key.to_string()
                    } else {
                        format!("{prefix}.{key}")
                    };
                    collect_toml_leaf_keys(child, &next, out);
                }
            }
            toml::Value::Array(values) => {
                if values.iter().all(toml::Value::is_table) {
                    for (index, child) in values.iter().enumerate() {
                        let next = if prefix.is_empty() {
                            index.to_string()
                        } else {
                            format!("{prefix}.{index}")
                        };
                        collect_toml_leaf_keys(child, &next, out);
                    }
                } else {
                    out.push(prefix.to_string());
                }
            }
            _ => out.push(prefix.to_string()),
        }
    }

    fn key_matches_patterns(key: &str, key_patterns: &BTreeSet<String>) -> bool {
        key_patterns
            .iter()
            .any(|pattern| dotted_key_matches(key, pattern))
    }

    fn dotted_key_matches(key: &str, pattern: &str) -> bool {
        let key_segments: Vec<&str> = key.split('.').collect();
        let pattern_segments: Vec<&str> = pattern.split('.').collect();
        if key_segments.len() != pattern_segments.len() {
            return false;
        }

        for (segment, pattern_segment) in key_segments.iter().zip(pattern_segments.iter()) {
            if pattern_segment.starts_with('<') && pattern_segment.ends_with('>') {
                continue;
            }
            if segment != pattern_segment {
                return false;
            }
        }
        true
    }

    fn collect_config_key_patterns() -> BTreeSet<String> {
        let mut patterns = BTreeSet::new();
        for field in BmuxConfig::field_docs() {
            if let Some(bmux_config_doc::NestedFieldDoc::Inline { fields, .. }) = field.nested {
                collect_field_patterns(fields, field.toml_key, &mut patterns);
            }
        }
        patterns
    }

    fn collect_field_patterns(
        fields: Vec<bmux_config::FieldDoc>,
        prefix: &str,
        out: &mut BTreeSet<String>,
    ) {
        for field in fields {
            let full_key = if prefix.is_empty() {
                field.toml_key.to_string()
            } else {
                format!("{prefix}.{}", field.toml_key)
            };

            match field.nested {
                Some(bmux_config_doc::NestedFieldDoc::Inline { fields, .. }) => {
                    collect_field_patterns(fields, &full_key, out);
                }
                Some(bmux_config_doc::NestedFieldDoc::Map {
                    key_placeholder,
                    value_fields,
                    ..
                }) => {
                    let map_prefix = format!("{full_key}.{key_placeholder}");
                    collect_field_patterns(value_fields, &map_prefix, out);
                }
                Some(bmux_config_doc::NestedFieldDoc::List {
                    index_placeholder,
                    item_fields,
                    ..
                }) => {
                    let list_prefix = format!("{full_key}.{index_placeholder}");
                    collect_field_patterns(item_fields, &list_prefix, out);
                }
                None => {
                    out.insert(full_key);
                }
            }
        }
    }

    fn markdown_sources() -> Vec<PathBuf> {
        let root = workspace_root();
        let mut files = vec![
            root.join("README.md"),
            root.join("TESTING.md"),
            root.join("packages/plugin-sdk/README.md"),
            root.join("examples/native-plugin/README.md"),
        ];

        collect_markdown_files(&root.join("docs"), &mut files);

        files.sort();
        files
    }

    fn collect_markdown_files(dir: &Path, out: &mut Vec<PathBuf>) {
        if !dir.exists() {
            return;
        }

        let entries = fs::read_dir(dir)
            .unwrap_or_else(|err| panic!("failed to read dir {}: {err}", dir.display()));
        for entry in entries {
            let entry = entry
                .unwrap_or_else(|err| panic!("failed to read entry in {}: {err}", dir.display()));
            let path = entry.path();

            if path.is_dir() {
                collect_markdown_files(&path, out);
                continue;
            }

            if path.extension().is_some_and(|ext| ext == "md") {
                out.push(path);
            }
        }
    }

    fn workspace_root() -> PathBuf {
        let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        manifest_dir
            .ancestors()
            .nth(3)
            .expect("workspace root should be three levels up from ui crate")
            .to_path_buf()
    }

    fn shell_split(command: &str) -> Result<Vec<String>, String> {
        let mut args = Vec::new();
        let mut current = String::new();
        let mut in_single = false;
        let mut in_double = false;
        let mut escaped = false;

        for ch in command.chars() {
            if escaped {
                current.push(ch);
                escaped = false;
                continue;
            }

            if ch == '\\' {
                escaped = true;
                continue;
            }

            if in_single {
                if ch == '\'' {
                    in_single = false;
                } else {
                    current.push(ch);
                }
                continue;
            }

            if in_double {
                if ch == '"' {
                    in_double = false;
                } else {
                    current.push(ch);
                }
                continue;
            }

            match ch {
                '\'' => in_single = true,
                '"' => in_double = true,
                c if c.is_ascii_whitespace() => {
                    if !current.is_empty() {
                        args.push(current.clone());
                        current.clear();
                    }
                }
                _ => current.push(ch),
            }
        }

        if escaped {
            return Err("dangling escape in command".to_string());
        }
        if in_single || in_double {
            return Err("unterminated quote in command".to_string());
        }
        if !current.is_empty() {
            args.push(current);
        }

        Ok(args)
    }
}
