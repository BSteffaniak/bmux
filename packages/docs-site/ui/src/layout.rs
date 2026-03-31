//! Layout components: nav bar, sidebar, docs page wrapper.

use hyperchad::actions::ActionType;
use hyperchad::actions::logic::if_responsive;
use hyperchad::color::Color;
use hyperchad::template::{Container, Containers, container};
use hyperchad::transformer::models::LayoutDirection;

use crate::nav;

/// Terminal green accent color (#7ee787 — can't use hex literal because `e` parses as exponent).
pub fn green() -> Color {
    Color::from_hex("#7ee787")
}

/// Light text color.
pub fn text_primary() -> Color {
    Color::from_hex("#f0f6fc")
}

/// Muted text color.
pub fn text_secondary() -> Color {
    Color::from_hex("#c9d1d9")
}

/// Muted/dim text color.
pub fn text_muted() -> Color {
    Color::from_hex("#8b949e")
}

/// Dark surface background.
pub fn surface() -> Color {
    Color::from_hex("#161b22")
}

/// Border color.
pub fn border() -> Color {
    Color::from_hex("#21262d")
}

// ── Shared constants ────────────────────────────────────────────────────────

pub const MONO_FONT: &str = "'SF Mono', 'Cascadia Code', 'Fira Code', Menlo, Consolas, monospace";

/// Sidebar element ID used for toggle actions.
const SIDEBAR_ID: &str = "docs-sidebar";

/// Backdrop element ID used for dismissing the sidebar.
const BACKDROP_ID: &str = "docs-backdrop";

// ── Top navigation bar ──────────────────────────────────────────────────────

/// Top navigation bar shown on every page.
#[must_use]
pub fn nav_bar() -> Containers {
    container! {
        header
            direction=row
            align-items=center
            background=#0d1117
            border-bottom="1, #21262d"
            padding-y=12
        {
            // Left: hamburger (mobile only) + logo
            div
                direction=row
                align-items=center
                padding-x=(if_responsive("mobile").then::<i32>(16).or_else(24))
                gap=(if_responsive("mobile").then::<i32>(12).or_else(0))
            {
                // Hamburger button — visible on mobile only
                div
                    hidden=(if_responsive("mobile").then::<bool>(false).or_else(true))
                    fx-click=(ActionType::Multi(vec![
                        ActionType::display_by_id(SIDEBAR_ID),
                        ActionType::display_by_id(BACKDROP_ID),
                    ]))
                    cursor="pointer"
                    color=(text_secondary())
                    font-size=20
                    padding=4
                    user-select="none"
                {
                    "\u{2630}"
                }
                anchor
                    color=(green())
                    direction=row
                    align-items=center
                    text-decoration="none"
                    href="/"
                {
                    span font-size=18 font-family=(MONO_FONT) {
                        ">_ bmux"
                    }
                }
            }
            div
                direction=row
                align-items=center
                justify-content=end
                flex=1
                padding-x=(if_responsive("mobile").then::<i32>(16).or_else(24))
                gap=(if_responsive("mobile").then::<i32>(16).or_else(24))
            {
                anchor
                    color=(text_secondary())
                    text-decoration="none"
                    font-family=(MONO_FONT)
                    font-size=14
                    href="/docs"
                {
                    "docs"
                }
                anchor
                    color=(text_secondary())
                    text-decoration="none"
                    font-family=(MONO_FONT)
                    font-size=14
                    href="https://github.com/BSteffaniak/bmux"
                    target="_blank"
                {
                    "github"
                }
            }
        }
    }
}

// ── Sidebar ─────────────────────────────────────────────────────────────────

/// Docs sidebar with section navigation.
///
/// On desktop/tablet the sidebar is a static column (240 px on desktop, 200 px
/// on tablet widths). On mobile it renders as a fixed overlay drawer that starts
/// hidden and is toggled via the hamburger button in the nav bar.
#[must_use]
pub fn sidebar(current_path: &str) -> Containers {
    let mut sections = Vec::new();

    for section in nav::SECTIONS {
        let mut items = Vec::new();
        for item in section.items {
            let is_active = current_path == item.href;
            items.push(sidebar_item(item.label, item.href, is_active));
        }
        sections.push(sidebar_section(section.title, &items));
    }

    container! {
        aside
            #docs-sidebar
            direction=column
            // Tablet: 200px, desktop: 240px. On mobile this is overridden to
            // 280px via the fixed overlay below.
            width=(if_responsive("tablet").then::<i32>(200).or_else(240))
            min-width=(if_responsive("tablet").then::<i32>(200).or_else(240))
            background=#010409
            border-right="1, #21262d"
            padding-y=24
            overflow-y=auto
            // Mobile: fixed overlay, hidden by default (display=none).
            // Desktop/tablet: normal flow, always visible.
            position=(
                if_responsive("mobile")
                    .then::<hyperchad::transformer::models::Position>(
                        hyperchad::transformer::models::Position::Fixed
                    )
                    .or_else(hyperchad::transformer::models::Position::Static)
            )
            top=0
            left=0
            height=100%
            hidden=(if_responsive("mobile").then::<bool>(true).or_else(false))
        {
            // Close button at top of mobile drawer
            div
                hidden=(if_responsive("mobile").then::<bool>(false).or_else(true))
                direction=row
                justify-content=end
                padding-x=16
                padding-bottom=8
            {
                div
                    fx-click=(ActionType::Multi(vec![
                        ActionType::no_display_by_id(SIDEBAR_ID),
                        ActionType::no_display_by_id(BACKDROP_ID),
                    ]))
                    cursor="pointer"
                    color=(text_muted())
                    font-size=18
                    padding=4
                    user-select="none"
                {
                    "\u{2715}"
                }
            }
            @for section in sections {
                (section)
            }
        }
    }
}

/// Backdrop overlay that dismisses the mobile sidebar when clicked.
#[must_use]
fn backdrop() -> Containers {
    container! {
        div
            #docs-backdrop
            hidden=true
            position=fixed
            top=0
            left=0
            width=100%
            height=100%
            background="rgba(0,0,0,0.5)"
            fx-click=(ActionType::Multi(vec![
                ActionType::no_display_by_id(SIDEBAR_ID),
                ActionType::no_display_by_id(BACKDROP_ID),
            ]))
        {
        }
    }
}

fn sidebar_section(title: &str, items: &[Container]) -> Containers {
    container! {
        div padding-x=16 margin-bottom=16 {
            div
                color=(text_muted())
                font-size=11
                font-family=(MONO_FONT)
                margin-bottom=8
                padding-x=8
            {
                (title.to_uppercase())
            }
            @for item in items {
                (item)
            }
        }
    }
}

fn sidebar_item(label: &str, href: &str, active: bool) -> Container {
    if active {
        container! {
            anchor
                color=(green())
                text-decoration="none"
                font-family=(MONO_FONT)
                font-size=13
                padding-y=4
                padding-x=8
                border-radius=4
                background=(surface())
                href=(href)
            {
                (label)
            }
        }
    } else {
        container! {
            anchor
                color=(text_secondary())
                text-decoration="none"
                font-family=(MONO_FONT)
                font-size=13
                padding-y=4
                padding-x=8
                href=(href)
            {
                (label)
            }
        }
    }
    .into()
}

// ── Docs page layout (sidebar + content) ────────────────────────────────────

/// Full docs page layout with sidebar and content area.
#[must_use]
pub fn docs_layout(current_path: &str, title: &str, content: &Containers) -> Containers {
    page(&container! {
        div
            direction=(
                if_responsive("mobile")
                    .then::<LayoutDirection>(LayoutDirection::Column)
                    .or_else(LayoutDirection::Row)
            )
            flex-grow=1
            min-height=0
            position=relative
        {
            (sidebar(current_path))
            // Backdrop overlay for mobile drawer
            (backdrop())
            div
                flex-grow=1
                min-height=0
                overflow-y=auto
            {
                div
                    padding=(if_responsive("tablet").then::<i32>(24).or_else(48))
                    max-width=900
                {
                    h1
                        color=(text_primary())
                        font-size=(if_responsive("mobile").then::<i32>(24).or_else(32))
                        font-family=(MONO_FONT)
                        margin-bottom=24
                        padding-bottom=16
                        border-bottom="1, #21262d"
                    {
                        (title)
                    }
                    // Content wrapper with overflow-x for wide code blocks / tables
                    div
                        color=(text_secondary())
                        font-family=(MONO_FONT)
                        font-size=(if_responsive("mobile").then::<i32>(13).or_else(14))
                        overflow-x=auto
                    {
                        (content)
                    }
                }
            }
        }
    })
}

// ── Base page wrapper ───────────────────────────────────────────────────────

/// Full-page wrapper with nav bar and base styling.
#[must_use]
pub fn page(slot: &Containers) -> Containers {
    container! {
        div
            width=100%
            height=100%
            position=relative
            color=(text_secondary())
            background=#0d1117
            font-family=(MONO_FONT)
            overflow-x=hidden
            overflow-y=auto
        {
            (nav_bar())
            main flex-grow=1 min-height=0 {
                (slot)
            }
        }
    }
}
