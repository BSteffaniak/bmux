//! Wrapper-free packaging for a plugin whose BMUX dependencies come from git.
//!
//! External plugin crates (bcode pins bmux with `git = ..., branch = "master"`)
//! package themselves: the packager builds the crate directly with
//! `cargo rustc --crate-type cdylib` in the crate's own workspace, so the SDK
//! is exactly the one its Cargo.lock resolved from git. This snapshots the
//! working tree into a local `file://` git repository, packages a plugin that
//! depends on it, and inspects the artifact's embedded declaration.
#![cfg(unix)]
#[allow(dead_code)] // Reuse binary/profile selection only.
mod support;

use std::path::{Path, PathBuf};
use std::process::Command;

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("workspace root")
        .to_path_buf()
}

fn git(dir: &Path, args: &[&str]) -> Vec<u8> {
    let output = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "bmux-test")
        .env("GIT_AUTHOR_EMAIL", "bmux-test@example.invalid")
        .env("GIT_COMMITTER_NAME", "bmux-test")
        .env("GIT_COMMITTER_EMAIL", "bmux-test@example.invalid")
        // Fixed dates: the same tree always makes the same commit, so Cargo reuses
        // what it built from an unchanged snapshot instead of rebuilding the SDK.
        .env("GIT_AUTHOR_DATE", "2000-01-01T00:00:00Z")
        .env("GIT_COMMITTER_DATE", "2000-01-01T00:00:00Z")
        .output()
        .expect("run git");
    support::assert_success(&output, &format!("git {}", args.join(" ")));
    output.stdout
}

/// Snapshot the working tree (tracked and untracked, honoring ignores) into a
/// fresh repository on branch `master`.
fn snapshot_repository(workspace: &Path, repo: &Path) {
    match std::fs::remove_dir_all(repo) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => panic!("clear previous snapshot repo: {error}"),
    }
    std::fs::create_dir_all(repo).expect("create snapshot repo");
    let listing = git(
        workspace,
        &[
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
        ],
    );
    for name in listing
        .split(|byte| *byte == 0)
        .filter(|name| !name.is_empty())
    {
        let name = std::str::from_utf8(name).expect("UTF-8 path");
        let source = workspace.join(name);
        if !source.is_file() {
            continue;
        }
        let target = repo.join(name);
        std::fs::create_dir_all(target.parent().expect("parent")).expect("create parent");
        std::fs::copy(&source, &target).expect("copy source file");
    }
    git(repo, &["init", "-q", "-b", "master"]);
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "-q", "-m", "snapshot"]);
}

fn write_consumer(consumer: &Path, repo: &Path, lockfile: &Path) {
    std::fs::create_dir_all(consumer.join("src")).expect("create consumer");
    std::fs::create_dir_all(consumer.join("bpdl")).expect("create bpdl dir");
    let url = format!("file://{}", repo.display());
    std::fs::write(
        consumer.join("Cargo.toml"),
        format!(
            r#"[package]
name = "git_sourced_plugin"
version = "0.1.0"
edition = "2024"
publish = false

[lib]
crate-type = ["rlib"]

[dependencies]
bmux_plugin_sdk = {{ git = "{url}", branch = "master" }}
bmux_plugin_schema_macros = {{ git = "{url}", branch = "master" }}

[features]
dynamic-backend = []
dynamic-frontend = []

[workspace]
"#
        ),
    )
    .expect("write consumer manifest");
    std::fs::write(
        consumer.join("plugin.toml"),
        "component_roles = [\"backend\"]\nid = \"example.git-sourced\"\nname = \"Git sourced\"\nversion = \"0.1.0\"\n",
    )
    .expect("write settings");
    std::fs::write(
        consumer.join("bpdl/git-sourced.bpdl"),
        "plugin example.git-sourced version 1;\ncommand ping() title \"Ping\";\nworld git-sourced {}\n",
    )
    .expect("write bpdl");
    std::fs::write(
        consumer.join("src/lib.rs"),
        r#"pub mod declaration {
    bmux_plugin_schema_macros::schema! { source: "bpdl/git-sourced.bpdl", manifest: "plugin.toml" }
}

use bmux_plugin_sdk::author::{self, Call, Declarations, Plugin};

#[derive(Default)]
pub struct Pinger;

async fn ping(_cx: &Call, _args: declaration::commands::PingArgs) -> author::Result<()> {
    println!("pong");
    Ok(())
}

impl Plugin for Pinger {
    fn declare(d: &mut Declarations<Self>) {
        d.command(declaration::commands::PING, ping);
    }
}

bmux_plugin_sdk::plugin!(Pinger);
"#,
    )
    .expect("write consumer source");
    // Pin registry crates to the versions the workspace already resolved.
    std::fs::copy(lockfile, consumer.join("Cargo.lock")).expect("seed consumer lockfile");
}

#[test]
fn a_plugin_pinned_to_git_bmux_packages_itself() {
    let workspace = workspace_root();
    let target_dir = support::bmux_binary()
        .parent()
        .and_then(Path::parent)
        .expect("cargo target directory")
        .join("plugin-packaging/git-source-target");
    let scratch = tempfile::Builder::new()
        .prefix("bmux-git-source-")
        .tempdir_in(
            target_dir
                .parent()
                .expect("packaging dir")
                .parent()
                .expect("target dir"),
        )
        .expect("scratch dir");
    // A stable location (with deterministic commits): the git source Cargo resolves
    // is identical whenever the working tree is, so its build is reused. One run at
    // a time owns it.
    let packaging = target_dir.parent().expect("packaging dir");
    std::fs::create_dir_all(packaging).expect("create packaging dir");
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(packaging.join("git-source.lock"))
        .expect("open git-source lock");
    lock.lock().expect("lock git-source snapshot");
    let repo = packaging.join("git-source-repo/bmux");
    let consumer = scratch.path().join("consumer");
    let receipt = scratch.path().join("receipt");
    snapshot_repository(&workspace, &repo);
    write_consumer(&consumer, &repo, &workspace.join("Cargo.lock"));

    let package = Command::new("python3")
        .current_dir(&workspace)
        .arg("-B")
        .arg("scripts/package-plugin.py")
        .arg("--implementation-manifest")
        .arg(consumer.join("Cargo.toml"))
        .args([
            "--component",
            "backend",
            "--build",
            "--artifact-id",
            "git-source-test",
        ])
        .arg("--cargo-target-dir")
        .arg(&target_dir)
        .arg("--output")
        .arg(&receipt)
        // The consumer resolves its own graph; never inherit this workspace's target.
        .env_remove("CARGO_TARGET_DIR")
        .output()
        .expect("run plugin packager");
    support::assert_success(&package, "package git-sourced plugin");
    let library = String::from_utf8_lossy(&package.stdout)
        .lines()
        .last()
        .map(PathBuf::from)
        .expect("packager prints the library");
    assert!(library.is_file(), "missing {}", library.display());

    let receipt: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(receipt.join("packaging.json")).expect("read receipt"),
    )
    .expect("parse receipt");
    let sdk = receipt["sdk_source"].as_str().expect("sdk source");
    assert!(
        sdk.starts_with(&format!("git+file://{}", repo.display()))
            && sdk.contains("branch=master#"),
        "SDK must come from the consumer's git dependency: {sdk}"
    );

    let declaration = Command::new("python3")
        .current_dir(&workspace)
        .arg("-B")
        .arg("scripts/package-plugin.py")
        .arg("--declaration-of")
        .arg(&library)
        .output()
        .expect("inspect artifact");
    support::assert_success(&declaration, "read embedded declaration");
    let manifest = String::from_utf8_lossy(&declaration.stdout);
    assert!(
        manifest.contains("id = \"example.git-sourced\""),
        "{manifest}"
    );
    assert!(manifest.contains("name = \"ping\""), "{manifest}");
}
