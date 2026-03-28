//! Navigation data structures for the docs sidebar.

/// A section in the sidebar navigation.
pub struct NavSection {
    pub title: &'static str,
    pub items: &'static [NavItem],
}

/// A single navigation item (link) in the sidebar.
pub struct NavItem {
    pub label: &'static str,
    pub href: &'static str,
}

/// All sidebar navigation sections for the docs.
pub static SECTIONS: &[NavSection] = &[
    NavSection {
        title: "Getting Started",
        items: &[
            NavItem {
                label: "Overview",
                href: "/docs",
            },
            NavItem {
                label: "Installation",
                href: "/docs/installation",
            },
            NavItem {
                label: "Quick Start",
                href: "/docs/quickstart",
            },
        ],
    },
    NavSection {
        title: "Reference",
        items: &[
            NavItem {
                label: "CLI",
                href: "/docs/cli",
            },
            NavItem {
                label: "Playbooks",
                href: "/docs/playbooks",
            },
            NavItem {
                label: "Configuration",
                href: "/docs/config",
            },
        ],
    },
    NavSection {
        title: "Plugins",
        items: &[
            NavItem {
                label: "Plugin Architecture",
                href: "/docs/plugins",
            },
            NavItem {
                label: "Plugin SDK",
                href: "/docs/plugin-sdk",
            },
            NavItem {
                label: "Example Plugin",
                href: "/docs/plugin-example",
            },
        ],
    },
    NavSection {
        title: "Development",
        items: &[NavItem {
            label: "Testing",
            href: "/docs/testing",
        }],
    },
];
