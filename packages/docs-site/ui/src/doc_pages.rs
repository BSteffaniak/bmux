//! Central registry of documentation pages.
//!
//! Routes, navigation, and markdown link rewriting derive from this one
//! registry. The rendering/layout machinery lives in `hyperchad_docs_site`.

use hyperchad_docs_site::{DocPage, DocsSection, docs_generated_page, docs_markdown_page};

use crate::pages::docs;

const GETTING_STARTED: &str = "getting-started";
const REFERENCE: &str = "reference";
const PLUGINS: &str = "plugins";
const PLUGIN_OPERATIONS: &str = "plugin-operations";
const DEVELOPMENT: &str = "development";

/// Sidebar sections, in display order.
pub static DOC_SECTIONS: &[DocsSection] = &[
    DocsSection::new(GETTING_STARTED, "Getting Started"),
    DocsSection::new(REFERENCE, "Reference"),
    DocsSection::new(PLUGINS, "Plugins"),
    DocsSection::new(PLUGIN_OPERATIONS, "Plugin Operations"),
    DocsSection::new(DEVELOPMENT, "Development"),
];

fn generate_overview() -> String {
    include_str!("../../../../README.md").to_string()
}

fn generate_installation() -> String {
    let readme = include_str!("../../../../README.md");
    docs::extract_section_for(readme, "## Installation", Some("## "))
}

fn generate_quickstart() -> String {
    let readme = include_str!("../../../../README.md");
    docs::extract_section_for(readme, "## Current CLI Workflow", Some("## "))
}

fn generate_cli() -> String {
    docs::generate_cli_reference()
}

fn generate_config() -> String {
    docs::generate_config_reference()
}

/// Every doc page served by the site, in sidebar display order within each
/// section.
pub static DOC_PAGES: &[DocPage] = &[
    // Getting Started
    docs_generated_page! {
        route: "/docs",
        title: None,
        section: GETTING_STARTED,
        nav_label: "Overview",
        generate: generate_overview,
    },
    docs_generated_page! {
        route: "/docs/installation",
        title: "Installation",
        section: GETTING_STARTED,
        nav_label: "Installation",
        generate: generate_installation,
    },
    docs_generated_page! {
        route: "/docs/quickstart",
        title: "Quick Start",
        section: GETTING_STARTED,
        nav_label: "Quick Start",
        generate: generate_quickstart,
    },
    // Reference
    docs_markdown_page! {
        source: "docs/concepts.md",
        route: "/docs/concepts",
        title: "Concepts",
        section: REFERENCE,
        nav_label: "Concepts",
    },
    docs_generated_page! {
        route: "/docs/cli",
        title: "CLI Reference",
        section: REFERENCE,
        nav_label: "CLI",
        generate: generate_cli,
    },
    docs_markdown_page! {
        source: "docs/command-cookbook.md",
        route: "/docs/command-cookbook",
        title: "Command Cookbook",
        section: REFERENCE,
        nav_label: "Command Cookbook",
    },
    docs_markdown_page! {
        source: "docs/kiosk.md",
        route: "/docs/kiosk",
        title: "Kiosk Access",
        section: REFERENCE,
        nav_label: "Kiosk Access",
    },
    docs_markdown_page! {
        source: "docs/playbooks.md",
        route: "/docs/playbooks",
        title: "Playbooks",
        section: REFERENCE,
        nav_label: "Playbooks",
    },
    docs_markdown_page! {
        source: "docs/images.md",
        route: "/docs/images",
        title: "Images & Compression",
        section: REFERENCE,
        nav_label: "Images & Compression",
    },
    docs_generated_page! {
        route: "/docs/config",
        title: "Configuration",
        section: REFERENCE,
        nav_label: "Configuration",
        generate: generate_config,
    },
    // Plugins
    docs_markdown_page! {
        source: "docs/plugins.md",
        route: "/docs/plugins",
        title: "Plugin Architecture",
        section: PLUGINS,
        nav_label: "Plugin Architecture",
    },
    docs_markdown_page! {
        source: "docs/bpdl-spec.md",
        route: "/docs/bpdl-spec",
        title: "BPDL Specification",
        section: PLUGINS,
        nav_label: "BPDL Specification",
    },
    docs_markdown_page! {
        source: "packages/plugin-sdk/README.md",
        route: "/docs/plugin-sdk",
        title: "Plugin SDK",
        section: PLUGINS,
        nav_label: "Plugin SDK",
    },
    docs_markdown_page! {
        source: "examples/native-plugin/README.md",
        route: "/docs/plugin-example",
        title: "Example Plugin",
        section: PLUGINS,
        nav_label: "Example Plugin",
    },
    // Plugin Operations
    docs_markdown_page! {
        source: "docs/plugin-ops.md",
        route: "/docs/plugin-ops",
        title: "Plugin Ops",
        section: PLUGIN_OPERATIONS,
        nav_label: "Plugin Ops",
    },
    docs_markdown_page! {
        source: "docs/plugin-triage-playbook.md",
        route: "/docs/plugin-triage-playbook",
        title: "Plugin Triage",
        section: PLUGIN_OPERATIONS,
        nav_label: "Plugin Triage",
    },
    docs_markdown_page! {
        source: "docs/plugin-perf-troubleshooting.md",
        route: "/docs/plugin-perf-troubleshooting",
        title: "Perf Troubleshooting",
        section: PLUGIN_OPERATIONS,
        nav_label: "Perf Troubleshooting",
    },
    // Development
    docs_markdown_page! {
        source: "docs/setup-guide.md",
        route: "/docs/setup-guide",
        title: "Setup Guide",
        section: DEVELOPMENT,
        nav_label: "Setup Guide",
    },
    docs_markdown_page! {
        source: "TESTING.md",
        route: "/docs/testing",
        title: "Testing",
        section: DEVELOPMENT,
        nav_label: "Testing",
    },
    docs_markdown_page! {
        source: "docs/troubleshooting.md",
        route: "/docs/troubleshooting",
        title: "Troubleshooting",
        section: DEVELOPMENT,
        nav_label: "Troubleshooting",
    },
    docs_markdown_page! {
        source: "docs/operations.md",
        route: "/docs/operations",
        title: "Operations",
        section: DEVELOPMENT,
        nav_label: "Operations",
    },
    docs_markdown_page! {
        source: "docs/docs-snippet-tags.md",
        route: "/docs/docs-snippet-tags",
        title: "Snippet Tags",
        section: DEVELOPMENT,
        nav_label: "Snippet Tags",
    },
];

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn routes_are_unique() {
        let mut seen = HashSet::new();
        for page in DOC_PAGES {
            assert!(
                seen.insert(page.route),
                "duplicate route in DOC_PAGES: {}",
                page.route
            );
        }
    }

    #[test]
    fn nav_entries_have_sections() {
        let section_ids: HashSet<_> = DOC_SECTIONS.iter().map(|section| section.id).collect();
        for page in DOC_PAGES {
            match (page.section, page.nav_label) {
                (Some(section), Some(_)) => assert!(
                    section_ids.contains(section),
                    "page {} references unknown section {section:?}",
                    page.route
                ),
                (None, None) => {}
                (Some(section), None) => panic!(
                    "page {} has section {:?} but no nav_label",
                    page.route, section
                ),
                (None, Some(label)) => {
                    panic!("page {} has nav_label {label:?} but no section", page.route)
                }
            }
        }
    }

    #[test]
    fn every_route_starts_with_slash() {
        for page in DOC_PAGES {
            assert!(
                page.route.starts_with('/'),
                "route must start with '/': {}",
                page.route
            );
        }
    }

    #[test]
    fn sources_exist_on_disk() {
        let manifest_dir: std::path::PathBuf = env!("CARGO_MANIFEST_DIR").into();
        let workspace_root = manifest_dir
            .ancestors()
            .nth(3)
            .expect("workspace root should be three levels up from ui crate");

        for page in DOC_PAGES {
            if let Some(source) = page.source {
                let path = workspace_root.join(source);
                assert!(
                    path.is_file(),
                    "DOC_PAGES source {source:?} does not exist at {}",
                    path.display()
                );
            }
        }
    }
}
