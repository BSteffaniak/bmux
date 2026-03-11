fn production_section(source: &str) -> &str {
    source.split("\n#[cfg(test)]").next().unwrap_or(source)
}

#[test]
fn runtime_production_code_does_not_reference_bundled_plugin_ids() {
    let sources = [
        production_section(include_str!("../src/runtime/mod.rs")),
        production_section(include_str!("../src/runtime/plugin_commands.rs")),
        production_section(include_str!("../src/runtime/built_in_commands.rs")),
        production_section(include_str!("../src/runtime/plugin_host.rs")),
    ];

    for source in sources {
        assert!(
            !source.contains("bmux.permissions"),
            "production runtime source should not reference bundled plugin id bmux.permissions",
        );
        assert!(
            !source.contains("bmux.windows"),
            "production runtime source should not reference bundled plugin id bmux.windows",
        );
    }
}
