//! Relative markdown link resolver for the docs site.
//!
//! Hyperchad's markdown renderer passes `href` values through verbatim. Our
//! source markdown (e.g. `docs/plugins.md`) uses relative file paths like
//! `./bpdl-spec.md` and `../packages/plugin-sdk/README.md` so links work when
//! the files are rendered on GitHub or in a local IDE. On the docs site those
//! paths would 404 because routes look like `/docs/bpdl-spec`, not
//! `/docs/bpdl-spec.md`.
//!
//! This module post-processes the rendered [`Container`] tree and rewrites
//! each [`Element::Anchor`]'s `href` to the corresponding site route when
//! the relative path resolves to a known [`DocPage`] source. Unknown paths
//! pass through unmodified so GitHub-only links remain intact (they simply
//! 404 on the site).
//!
//! The lookup table is **derived** from [`crate::doc_pages::DOC_PAGES`]; we
//! never maintain a parallel registry.

use hyperchad::template::Container;
use hyperchad::transformer::Element;

use crate::doc_pages::DOC_PAGES;

/// Walk the container tree and rewrite relative `.md` links on anchor
/// elements, given the workspace-relative `source_path` of the markdown file
/// that produced this tree (e.g. `"docs/plugins.md"`).
pub fn rewrite_relative_links(container: &mut Container, source_path: &str) {
    if let Element::Anchor { href, .. } = &mut container.element
        && let Some(current) = href.as_deref()
        && let Some(rewritten) = resolve_href(current, source_path)
    {
        *href = Some(rewritten);
    }

    for child in &mut container.children {
        rewrite_relative_links(child, source_path);
    }
}

/// Resolve an anchor `href` against the given page `source_path`.
///
/// Returns `Some(new_href)` when the href is a relative markdown path whose
/// normalized target matches a known [`DocPage`] `source`. Returns `None`
/// when the href should be left verbatim (external URLs, mail links,
/// in-page anchors, absolute paths, or unknown relative targets).
#[must_use]
pub fn resolve_href(href: &str, source_path: &str) -> Option<String> {
    // Leave non-relative references alone.
    if href.is_empty()
        || href.starts_with("http://")
        || href.starts_with("https://")
        || href.starts_with("mailto:")
        || href.starts_with("ftp://")
        || href.starts_with("//")
        || href.starts_with('#')
        || href.starts_with('/')
    {
        return None;
    }

    // Split off optional `#fragment`.
    let (path_part, fragment) = match href.split_once('#') {
        Some((p, f)) => (p, Some(f)),
        None => (href, None),
    };

    // Normalize `dirname(source) + path_part` purely as strings.
    let base_dir = parent_dir(source_path);
    let normalized = normalize_join(base_dir, path_part)?;

    let route = source_to_route(&normalized)?;

    Some(match fragment {
        Some(f) if !f.is_empty() => format!("{route}#{f}"),
        _ => route.to_string(),
    })
}

/// Look up the site route for a workspace-relative source path, iterating the
/// central [`DOC_PAGES`] registry so there is no duplicate map to maintain.
fn source_to_route(source: &str) -> Option<&'static str> {
    DOC_PAGES
        .iter()
        .find(|page| page.source == Some(source))
        .map(|page| page.route)
}

/// Return everything before the last `/` in `source`, or `""` for top-level
/// files (e.g. `README.md` -> `""`, `docs/plugins.md` -> `"docs"`).
fn parent_dir(source: &str) -> &str {
    match source.rfind('/') {
        Some(idx) => &source[..idx],
        None => "",
    }
}

/// Join `base` and `rel` as workspace-relative POSIX paths and collapse
/// `.`/`..` segments. Pure string manipulation — no filesystem access.
///
/// Returns `None` if the result escapes the workspace root (too many `..`).
fn normalize_join(base: &str, rel: &str) -> Option<String> {
    let mut segments: Vec<&str> = if base.is_empty() {
        Vec::new()
    } else {
        base.split('/').collect()
    };

    for segment in rel.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                // `pop()` returning None means we tried to ascend past the
                // workspace root; treat as a non-match so the link falls
                // through to the verbatim-pass-through branch.
                segments.pop()?;
            }
            other => segments.push(other),
        }
    }

    Some(segments.join("/"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn external_http_links_pass_through() {
        assert_eq!(resolve_href("https://example.com", "docs/plugins.md"), None);
        assert_eq!(
            resolve_href("http://example.com/foo", "docs/plugins.md"),
            None
        );
    }

    #[test]
    fn mailto_and_ftp_pass_through() {
        assert_eq!(
            resolve_href("mailto:foo@example.com", "docs/plugins.md"),
            None
        );
        assert_eq!(resolve_href("ftp://example.com/x", "docs/plugins.md"), None);
    }

    #[test]
    fn in_page_anchor_passes_through() {
        assert_eq!(resolve_href("#heading", "docs/plugins.md"), None);
    }

    #[test]
    fn absolute_site_path_passes_through() {
        assert_eq!(resolve_href("/docs/kiosk", "docs/plugins.md"), None);
    }

    #[test]
    fn relative_sibling_md_resolves_to_route() {
        assert_eq!(
            resolve_href("./bpdl-spec.md", "docs/plugins.md"),
            Some("/docs/bpdl-spec".to_string())
        );
        assert_eq!(
            resolve_href("bpdl-spec.md", "docs/plugins.md"),
            Some("/docs/bpdl-spec".to_string())
        );
    }

    #[test]
    fn relative_md_with_fragment_preserves_fragment() {
        assert_eq!(
            resolve_href("./bpdl-spec.md#code-generation", "docs/plugins.md"),
            Some("/docs/bpdl-spec#code-generation".to_string())
        );
    }

    #[test]
    fn parent_relative_path_resolves() {
        assert_eq!(
            resolve_href("../packages/plugin-sdk/README.md", "docs/plugins.md"),
            Some("/docs/plugin-sdk".to_string())
        );
    }

    #[test]
    fn unknown_relative_md_passes_through_silently() {
        assert_eq!(
            resolve_href("./nonexistent-doc.md", "docs/plugins.md"),
            None
        );
    }

    #[test]
    fn empty_fragment_does_not_append_hash() {
        assert_eq!(
            resolve_href("./bpdl-spec.md#", "docs/plugins.md"),
            Some("/docs/bpdl-spec".to_string())
        );
    }

    #[test]
    fn escaping_parent_dir_returns_none() {
        assert_eq!(normalize_join("", "../foo"), None);
        assert_eq!(normalize_join("docs", "../../foo"), None);
    }

    #[test]
    fn parent_dir_handles_root_and_nested() {
        assert_eq!(parent_dir("README.md"), "");
        assert_eq!(parent_dir("docs/plugins.md"), "docs");
        assert_eq!(
            parent_dir("packages/plugin-sdk/README.md"),
            "packages/plugin-sdk"
        );
    }

    #[test]
    fn normalize_join_collapses_dots() {
        assert_eq!(
            normalize_join("docs", "./sibling.md"),
            Some("docs/sibling.md".to_string())
        );
        assert_eq!(
            normalize_join("docs", "../README.md"),
            Some("README.md".to_string())
        );
        assert_eq!(
            normalize_join("a/b/c", "../../d/e.md"),
            Some("a/d/e.md".to_string())
        );
    }
}
