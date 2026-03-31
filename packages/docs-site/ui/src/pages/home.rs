//! Home page — landing page with no sidebar.

use hyperchad::actions::logic::if_responsive;
use hyperchad::template::{Containers, container};
use hyperchad::transformer::models::LayoutDirection;

use crate::layout;

/// The landing page for bmux docs site.
#[must_use]
pub fn home() -> Containers {
    layout::page(&container! {
        div
            flex-grow=1
            justify-content=center
            align-items=center
            padding-x=(if_responsive("tablet").then::<i32>(24).or_else(80))
            padding-y=(if_responsive("mobile").then::<i32>(48).or_else(80))
        {
            // Hero section
            div
                align-items=center
                max-width=800
                gap=(if_responsive("mobile").then::<i32>(24).or_else(32))
            {
                // Terminal prompt branding
                div
                    color=(layout::green())
                    font-size=(if_responsive("tablet").then::<i32>(40).or_else(64))
                    font-family=(layout::MONO_FONT)
                    text-align=center
                {
                    ">_ bmux"
                }

                // Tagline
                h1
                    color=(layout::text_primary())
                    font-size=(if_responsive("tablet").then::<i32>(22).or_else(32))
                    font-family=(layout::MONO_FONT)
                    text-align=center
                    margin-bottom=16
                {
                    "A modern terminal multiplexer"
                }

                div
                    color=(layout::text_muted())
                    font-size=(if_responsive("mobile").then::<i32>(14).or_else(16))
                    font-family=(layout::MONO_FONT)
                    text-align=center
                    max-width=600
                {
                    "Built in Rust. Plugin-driven. Multi-client sessions with independent views, modal interaction, and deep customization."
                }

                // CTA buttons
                div
                    direction=(
                        if_responsive("mobile")
                            .then::<LayoutDirection>(LayoutDirection::Column)
                            .or_else(LayoutDirection::Row)
                    )
                    gap=16
                    margin-top=24
                    align-items=center
                {
                    anchor
                        color=#0d1117
                        background=(layout::green())
                        border-radius=6
                        padding-x=24
                        padding-y=12
                        text-decoration="none"
                        font-family=(layout::MONO_FONT)
                        font-size=14
                        href="/docs"
                    {
                        "read the docs"
                    }
                    anchor
                        color=(layout::text_secondary())
                        background=#21262d
                        border-radius=6
                        padding-x=24
                        padding-y=12
                        text-decoration="none"
                        font-family=(layout::MONO_FONT)
                        font-size=14
                        href="https://github.com/BSteffaniak/bmux"
                        target="_blank"
                    {
                        "view on github"
                    }
                }
            }

            // Feature cards
            div
                direction=(
                    if_responsive("tablet")
                        .then::<LayoutDirection>(LayoutDirection::Column)
                        .or_else(LayoutDirection::Row)
                )
                gap=24
                margin-top=(if_responsive("mobile").then::<i32>(48).or_else(80))
                max-width=900
            {
                (feature_card(
                    "plugin-driven",
                    "Extensibility is built in, not bolted on. Plugins are first-class architecture."
                ))
                (feature_card(
                    "multi-client",
                    "Multiple clients attach to the same session with independent views and roles."
                ))
                (feature_card(
                    "modal & fast",
                    "Keyboard-driven with Vim-style navigation. Written in Rust for performance."
                ))
            }
        }
    })
}

fn feature_card(title: &str, description: &str) -> Containers {
    container! {
        div
            background=(layout::surface())
            border-radius=8
            padding=(if_responsive("mobile").then::<i32>(20).or_else(24))
            flex=1
            border-left="2, #7ee787"
        {
            h3
                color=(layout::text_primary())
                font-size=(if_responsive("mobile").then::<i32>(14).or_else(16))
                font-family=(layout::MONO_FONT)
                margin-bottom=8
            {
                (title)
            }
            div
                color=(layout::text_muted())
                font-size=(if_responsive("mobile").then::<i32>(12).or_else(13))
                font-family=(layout::MONO_FONT)
            {
                (description)
            }
        }
    }
}
