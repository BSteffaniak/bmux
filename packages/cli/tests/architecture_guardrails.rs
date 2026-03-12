fn production_section(source: &str) -> &str {
    source.split("\n#[cfg(test)]").next().unwrap_or(source)
}

fn assert_no_domain_markers(source: &str, context: &str) {
    let denied = [
        "bmux.permissions",
        "bmux.windows",
        "permission-query/v1",
        "permission-command/v1",
        "window-query/v1",
        "window-command/v1",
        "Request::NewWindow",
        "Request::ListWindows",
        "Request::KillWindow",
        "Request::SwitchWindow",
        "Request::ListPermissions",
        "Request::GrantRole",
        "Request::RevokeRole",
    ];

    for marker in denied {
        assert!(
            !source.contains(marker),
            "{context} should not contain domain marker {marker}",
        );
    }
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
        assert_no_domain_markers(source, "production runtime source");
        assert!(
            !source.contains("bmux_clipboard::"),
            "production runtime source should not directly reference clipboard backend crate APIs",
        );
        assert!(
            !source.contains("clipboard-command/v1"),
            "production runtime source should not retain deprecated clipboard service interface clipboard-command/v1",
        );
    }
}

#[test]
fn core_packages_do_not_reference_domain_plugin_markers() {
    let core_sources = [
        (
            "packages/server/src/lib.rs",
            include_str!("../../server/src/lib.rs"),
        ),
        (
            "packages/server/src/persistence.rs",
            include_str!("../../server/src/persistence.rs"),
        ),
        (
            "packages/client/src/lib.rs",
            include_str!("../../client/src/lib.rs"),
        ),
        (
            "packages/ipc/src/lib.rs",
            include_str!("../../ipc/src/lib.rs"),
        ),
        (
            "packages/session/src/lib.rs",
            include_str!("../../session/src/lib.rs"),
        ),
        (
            "packages/session/models/src/lib.rs",
            include_str!("../../session/models/src/lib.rs"),
        ),
        (
            "packages/terminal/src/lib.rs",
            include_str!("../../terminal/src/lib.rs"),
        ),
        (
            "packages/terminal/models/src/lib.rs",
            include_str!("../../terminal/models/src/lib.rs"),
        ),
        (
            "packages/event/src/lib.rs",
            include_str!("../../event/src/lib.rs"),
        ),
        (
            "packages/event/models/src/lib.rs",
            include_str!("../../event/models/src/lib.rs"),
        ),
    ];

    for (path, source) in core_sources {
        assert_no_domain_markers(production_section(source), path);
    }
}
