//! Doc page functions — one per route, embedding markdown via `include_str!`.

use hyperchad::markdown::markdown_to_container;
use hyperchad::template::Containers;

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
        &md(include_str!("../../../../../docs/config.md")),
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
