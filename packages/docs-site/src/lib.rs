//! bmux documentation website.
//!
//! Uses the HyperChad framework to serve the bmux documentation as a web
//! application with SPA-like navigation.

use std::sync::LazyLock;

use hyperchad::app::{App, AppBuilder, renderer::DefaultRenderer};
use hyperchad::color::Color;
use hyperchad::router::Router;
use serde_json::json;

static BACKGROUND_COLOR: LazyLock<Color> = LazyLock::new(|| Color::from_hex("#0d1117"));

/// Default viewport meta tag for responsive design.
pub static VIEWPORT: LazyLock<String> = LazyLock::new(|| "width=device-width".to_string());

#[cfg(feature = "assets")]
static CARGO_MANIFEST_DIR: LazyLock<Option<std::path::PathBuf>> =
    LazyLock::new(|| std::option_env!("CARGO_MANIFEST_DIR").map(Into::into));

/// Application router with all configured routes.
pub static ROUTER: LazyLock<Router> = LazyLock::new(|| {
    Router::new()
        // Home
        .with_static_route(&["/", "/home"], |_| async {
            bmux_docs_site_ui::pages::home::home()
        })
        // Docs — Getting Started
        .with_static_route(&["/docs"], |_| async {
            bmux_docs_site_ui::pages::docs::overview()
        })
        .with_static_route(&["/docs/installation"], |_| async {
            bmux_docs_site_ui::pages::docs::installation()
        })
        .with_static_route(&["/docs/quickstart"], |_| async {
            bmux_docs_site_ui::pages::docs::quickstart()
        })
        // Docs — Reference
        .with_static_route(&["/docs/cli"], |_| async {
            bmux_docs_site_ui::pages::docs::cli()
        })
        .with_static_route(&["/docs/playbooks"], |_| async {
            bmux_docs_site_ui::pages::docs::playbooks()
        })
        .with_static_route(&["/docs/images"], |_| async {
            bmux_docs_site_ui::pages::docs::images()
        })
        .with_static_route(&["/docs/config"], |_| async {
            bmux_docs_site_ui::pages::docs::config()
        })
        // Docs — Plugins
        .with_static_route(&["/docs/plugins"], |_| async {
            bmux_docs_site_ui::pages::docs::plugins()
        })
        .with_static_route(&["/docs/plugin-sdk"], |_| async {
            bmux_docs_site_ui::pages::docs::plugin_sdk()
        })
        .with_static_route(&["/docs/plugin-example"], |_| async {
            bmux_docs_site_ui::pages::docs::plugin_example()
        })
        // Docs — Development
        .with_static_route(&["/docs/testing"], |_| async {
            bmux_docs_site_ui::pages::docs::testing()
        })
        // 404
        .with_static_route(&["/not-found"], |_| async {
            bmux_docs_site_ui::pages::docs::not_found()
        })
        // Health check
        .with_route(&["/health"], |_| async {
            json!({
                "healthy": true,
                "hash": std::env!("GIT_HASH"),
            })
        })
});

#[cfg(feature = "assets")]
static ASSETS_DIR: LazyLock<std::path::PathBuf> = LazyLock::new(|| {
    CARGO_MANIFEST_DIR.as_ref().map_or_else(
        || <std::path::PathBuf as std::str::FromStr>::from_str("public").unwrap(),
        |dir| dir.join("public"),
    )
});

#[cfg(feature = "assets")]
pub static ASSETS: LazyLock<Vec<hyperchad::renderer::assets::StaticAssetRoute>> =
    LazyLock::new(|| {
        vec![
            #[cfg(feature = "vanilla-js")]
            hyperchad::renderer::assets::StaticAssetRoute {
                route: format!(
                    "js/{}",
                    hyperchad::renderer_vanilla_js::SCRIPT_NAME_HASHED.as_str()
                ),
                target: hyperchad::renderer::assets::AssetPathTarget::FileContents(
                    hyperchad::renderer_vanilla_js::SCRIPT.as_bytes().into(),
                ),
                not_found_behavior: None,
            },
            hyperchad::renderer::assets::StaticAssetRoute {
                route: "public".to_string(),
                target: ASSETS_DIR.clone().try_into().unwrap(),
                not_found_behavior: None,
            },
        ]
    });

/// Initialize the application builder with default configuration.
#[must_use]
pub fn init() -> AppBuilder {
    #[allow(unused_mut)]
    let mut app = AppBuilder::new()
        .with_router(ROUTER.clone())
        .with_background(*BACKGROUND_COLOR)
        .with_title("bmux docs".to_string())
        .with_description("Documentation for bmux — a modern terminal multiplexer".to_string())
        .with_size(1100.0, 700.0);

    #[cfg(feature = "assets")]
    for assets in ASSETS.iter().cloned() {
        app.static_asset_route_result(assets).unwrap();
    }

    app
}

/// Build the application from the provided builder.
///
/// # Errors
///
/// Returns an error if the application fails to build.
pub fn build_app(builder: AppBuilder) -> Result<App<DefaultRenderer>, hyperchad::app::Error> {
    use hyperchad::renderer::Renderer as _;

    #[allow(unused_mut)]
    let mut app = builder.build_default()?;

    app.renderer.add_responsive_trigger(
        "mobile".into(),
        hyperchad::renderer::transformer::ResponsiveTrigger::MaxWidth(
            hyperchad::renderer::transformer::Number::Integer(600),
        ),
    );
    app.renderer.add_responsive_trigger(
        "tablet".into(),
        hyperchad::renderer::transformer::ResponsiveTrigger::MaxWidth(
            hyperchad::renderer::transformer::Number::Integer(900),
        ),
    );

    Ok(app)
}
