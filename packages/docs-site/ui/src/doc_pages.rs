//! Central registry of documentation pages.
//!
//! This module is the single source of truth for every doc page served by
//! the site. Routes, navigation, and markdown link rewriting all derive from
//! [`DOC_PAGES`] — there is no parallel registry to keep in sync.
//!
//! Pages fall into two kinds:
//!
//! * **Markdown pages**, declared with the [`markdown_page!`] macro, carry a
//!   workspace-relative `source` path plus the baked-in file bytes via
//!   `include_str!`. The source path is also used to resolve relative
//!   markdown links (see [`crate::link_map`]).
//! * **Generated pages**, declared with the [`generated_page!`] macro, are
//!   produced at render time (e.g. the CLI and config references, which are
//!   derived from `clap` metadata and config schema). They have no `source`.
//!
//! Adding a new markdown page is one `markdown_page!` invocation — the route,
//! nav entry, and link-map entry are all wired up automatically.

use hyperchad::markdown::markdown_to_container;
use hyperchad::template::Containers;

use crate::layout;
use crate::link_map;
use crate::pages::docs;
use crate::theme;

/// A single doc page wired into the site.
pub struct DocPage {
    /// Workspace-relative source path of the markdown file
    /// (e.g. `"docs/bpdl-spec.md"`). `None` for generated pages.
    ///
    /// When `Some`, relative `.md` links inside this page are rewritten by
    /// [`crate::link_map`] to the corresponding site route.
    pub source: Option<&'static str>,

    /// Site URL for this page (e.g. `"/docs/bpdl-spec"`).
    pub route: &'static str,

    /// Optional page title rendered above the content. `None` lets the
    /// markdown's first heading serve as the title.
    pub title: Option<&'static str>,

    /// Sidebar section for this page. `None` hides the page from navigation.
    pub section: Option<NavSectionId>,

    /// Sidebar label. Must be `Some` when `section` is `Some`.
    pub nav_label: Option<&'static str>,

    /// Renderer. Produces the layout-wrapped page contents.
    pub render: fn(&DocPage) -> Containers,
}

/// Logical sidebar sections. The display order of sections in the sidebar is
/// the order they appear here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NavSectionId {
    GettingStarted,
    Reference,
    Plugins,
    PluginOperations,
    Development,
}

impl NavSectionId {
    /// Human-readable section title for the sidebar.
    #[must_use]
    pub const fn title(self) -> &'static str {
        match self {
            Self::GettingStarted => "Getting Started",
            Self::Reference => "Reference",
            Self::Plugins => "Plugins",
            Self::PluginOperations => "Plugin Operations",
            Self::Development => "Development",
        }
    }

    /// All section IDs, in the order they appear in the sidebar.
    #[must_use]
    pub const fn ordered() -> &'static [Self] {
        &[
            Self::GettingStarted,
            Self::Reference,
            Self::Plugins,
            Self::PluginOperations,
            Self::Development,
        ]
    }
}

/// Shared render helper for markdown pages. Applies the link-map rewrite and
/// dark theme, then wraps the output in the standard docs layout.
fn render_markdown(page: &DocPage, markdown: &str) -> Containers {
    let mut container = markdown_to_container(markdown);
    if let Some(source) = page.source {
        link_map::rewrite_relative_links(&mut container, source);
    }
    theme::apply_dark_theme(&mut container);
    layout::docs_layout(page.route, page.title, &vec![container])
}

/// Declare a [`DocPage`] backed by a markdown file on disk. The file is
/// `include_str!`'d at compile time so the docs binary is fully self-contained.
///
/// # Example
///
/// ```ignore
/// markdown_page! {
///     source:    "docs/bpdl-spec.md",
///     route:     "/docs/bpdl-spec",
///     title:     Some("BPDL Specification"),
///     section:   Some(NavSectionId::Plugins),
///     nav_label: Some("BPDL Specification"),
/// }
/// ```
macro_rules! markdown_page {
    (
        source:    $source:literal,
        route:     $route:literal,
        title:     $title:expr,
        section:   $section:expr,
        nav_label: $nav_label:expr $(,)?
    ) => {{
        fn render(page: &DocPage) -> Containers {
            render_markdown(page, include_str!(concat!("../../../../", $source)))
        }
        DocPage {
            source: Some($source),
            route: $route,
            title: $title,
            section: $section,
            nav_label: $nav_label,
            render,
        }
    }};
}

/// Declare a [`DocPage`] whose content is generated in-process by an arbitrary
/// render function (e.g. the CLI reference produced from clap metadata).
macro_rules! generated_page {
    (
        route:     $route:literal,
        title:     $title:expr,
        section:   $section:expr,
        nav_label: $nav_label:expr,
        render:    $render:expr $(,)?
    ) => {
        DocPage {
            source: None,
            route: $route,
            title: $title,
            section: $section,
            nav_label: $nav_label,
            render: $render,
        }
    };
}

// ── Generated-page render functions ─────────────────────────────────────────
//
// The per-page helpers live on `crate::pages::docs` so they can stay close to
// the clap/config machinery they depend on.

fn render_overview(page: &DocPage) -> Containers {
    render_markdown(page, include_str!("../../../../README.md"))
}

fn render_installation(page: &DocPage) -> Containers {
    let readme = include_str!("../../../../README.md");
    let content = docs::extract_section_for(readme, "## Installation", Some("## "));
    render_markdown(page, &content)
}

fn render_quickstart(page: &DocPage) -> Containers {
    let readme = include_str!("../../../../README.md");
    let content = docs::extract_section_for(readme, "## Current CLI Workflow", Some("## "));
    render_markdown(page, &content)
}

fn render_cli(page: &DocPage) -> Containers {
    render_markdown(page, &docs::generate_cli_reference())
}

fn render_config(page: &DocPage) -> Containers {
    render_markdown(page, &docs::generate_config_reference())
}

// ── The registry ────────────────────────────────────────────────────────────

/// Every doc page served by the site, in sidebar display order within each
/// section.
pub static DOC_PAGES: &[DocPage] = &[
    // Getting Started
    generated_page! {
        route:     "/docs",
        title:     None,
        section:   Some(NavSectionId::GettingStarted),
        nav_label: Some("Overview"),
        render:    render_overview,
    },
    generated_page! {
        route:     "/docs/installation",
        title:     Some("Installation"),
        section:   Some(NavSectionId::GettingStarted),
        nav_label: Some("Installation"),
        render:    render_installation,
    },
    generated_page! {
        route:     "/docs/quickstart",
        title:     Some("Quick Start"),
        section:   Some(NavSectionId::GettingStarted),
        nav_label: Some("Quick Start"),
        render:    render_quickstart,
    },
    // Reference
    markdown_page! {
        source:    "docs/concepts.md",
        route:     "/docs/concepts",
        title:     None,
        section:   Some(NavSectionId::Reference),
        nav_label: Some("Concepts"),
    },
    generated_page! {
        route:     "/docs/cli",
        title:     Some("CLI Reference"),
        section:   Some(NavSectionId::Reference),
        nav_label: Some("CLI"),
        render:    render_cli,
    },
    markdown_page! {
        source:    "docs/command-cookbook.md",
        route:     "/docs/command-cookbook",
        title:     None,
        section:   Some(NavSectionId::Reference),
        nav_label: Some("Command Cookbook"),
    },
    markdown_page! {
        source:    "docs/kiosk.md",
        route:     "/docs/kiosk",
        title:     Some("Kiosk Access"),
        section:   Some(NavSectionId::Reference),
        nav_label: Some("Kiosk Access"),
    },
    markdown_page! {
        source:    "docs/playbooks.md",
        route:     "/docs/playbooks",
        title:     None,
        section:   Some(NavSectionId::Reference),
        nav_label: Some("Playbooks"),
    },
    markdown_page! {
        source:    "docs/images.md",
        route:     "/docs/images",
        title:     None,
        section:   Some(NavSectionId::Reference),
        nav_label: Some("Images & Compression"),
    },
    generated_page! {
        route:     "/docs/config",
        title:     Some("Configuration"),
        section:   Some(NavSectionId::Reference),
        nav_label: Some("Configuration"),
        render:    render_config,
    },
    // Plugins
    markdown_page! {
        source:    "docs/plugins.md",
        route:     "/docs/plugins",
        title:     None,
        section:   Some(NavSectionId::Plugins),
        nav_label: Some("Plugin Architecture"),
    },
    markdown_page! {
        source:    "docs/bpdl-spec.md",
        route:     "/docs/bpdl-spec",
        title:     Some("BPDL Specification"),
        section:   Some(NavSectionId::Plugins),
        nav_label: Some("BPDL Specification"),
    },
    markdown_page! {
        source:    "packages/plugin-sdk/README.md",
        route:     "/docs/plugin-sdk",
        title:     None,
        section:   Some(NavSectionId::Plugins),
        nav_label: Some("Plugin SDK"),
    },
    markdown_page! {
        source:    "examples/native-plugin/README.md",
        route:     "/docs/plugin-example",
        title:     None,
        section:   Some(NavSectionId::Plugins),
        nav_label: Some("Example Plugin"),
    },
    // Plugin Operations
    markdown_page! {
        source:    "docs/plugin-ops.md",
        route:     "/docs/plugin-ops",
        title:     None,
        section:   Some(NavSectionId::PluginOperations),
        nav_label: Some("Plugin Ops"),
    },
    markdown_page! {
        source:    "docs/plugin-triage-playbook.md",
        route:     "/docs/plugin-triage-playbook",
        title:     None,
        section:   Some(NavSectionId::PluginOperations),
        nav_label: Some("Plugin Triage"),
    },
    markdown_page! {
        source:    "docs/plugin-perf-troubleshooting.md",
        route:     "/docs/plugin-perf-troubleshooting",
        title:     None,
        section:   Some(NavSectionId::PluginOperations),
        nav_label: Some("Perf Troubleshooting"),
    },
    // Development
    markdown_page! {
        source:    "docs/setup-guide.md",
        route:     "/docs/setup-guide",
        title:     None,
        section:   Some(NavSectionId::Development),
        nav_label: Some("Setup Guide"),
    },
    markdown_page! {
        source:    "TESTING.md",
        route:     "/docs/testing",
        title:     None,
        section:   Some(NavSectionId::Development),
        nav_label: Some("Testing"),
    },
    markdown_page! {
        source:    "docs/troubleshooting.md",
        route:     "/docs/troubleshooting",
        title:     None,
        section:   Some(NavSectionId::Development),
        nav_label: Some("Troubleshooting"),
    },
    markdown_page! {
        source:    "docs/operations.md",
        route:     "/docs/operations",
        title:     None,
        section:   Some(NavSectionId::Development),
        nav_label: Some("Operations"),
    },
    markdown_page! {
        source:    "docs/docs-snippet-tags.md",
        route:     "/docs/docs-snippet-tags",
        title:     None,
        section:   Some(NavSectionId::Development),
        nav_label: Some("Snippet Tags"),
    },
];

/// Render the 404 page. Not part of `DOC_PAGES` because it has no route or
/// nav entry; it's served by a dedicated router path.
#[must_use]
pub fn not_found() -> Containers {
    let page = DocPage {
        source: None,
        route: "/not-found",
        title: Some("404"),
        section: None,
        nav_label: None,
        render: |_| Containers::new(),
    };
    render_markdown(&page, "The page you are looking for does not exist.")
}

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
    fn sources_are_unique_when_present() {
        let mut seen = HashSet::new();
        for page in DOC_PAGES {
            if let Some(source) = page.source {
                assert!(
                    seen.insert(source),
                    "duplicate source in DOC_PAGES: {source}",
                );
            }
        }
    }

    #[test]
    fn section_and_nav_label_are_coupled() {
        for page in DOC_PAGES {
            match (page.section, page.nav_label) {
                (Some(_), Some(_)) | (None, None) => {}
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
        // Sanity check that every declared source path points at a real file
        // in the workspace. If this fires, someone renamed a markdown file
        // without updating DOC_PAGES.
        let manifest_dir: std::path::PathBuf = env!("CARGO_MANIFEST_DIR").into();
        // CARGO_MANIFEST_DIR is packages/docs-site/ui; workspace root is three levels up.
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
