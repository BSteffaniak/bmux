//! Validate the shipped manifest through the actual production CLI registry,
//! including clap assembly. No reimplementation of command nesting validation.
#[path = "../../../packages/cli/src/runtime/plugin_commands.rs"]
pub mod production_registry;

use bmux_config::BmuxConfig;
use bmux_plugin::PluginRegistry;
use clap::CommandFactory;
use production_registry::PluginCommandRegistry;

#[test]
fn presentation_namespace_builds_in_real_cli_registry_without_leaf_collision() {
    let mut plugins = PluginRegistry::new();
    plugins
        .register_bundled_manifest(include_str!("../plugin.toml"))
        .unwrap();
    let mut config = BmuxConfig::default();
    config.plugins.enabled = vec!["bmux.presentation".into()];
    let registry = PluginCommandRegistry::build(&config, &plugins).unwrap();
    let mut cli = registry
        .augment_clap_command(bmux_cli_schema::Cli::command())
        .unwrap();
    cli.clone().debug_assert();
    for operation in ["play", "host", "join", "check"] {
        let resolved = registry
            .resolve(&["present".into(), operation.into(), "deck.md".into()])
            .unwrap();
        assert_eq!(resolved.command_name, format!("present-{operation}"));
        assert_eq!(resolved.arguments, ["deck.md"]);
        assert!(
            cli.try_get_matches_from_mut(["bmux", "present", operation, "deck.md"])
                .is_ok()
        );
    }
    assert!(registry.resolve_exact_path(&["present".into()]).is_none());
    assert!(
        registry
            .resolve(&["present".into(), "deck.md".into()])
            .is_none()
    );
    assert!(
        cli.try_get_matches_from_mut(["bmux", "present", "deck.md"])
            .is_err()
    );
}
