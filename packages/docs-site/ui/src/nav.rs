//! Navigation data structures for the docs sidebar.
//!
//! The actual content is derived from [`crate::doc_pages::DOC_PAGES`] so
//! the sidebar stays in sync with the router automatically.

use std::sync::LazyLock;

use crate::doc_pages::{DOC_PAGES, NavSectionId};

/// A section in the sidebar navigation.
pub struct NavSection {
    pub title: &'static str,
    pub items: Vec<NavItem>,
}

/// A single navigation item (link) in the sidebar.
pub struct NavItem {
    pub label: &'static str,
    pub href: &'static str,
}

/// Sidebar navigation sections, derived from [`DOC_PAGES`].
///
/// Pages are grouped by their [`NavSectionId`]; within a section the order
/// matches the order they appear in [`DOC_PAGES`]. Pages without a section
/// or nav label are skipped.
pub static SECTIONS: LazyLock<Vec<NavSection>> = LazyLock::new(|| {
    NavSectionId::ordered()
        .iter()
        .filter_map(|section_id| {
            let items: Vec<NavItem> = DOC_PAGES
                .iter()
                .filter_map(|page| {
                    let page_section = page.section?;
                    if page_section != *section_id {
                        return None;
                    }
                    let label = page.nav_label?;
                    Some(NavItem {
                        label,
                        href: page.route,
                    })
                })
                .collect();

            if items.is_empty() {
                None
            } else {
                Some(NavSection {
                    title: section_id.title(),
                    items,
                })
            }
        })
        .collect()
});
