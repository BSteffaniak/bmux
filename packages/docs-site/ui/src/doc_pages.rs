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
const USAGE: &str = "usage";
const BROWSER_LIVE: &str = "browser-live";
const TECHNICAL_REFERENCE: &str = "technical-reference";
const PERFORMANCE: &str = "performance";

/// Sidebar sections, in display order.
pub static DOC_SECTIONS: &[DocsSection] = &[
    DocsSection::new(GETTING_STARTED, "Getting Started"),
    DocsSection::new(USAGE, "Usage & Workflows"),
    DocsSection::new(REFERENCE, "Reference"),
    DocsSection::new(BROWSER_LIVE, "Browser & Live"),
    DocsSection::new(PLUGINS, "Plugins"),
    DocsSection::new(PLUGIN_OPERATIONS, "Plugin Operations"),
    DocsSection::new(PERFORMANCE, "Performance & Operations"),
    DocsSection::new(DEVELOPMENT, "Development & Architecture"),
    DocsSection::new(TECHNICAL_REFERENCE, "Technical Reference"),
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
        source: "docs/bterm-managed-storage.md",
        route: "/docs/bterm-managed-storage",
        title: "Managed Companion Storage",
        section: REFERENCE,
        nav_label: "Managed Storage",
    },
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
    // Usage & Workflows
    docs_markdown_page! {
        source: "docs/cli-workflows.md",
        route: "/docs/cli-workflows",
        title: "CLI Workflows",
        section: USAGE,
        nav_label: "CLI Workflows",
    },
    docs_markdown_page! {
        source: "docs/config-profiles.md",
        route: "/docs/config-profiles",
        title: "Configuration Profiles",
        section: USAGE,
        nav_label: "Configuration Profiles",
    },
    docs_markdown_page! {
        source: "docs/tab-presentations.md",
        route: "/docs/tab-presentations",
        title: "Tab Presentations",
        section: USAGE,
        nav_label: "Tab Presentations",
    },
    docs_markdown_page! {
        source: "docs/text-input.md",
        route: "/docs/text-input",
        title: "Text Input",
        section: USAGE,
        nav_label: "Text Input",
    },
    docs_markdown_page! {
        source: "docs/private-input.md",
        route: "/docs/private-input",
        title: "Private Input",
        section: USAGE,
        nav_label: "Private Input",
    },
    docs_markdown_page! {
        source: "docs/pane-padding.md",
        route: "/docs/pane-padding",
        title: "Pane Padding",
        section: USAGE,
        nav_label: "Pane Padding",
    },
    docs_markdown_page! {
        source: "docs/remote-pairing.md",
        route: "/docs/remote-pairing",
        title: "Remote Pairing",
        section: USAGE,
        nav_label: "Remote Pairing",
    },
    docs_markdown_page! {
        source: "docs/recording-process.md",
        route: "/docs/recording-process",
        title: "Recorded Processes",
        section: USAGE,
        nav_label: "Recorded Processes",
    },
    docs_markdown_page! {
        source: "docs/slots.md",
        route: "/docs/slots",
        title: "Version Slots",
        section: USAGE,
        nav_label: "Version Slots",
    },
    docs_markdown_page! {
        source: "docs/application-resurrection.md",
        route: "/docs/application-resurrection",
        title: "Application Resurrection (Experimental Integration)",
        section: USAGE,
        nav_label: "Resurrection (Experimental)",
    },
    // Browser & Live
    docs_markdown_page! {
        source: "docs/browser-rendering.md",
        route: "/docs/browser-rendering",
        title: "Browser Rendering",
        section: BROWSER_LIVE,
        nav_label: "Browser Rendering",
    },
    docs_markdown_page! {
        source: "docs/browser-delivery-diagnostics.md",
        route: "/docs/browser-delivery-diagnostics",
        title: "Browser Delivery Diagnostics",
        section: BROWSER_LIVE,
        nav_label: "Delivery Diagnostics",
    },
    docs_markdown_page! {
        source: "docs/live-broadcasting.md",
        route: "/docs/live-broadcasting",
        title: "Live Broadcasting",
        section: BROWSER_LIVE,
        nav_label: "Live Broadcasting",
    },
    docs_markdown_page! {
        source: "docs/live-deployment.md",
        route: "/docs/live-deployment",
        title: "Live Deployment",
        section: BROWSER_LIVE,
        nav_label: "Live Deployment",
    },
    docs_markdown_page! {
        source: "docs/presenter-web.md",
        route: "/docs/presenter-web",
        title: "Presenter Web Audience",
        section: BROWSER_LIVE,
        nav_label: "Presenter Web Audience",
    },
    docs_markdown_page! {
        source: "docs/responsive-presentations.md",
        route: "/docs/responsive-presentations",
        title: "Responsive Presentations",
        section: BROWSER_LIVE,
        nav_label: "Responsive Presentations",
    },
    // Development & Architecture
    docs_markdown_page! {
        source: "docs/development-toolchain.md",
        route: "/docs/development-toolchain",
        title: "Development Toolchain",
        section: DEVELOPMENT,
        nav_label: "Development Toolchain",
    },
    docs_markdown_page! {
        source: "docs/windows-platform-backends.md",
        route: "/docs/windows-platform-backends",
        title: "Windows Platform Backends",
        section: DEVELOPMENT,
        nav_label: "Windows Platform Backends",
    },
    docs_markdown_page! {
        source: "docs/tui-framework.md",
        route: "/docs/tui-framework",
        title: "TUI Framework",
        section: DEVELOPMENT,
        nav_label: "TUI Framework",
    },
    docs_markdown_page! {
        source: "docs/tui-runtime.md",
        route: "/docs/tui-runtime",
        title: "TUI Runtime",
        section: DEVELOPMENT,
        nav_label: "TUI Runtime",
    },
    docs_markdown_page! {
        source: "docs/bterm-configuration.md",
        route: "/docs/bterm-configuration",
        title: "Bterm Configuration",
        section: DEVELOPMENT,
        nav_label: "Bterm Configuration",
    },
    docs_markdown_page! {
        source: "docs/bterm-reconciliation.md",
        route: "/docs/bterm-reconciliation",
        title: "Bterm Reconciliation",
        section: DEVELOPMENT,
        nav_label: "Bterm Reconciliation",
    },
    docs_markdown_page! {
        source: "docs/selection-authority.md",
        route: "/docs/selection-authority",
        title: "Selection Authority",
        section: DEVELOPMENT,
        nav_label: "Selection Authority",
    },
    docs_markdown_page! {
        source: "docs/invocation-compatibility.md",
        route: "/docs/invocation-compatibility",
        title: "Invocation Compatibility",
        section: DEVELOPMENT,
        nav_label: "Invocation Compatibility",
    },
    // Technical Reference
    docs_markdown_page! {
        source: "docs/browser-viewport-sizing.md",
        route: "/docs/browser-viewport-sizing",
        title: "Browser Viewport Sizing",
        section: TECHNICAL_REFERENCE,
        nav_label: "Browser Viewport Sizing",
    },
    docs_markdown_page! {
        source: "docs/interactive-browser-surfaces.md",
        route: "/docs/interactive-browser-surfaces",
        title: "Interactive Browser Surfaces",
        section: TECHNICAL_REFERENCE,
        nav_label: "Interactive Browser Surfaces",
    },
    docs_markdown_page! {
        source: "docs/live-components.md",
        route: "/docs/live-components",
        title: "Live Components",
        section: TECHNICAL_REFERENCE,
        nav_label: "Live Components",
    },
    docs_markdown_page! {
        source: "docs/live-broadcast-protocol.md",
        route: "/docs/live-broadcast-protocol",
        title: "Live Broadcasting Protocol",
        section: TECHNICAL_REFERENCE,
        nav_label: "Broadcasting Protocol",
    },
    docs_markdown_page! {
        source: "docs/live-stream-protocol.md",
        route: "/docs/live-stream-protocol",
        title: "Live Stream Protocol",
        section: TECHNICAL_REFERENCE,
        nav_label: "Stream Protocol",
    },
    docs_markdown_page! {
        source: "docs/ipc-security-policy.md",
        route: "/docs/ipc-security-policy",
        title: "Local IPC Security Policy",
        section: TECHNICAL_REFERENCE,
        nav_label: "IPC Security",
    },
    // Performance & Operations
    docs_markdown_page! {
        source: "docs/performance-metrics.md",
        route: "/docs/performance-metrics",
        title: "Performance Metrics",
        section: PERFORMANCE,
        nav_label: "Performance Metrics",
    },
    docs_markdown_page! {
        source: "docs/image-performance.md",
        route: "/docs/image-performance",
        title: "Image Performance",
        section: PERFORMANCE,
        nav_label: "Image Performance",
    },
    docs_markdown_page! {
        source: "docs/plugin-game-day.md",
        route: "/docs/plugin-game-day",
        title: "Plugin Game Day",
        section: PERFORMANCE,
        nav_label: "Plugin Game Day",
    },
    docs_markdown_page! {
        source: "docs/plugin-performance-usability-gates.md",
        route: "/docs/plugin-performance-usability-gates",
        title: "Plugin Performance & Usability Gates",
        section: PERFORMANCE,
        nav_label: "Plugin Performance Gates",
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

    // Top-level docs not intended as current public-facing site guidance.
    // Keep reasons explicit; publishing a page requires removing its exclusion.
    const SITE_EXCLUSIONS: &[(&str, &str)] = &[
        (
            "docs/AGENTS.md",
            "Agent workflow instructions, not product documentation",
        ),
        (
            "docs/distribution.md",
            "Package availability claims need reconciliation with README",
        ),
        (
            "docs/mobile-adr-0001-ui-and-shared-core.md",
            "Mobile architecture proposal, not a supported usage guide",
        ),
        ("docs/mobile-ffi-contract.md", "Draft mobile FFI contract"),
        (
            "docs/mobile-m0-architecture.md",
            "Mobile milestone planning document",
        ),
        (
            "docs/terminology-cutover.md",
            "Terminology migration record; concepts.md owns current vocabulary",
        ),
        (
            "docs/legacy-attach-presentation-inventory.md",
            "Historical presentation migration inventory",
        ),
        (
            "docs/bterm-performance.md",
            "Developer renderer measurement protocol and optimization evidence",
        ),
        (
            "docs/mermaid-wasm-performance.md",
            "Historical measurements of a removed renderer",
        ),
    ];

    #[test]
    fn top_level_docs_are_published_or_explicitly_excluded() {
        let workspace_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .nth(3)
            .expect("workspace root should be three levels up from ui crate");
        let published: HashSet<_> = DOC_PAGES.iter().filter_map(|page| page.source).collect();
        let mut excluded = HashSet::new();
        for &(source, reason) in SITE_EXCLUSIONS {
            assert!(
                !reason.trim().is_empty(),
                "missing exclusion reason for {source}"
            );
            assert!(excluded.insert(source), "duplicate exclusion for {source}");
            assert!(
                workspace_root.join(source).is_file(),
                "stale exclusion for {source}"
            );
            assert!(
                !published.contains(source),
                "published page {source} is also excluded"
            );
        }

        let mut missing = Vec::new();
        for entry in std::fs::read_dir(workspace_root.join("docs")).expect("read docs directory") {
            let path = entry.expect("read docs entry").path();
            if path.is_file() && path.extension().is_some_and(|extension| extension == "md") {
                let source = format!("docs/{}", path.file_name().unwrap().to_str().unwrap());
                if !published.contains(source.as_str()) && !excluded.contains(source.as_str()) {
                    missing.push(source);
                }
            }
        }
        missing.sort();
        assert!(
            missing.is_empty(),
            "Register public-facing docs in DOC_PAGES or add a reason to SITE_EXCLUSIONS: {missing:?}"
        );
    }

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
