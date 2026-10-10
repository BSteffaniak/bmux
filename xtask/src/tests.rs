use super::{artifacts, run};
use serde_json::{Value, json};
use std::{
    fs,
    path::{Path, PathBuf},
};

fn root() -> tempfile::TempDir {
    tempfile::tempdir_in(std::env::current_dir().unwrap()).unwrap()
}
fn write_executable(path: &Path, bytes: &[u8]) {
    fs::write(path, bytes).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }
}
fn fixture(root: &Path) -> (PathBuf, Vec<Value>) {
    let plugin = root.join("plugins/fixture");
    fs::create_dir_all(&plugin).unwrap();
    fs::write(plugin.join("plugin.toml"), "id = 'fixture.owner'\nname = 'Fixture'\nversion = '1.0.0'\n[native_abi]\nminimum = '1.1'\nmaximum = '1.1'\n[[auxiliary_executables]]\nid = 'helper'\ncargo_package = 'fixture_package'\ncargo_binary = 'fixture-helper'\nrelative_path = 'fixture-helper'\nprotocol_version = 1\n").unwrap();
    let build = root.join("build");
    fs::create_dir(&build).unwrap();
    for name in ["bmux", "bmux.exe", "xtask", "xtask.exe", "fixture-helper"] {
        write_executable(&build.join(name), format!("fixture:{name}").as_bytes());
    }
    (build, artifacts::declarations(root).unwrap())
}
fn staged(root: &Path) -> PathBuf {
    let (build, declarations) = fixture(root);
    let stage = root.join("stage");
    artifacts::stage(&build, &stage, "x86_64-unknown-linux-gnu", &declarations).unwrap();
    stage
}
fn inventory(source: &Path) -> Value {
    serde_json::from_slice(&fs::read(source.join("artifacts.json")).unwrap()).unwrap()
}
fn change_inventory(source: &Path, mutation: impl FnOnce(&mut Value)) {
    let mut value = inventory(source);
    mutation(&mut value);
    fs::write(
        source.join("artifacts.json"),
        serde_json::to_vec(&value).unwrap(),
    )
    .unwrap();
}

#[test]
fn stage_install_slots_and_platform_policy() {
    let root = root();
    let (build, declarations) = fixture(root.path());
    let stage = root.path().join("stage");
    artifacts::stage(&build, &stage, "x86_64-unknown-linux-gnu", &declarations).unwrap();
    let value = inventory(&stage);
    assert_eq!(value["artifacts"].as_array().unwrap().len(), 3);
    assert_eq!(value["artifacts"][2]["declaration"]["protocol_version"], 1);
    assert!(stage.join("bmux-install").is_file());
    assert!(!stage.join("install-artifacts.py").exists());
    let slot = root.path().join("slot");
    artifacts::install(&stage, &slot, Some("bmux-test")).unwrap();
    assert!(slot.join("bmux-test").is_file());
    assert!(slot.join("fixture-helper").is_file());
    assert_eq!(inventory(&slot), value);
    let installed: Value =
        serde_json::from_slice(&fs::read(slot.join("installation.json")).unwrap()).unwrap();
    assert_eq!(installed["binary_name"], "bmux-test");
    assert!(artifacts::install(&stage, &slot, Some("bmux-test")).is_err());
    let windows = root.path().join("windows");
    artifacts::stage(&build, &windows, "x86_64-pc-windows-msvc", &declarations).unwrap();
    assert!(!windows.join("fixture-helper").exists());
    assert_eq!(
        inventory(&windows)["unavailable_auxiliary_executables"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    artifacts::install(
        &windows,
        &root.path().join("windows-slot"),
        Some("bmux-test"),
    )
    .unwrap();
    assert!(root.path().join("windows-slot/bmux-test.exe").is_file());
    assert!(artifacts::unix_target("unrelated-unknown-linux-malicious").is_err());
    let config = root.path().join("nfpm.yaml");
    fs::write(
        &config,
        "contents:\n  - src: bmux\n    dst: /usr/bin/bmux\n",
    )
    .unwrap();
    let generated = root.path().join("generated.yaml");
    artifacts::nfpm(&config, &generated, &declarations).unwrap();
    let text = fs::read_to_string(generated).unwrap();
    assert!(text.contains("dst: /usr/bin/fixture-helper"));
    assert!(text.contains("mode: 0755"));
}

#[test]
fn declaration_rejects_unsafe_duplicate_reserved_unknown_and_zero_protocol() {
    let root = root();
    fixture(root.path());
    let path = root.path().join("plugins/fixture/plugin.toml");
    let original = fs::read_to_string(&path).unwrap();
    for replacement in [
        "../escape",
        "/absolute",
        "bmux",
        "BMUX",
        "BMUX-INSTALL",
        "ARTIFACTS.JSON",
        "INSTALLATION.JSON",
        "INSTALL-ARTIFACTS.PY",
        "artifacts.json",
        "install-artifacts.py",
    ] {
        fs::write(
            &path,
            original.replace(
                "relative_path = 'fixture-helper'",
                &format!("relative_path = '{replacement}'"),
            ),
        )
        .unwrap();
        assert!(
            artifacts::declarations(root.path()).is_err(),
            "{replacement}"
        );
    }
    for replacement in [
        original.replace("protocol_version = 1", "protocol_version = 0"),
        original.replace("protocol_version = 1", "protocol_version = -1"),
        format!("{original}unknown = true\n"),
        original.replace("minimum = '1.1'", "minimum = '1.0'"),
    ] {
        fs::write(&path, replacement).unwrap();
        assert!(artifacts::declarations(root.path()).is_err());
    }
    fs::write(&path, &original).unwrap();
    let other = root.path().join("plugins/other");
    fs::create_dir(&other).unwrap();
    fs::write(
        other.join("plugin.toml"),
        original.replace("fixture.owner", "fixture.other"),
    )
    .unwrap();
    assert!(artifacts::declarations(root.path()).is_err());
    fs::write(
        other.join("plugin.toml"),
        original.replace("fixture.owner", "fixture.other").replace(
            "relative_path = 'fixture-helper'",
            "relative_path = 'FIXTURE-HELPER'",
        ),
    )
    .unwrap();
    assert!(artifacts::declarations(root.path()).is_err());
}

#[test]
fn corrupt_and_missing_artifacts_never_publish_partial_installations() {
    let root = root();
    let stage = staged(root.path());
    let destination = root.path().join("bad");
    fs::write(stage.join("fixture-helper"), "corrupt").unwrap();
    assert!(artifacts::install(&stage, &destination, None).is_err());
    assert!(!destination.exists());
    fs::remove_file(stage.join("fixture-helper")).unwrap();
    assert!(artifacts::install(&stage, &destination, None).is_err());
    assert!(!destination.exists());
    let (build, declarations) = {
        let another = root.path().join("another");
        fs::create_dir(&another).unwrap();
        fixture(&another)
    };
    fs::remove_file(build.join("fixture-helper")).unwrap();
    assert!(
        artifacts::stage(
            &build,
            &destination,
            "x86_64-unknown-linux-gnu",
            &declarations
        )
        .is_err()
    );
    assert!(!destination.exists());
}

#[test]
fn inventory_rejects_traversal_duplicates_unknown_schema_and_wrong_binary() {
    let root = root();
    let stage = staged(root.path());
    let original = fs::read(stage.join("artifacts.json")).unwrap();
    for name in [
        "../escape",
        "a/b",
        "/absolute",
        "artifacts.json",
        "installation.json",
        "install-artifacts.py",
        "ARTIFACTS.JSON",
        "INSTALLATION.JSON",
        "INSTALL-ARTIFACTS.PY",
        "BMUX",
    ] {
        change_inventory(&stage, |value| {
            value["artifacts"][1]["relative_path"] = json!(name)
        });
        assert!(artifacts::install(&stage, &root.path().join("bad"), None).is_err());
        assert!(!root.path().join("bad").exists());
        fs::write(stage.join("artifacts.json"), &original).unwrap();
    }
    for mutation in 0..5 {
        change_inventory(&stage, |value| match mutation {
            0 => value["schema_version"] = json!(2),
            1 => value["target"] = json!("x86_64-pc-windows-msvc"),
            2 => value["artifacts"] = json!([]),
            3 => value["artifacts"][0]["bytes"] = json!(999),
            _ => value["artifacts"][0]["sha256"] = json!("0".repeat(64)),
        });
        assert!(artifacts::verify(&stage).is_err());
        fs::write(stage.join("artifacts.json"), &original).unwrap();
    }
    fs::remove_file(stage.join("artifacts.json")).unwrap();
    assert!(artifacts::install(&stage, &root.path().join("legacy"), None).is_err());
}

#[cfg(unix)]
#[test]
fn symlink_files_inventory_and_parent_directories_are_refused() {
    use std::os::unix::fs::symlink;
    let root = root();
    let stage = staged(root.path());
    let destination = root.path().join("bad");
    let helper = stage.join("fixture-helper");
    fs::rename(&helper, stage.join("real-helper")).unwrap();
    symlink("real-helper", &helper).unwrap();
    assert!(artifacts::install(&stage, &destination, None).is_err());
    fs::remove_file(&helper).unwrap();
    fs::rename(stage.join("real-helper"), &helper).unwrap();
    let manifest = stage.join("artifacts.json");
    fs::rename(&manifest, stage.join("real-inventory")).unwrap();
    symlink("real-inventory", &manifest).unwrap();
    assert!(artifacts::verify(&stage).is_err());
    fs::remove_file(&manifest).unwrap();
    fs::rename(stage.join("real-inventory"), &manifest).unwrap();
    symlink(&stage, root.path().join("linked-source")).unwrap();
    assert!(artifacts::install(&root.path().join("linked-source"), &destination, None).is_err());
    let outside = root.path().join("outside");
    fs::create_dir(&outside).unwrap();
    symlink(&outside, root.path().join("linked-parent")).unwrap();
    assert!(artifacts::install(&stage, &root.path().join("linked-parent/output"), None).is_err());
    assert!(!outside.join("output").exists());
    symlink(&outside, &destination).unwrap();
    assert!(artifacts::install(&stage, &destination, None).is_err());
}

#[cfg(unix)]
#[test]
fn immutable_activation_preserves_old_versions_and_failed_input_preserves_pointer() {
    let root = root();
    let stage = staged(root.path());
    let prefix = root.path().join("prefix");
    artifacts::activate(&stage, &prefix, "test-version").unwrap();
    let link = prefix.join("bin/bmux");
    let original = fs::read_link(&link).unwrap();
    assert_eq!(
        fs::read(original.with_file_name("fixture-helper")).unwrap(),
        b"fixture:fixture-helper"
    );
    assert!(artifacts::activate(&stage, &prefix, "test-version").is_err());
    assert_eq!(fs::read_link(&link).unwrap(), original);
    artifacts::activate(&stage, &prefix, "second-version").unwrap();
    let second = fs::read_link(&link).unwrap();
    assert_ne!(second, original);
    assert_eq!(fs::read(&original).unwrap(), b"fixture:bmux");
    fs::write(stage.join("fixture-helper"), "corrupt").unwrap();
    assert!(artifacts::activate(&stage, &prefix, "bad-version").is_err());
    assert_eq!(fs::read_link(&link).unwrap(), second);
    assert!(!prefix.join("lib/bmux/versions/bad-version").exists());
    assert!(artifacts::activate(&stage, &prefix, "../escape").is_err());
}

#[cfg(unix)]
#[test]
fn activation_lock_and_symlink_prefix_are_not_bypassed() {
    use std::os::unix::fs::symlink;
    let root = root();
    let stage = staged(root.path());
    let prefix = root.path().join("prefix");
    fs::create_dir_all(prefix.join("lib/bmux/versions/.install-lock")).unwrap();
    assert!(artifacts::activate(&stage, &prefix, "version").is_err());
    assert!(prefix.join("lib/bmux/versions/.install-lock").is_dir());
    assert!(!prefix.join("bin/bmux").exists());
    symlink(&prefix, root.path().join("linked-prefix")).unwrap();
    assert!(artifacts::activate(&stage, &root.path().join("linked-prefix"), "version").is_err());
}

#[test]
fn occupied_destinations_and_invalid_slots_are_preserved() {
    let root = root();
    let stage = staged(root.path());
    let destination = root.path().join("occupied");
    fs::create_dir(&destination).unwrap();
    fs::write(destination.join("keep"), "untouched").unwrap();
    assert!(artifacts::install(&stage, &destination, None).is_err());
    assert_eq!(fs::read(destination.join("keep")).unwrap(), b"untouched");
    for name in ["../escape", "bmux-", "arbitrary", "bmux-a/b"] {
        assert!(artifacts::install(&stage, &root.path().join("slot"), Some(name)).is_err());
        assert!(!root.path().join("slot").exists());
    }
}

#[test]
fn cli_rejects_unknown_duplicate_conflicting_and_missing_flags() {
    for args in [
        vec!["install", "source"],
        vec!["install", "source", "dest", "--prefix", "other"],
        vec![
            "install",
            "source",
            "dest",
            "--version",
            "v1",
            "--binary-name",
            "bmux-slot",
        ],
        vec!["auxiliary", "stage"],
        vec!["auxiliary", "stage", "--target", "bad", "--target", "bad"],
        vec!["auxiliary", "stage", "--unknown", "value"],
    ] {
        assert!(run(args.into_iter().map(str::to_string).collect()).is_err());
    }
}

#[cfg(unix)]
#[test]
fn packaged_installer_accepts_explicit_install_and_implicit_install() {
    let root = root();
    let stage = staged(root.path());
    let prefix = root.path().join("prefix");
    let explicit = vec![
        "install".to_string(),
        stage.to_str().unwrap().to_string(),
        "--prefix".to_string(),
        prefix.to_str().unwrap().to_string(),
        "--version".to_string(),
        "explicit".to_string(),
    ];
    let parsed = super::invocation_args(Some("/release/bmux-install"), explicit.clone());
    assert_eq!(parsed, explicit);
    run(parsed).unwrap();
    assert!(prefix.join("lib/bmux/versions/explicit/bmux").is_file());
    let mut implicit = explicit[1..].to_vec();
    *implicit.last_mut().unwrap() = "implicit".to_string();
    run(super::invocation_args(
        Some("/release/bmux-install"),
        implicit,
    ))
    .unwrap();
    assert!(prefix.join("lib/bmux/versions/implicit/bmux").is_file());
    assert_eq!(
        super::invocation_args(Some("xtask"), explicit.clone()),
        explicit
    );
}

#[test]
fn unlisted_members_and_empty_existing_destination() {
    let root = root();
    let stage = staged(root.path());
    fs::write(stage.join("unexpected"), "untrusted").unwrap();
    assert!(artifacts::verify(&stage).is_err());
    fs::remove_file(stage.join("unexpected")).unwrap();
    let destination = root.path().join("empty");
    fs::create_dir(&destination).unwrap();
    artifacts::install(&stage, &destination, None).unwrap();
    assert!(destination.join("bmux-install").exists());
}

#[cfg(unix)]
#[test]
fn non_executable_build_artifact_is_not_published() {
    use std::os::unix::fs::PermissionsExt;
    let root = root();
    let (build, declarations) = fixture(root.path());
    fs::set_permissions(
        build.join("fixture-helper"),
        fs::Permissions::from_mode(0o644),
    )
    .unwrap();
    let stage = root.path().join("stage");
    assert!(artifacts::stage(&build, &stage, "x86_64-unknown-linux-gnu", &declarations).is_err());
    assert!(!stage.exists());
}
