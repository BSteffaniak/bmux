//! Generation helpers for documentation pages.
//!
//! The per-page renderers and the routing registry live in
//! [`crate::doc_pages`]. This module retains only the logic that generates
//! markdown from in-process data (clap CLI metadata and the config schema)
//! plus small helpers shared by those generators.

use clap::CommandFactory;

use bmux_config::{BmuxConfig, ENV_OVERRIDE_DOCS};

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
    let env_overrides =
        ENV_OVERRIDE_DOCS
            .iter()
            .map(|override_doc| hyperchad_docs_site::EnvOverrideDoc {
                variable: override_doc.variable,
                scope: override_doc.scope,
                description: override_doc.description,
            });

    hyperchad_docs_site::ConfigReference::<BmuxConfig>::new()
        .intro(
            "bmux is configured via a `bmux.toml` file. If no config file exists, \
             bmux uses sensible defaults for all options.\n\n\
             ## Config File Location\n\n\
             bmux resolves `bmux.toml` from the configured config directory candidate \
             chain. Use the path/env override table below to pin exact locations.\n\n\
             ---",
        )
        .env_overrides(env_overrides)
        .toml_table_headings()
        .option_column_label("Option")
        .section_appendix("status_bar", render_status_preset_examples())
        .render()
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
tab_label_max_width = 22\n\
tab_template = \"{index}:{name}\"\n\
show_mode = true\n\
show_role = true\n\
show_follow = true\n\
show_hint = true\n\
hover_highlight = true\n\
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
tab_template = \"{name}\"\n\
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
    s.push_str("### Status Color Override (partial)\n\n");
    s.push_str(
        "```toml\n\
[status_bar.colors]\n\
# Unset fields inherit from the runtime appearance defaults\n\
tab_active_bg = \"#7aa2f7\"\n\
tab_active_fg = \"#1a1b26\"\n\
tab_inactive_bg = \"#2a2f45\"\n\
module_bg = \"#343a55\"\n\
```\n\n",
    );
    s
}

// ── CLI reference generation from clap Command tree ─────────────────────────

/// Generate the full CLI reference markdown by walking the clap `Command` tree
/// from `bmux_cli_schema::Cli`. Descriptions come from `///` doc comments on
/// the derive structs and are always in sync with the actual binary.
pub(crate) fn generate_cli_reference() -> String {
    hyperchad_docs_site::CliReference::new("bmux", bmux_cli_schema::Cli::command()).render()
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
    fn config_reference_includes_bracketed_paste_default_and_safety_docs() {
        let doc = generate_config_reference();

        assert!(doc.contains("bracketed_paste"));
        assert!(doc.contains("`bracketed-paste` Cargo feature"));
        assert!(doc.contains("defaults to true"));
        assert!(doc.contains("indistinguishable from typed terminal input"));
    }

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
        assert!(doc.contains("colors.tab_active_bg"));
        assert!(doc.contains("routing.conflict_mode"));
        assert!(doc.contains("routing.required_paths.<index>.path"));
        assert!(doc.contains("routing.required_namespaces.<index>.namespace"));
        assert!(doc.contains("rolling_event_kinds"));
        assert!(doc.contains("pane_input_raw"));
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
        assert!(doc.contains("--rolling-event-kind"));
        assert!(doc.contains("`pane-input-raw`"));

        let rolling_event_kind_line = doc
            .lines()
            .find(|line| line.contains("`--rolling-event-kind`"))
            .expect("missing --rolling-event-kind line");
        assert!(rolling_event_kind_line.contains("repeatable"));
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
                Some(bmux_config_doc::NestedFieldDoc::MapValue {
                    key_placeholder, ..
                }) => {
                    out.insert(format!("{full_key}.{key_placeholder}"));
                }
                Some(bmux_config_doc::NestedFieldDoc::ListValue {
                    index_placeholder, ..
                }) => {
                    out.insert(format!("{full_key}.{index_placeholder}"));
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
