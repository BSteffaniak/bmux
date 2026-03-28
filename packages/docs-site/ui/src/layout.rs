//! Layout components: nav bar, sidebar, docs page wrapper.

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
            div
                direction=row
                align-items=center
                padding-x=(if_responsive("mobile").then::<i32>(16).or_else(24))
            {
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
                col-gap=(if_responsive("mobile").then::<i32>(16).or_else(24))
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
            direction=column
            width=240
            min-width=240
            background=#010409
            border-right="1, #21262d"
            padding-y=24
            overflow-y=auto
            hidden=(if_responsive("mobile").then::<bool>(true).or_else(false))
        {
            @for section in sections {
                (section)
            }
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
        {
            (sidebar(current_path))
            div
                flex-grow=1
                min-height=0
                overflow-y=auto
            {
                div
                    padding=(if_responsive("mobile").then::<i32>(16).or_else(48))
                    max-width=900
                {
                    h1
                        color=(text_primary())
                        font-size=32
                        font-family=(MONO_FONT)
                        margin-bottom=24
                        padding-bottom=16
                        border-bottom="1, #21262d"
                    {
                        (title)
                    }
                    div color=(text_secondary()) font-family=(MONO_FONT) font-size=14 {
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
