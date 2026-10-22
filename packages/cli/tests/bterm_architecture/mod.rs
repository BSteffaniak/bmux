//! Dependency-graph guards for optional native presentation.
//!
//! Read all production dependency declarations (including target-specific,
//! optional, renamed, and build dependencies). This intentionally checks more
//! than the current machine's feature resolution: headless builds must remain
//! GUI-free on every supported target. No cargo subprocess, network, or build
//! is needed to inspect the graph.
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

type Graph = BTreeMap<String, BTreeSet<String>>;

fn manifest(path: &Path) -> toml::Value {
    let source = std::fs::read_to_string(path)
        .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
    toml::from_str(&source).unwrap_or_else(|error| panic!("parse {}: {error}", path.display()))
}

fn collect_dependencies(
    value: &toml::Value,
    workspace: &toml::value::Table,
    dependencies: &mut BTreeSet<String>,
) {
    for section in ["dependencies", "build-dependencies"] {
        if let Some(table) = value.get(section).and_then(toml::Value::as_table) {
            for (alias, specification) in table {
                let specification = if specification
                    .get("workspace")
                    .and_then(toml::Value::as_bool)
                    == Some(true)
                {
                    workspace
                        .get(alias)
                        .unwrap_or_else(|| panic!("unresolved workspace dependency {alias}"))
                } else {
                    specification
                };
                let package = specification
                    .get("package")
                    .and_then(toml::Value::as_str)
                    .unwrap_or(alias);
                dependencies.insert(package.to_owned());
            }
        }
    }
    if let Some(targets) = value.get("target").and_then(toml::Value::as_table) {
        for target in targets.values() {
            collect_dependencies(target, workspace, dependencies);
        }
    }
}

fn workspace_graph(root: &Path) -> Graph {
    let root_manifest = manifest(&root.join("Cargo.toml"));
    let workspace = root_manifest["workspace"]["dependencies"]
        .as_table()
        .expect("workspace dependencies");
    let members = root_manifest["workspace"]["members"]
        .as_array()
        .expect("explicit workspace members");
    let mut graph = Graph::new();
    for member in members {
        let member = member.as_str().expect("workspace member path");
        assert!(
            !member.contains('*'),
            "expand wildcard member {member} in graph guard"
        );
        let package = manifest(&root.join(member).join("Cargo.toml"));
        let name = package["package"]["name"].as_str().expect("package name");
        let mut dependencies = BTreeSet::new();
        collect_dependencies(&package, workspace, &mut dependencies);
        assert!(graph.insert(name.to_owned(), dependencies).is_none());
    }
    graph
}

fn dependency_path(
    graph: &Graph,
    start: &str,
    denied: impl Fn(&str) -> bool,
) -> Option<Vec<String>> {
    let mut pending = vec![vec![start.to_owned()]];
    let mut visited = BTreeSet::new();
    while let Some(path) = pending.pop() {
        let package = path.last().expect("nonempty dependency path");
        if !visited.insert(package.clone()) {
            continue;
        }
        if path.len() > 1 && denied(package) {
            return Some(path);
        }
        if let Some(dependencies) = graph.get(package) {
            for dependency in dependencies {
                let mut next = path.clone();
                next.push(dependency.clone());
                pending.push(next);
            }
        }
    }
    None
}

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("workspace root")
}

fn assert_boundary(graph: &Graph, start: &str, denied: impl Fn(&str) -> bool) {
    assert!(
        graph.contains_key(start),
        "missing required package {start}"
    );
    assert!(
        dependency_path(graph, start, denied).is_none(),
        "{start} crosses its production dependency boundary"
    );
}

fn native_backend(package: &str) -> bool {
    matches!(
        package,
        "bterm_native_platform"
            | "bterm_native_renderer"
            | "bterm"
            | "winit"
            | "wgpu"
            | "softbuffer"
            | "accesskit_winit"
    )
}

#[test]
fn bterm_packages_use_product_namespace() {
    let graph = workspace_graph(&root());
    for package in ["bterm", "bterm_native_platform", "bterm_native_renderer"] {
        assert!(graph.contains_key(package), "missing package {package}");
    }
    for package in ["bmux_bterm", "bmux_native_platform", "bmux_native_renderer"] {
        assert!(
            !graph.contains_key(package),
            "obsolete Bterm package name {package}"
        );
    }
}

#[test]
fn bterm_headless_and_terminal_dependency_closures_remain_gui_free() {
    let graph = workspace_graph(&root());
    for package in [
        "bmux_cli",
        "bmux_server",
        "bmux_client",
        "bmux_ipc",
        "bmux_frontend_runtime",
        "bmux_attach_pipeline",
        "bmux_tui_runtime",
    ] {
        if let Some(path) = dependency_path(&graph, package, native_backend) {
            panic!(
                "GUI dependency escaped native quarantine: {}",
                path.join(" -> ")
            );
        }
        assert!(graph.contains_key(package), "missing package {package}");
    }
}

#[test]
fn bterm_native_mechanisms_do_not_own_plugin_or_browser_runtime() {
    let graph = workspace_graph(&root());
    for package in [
        "bmux_frontend_runtime",
        "bterm_native_platform",
        "bterm_native_renderer",
    ] {
        let denied = |dependency: &str| {
            (dependency.ends_with("_plugin") && dependency != "bmux_plugin")
                || matches!(
                    dependency,
                    "bmux_web_host" | "bmux_web_renderer" | "bmux_cli"
                )
        };
        if let Some(path) = dependency_path(&graph, package, denied) {
            panic!(
                "native mechanism acquired product/transport owner: {}",
                path.join(" -> ")
            );
        }
        assert_boundary(&graph, package, denied);
    }
}

#[test]
fn graph_guard_resolves_aliases_and_target_optional_build_dependencies() {
    let workspace: toml::Value = toml::from_str(
        r#"[dependencies]
window_alias = { package = "winit", version = "0.30" }
"#,
    )
    .unwrap();
    let source: toml::Value = toml::from_str(
        r#"[dependencies]
window_alias = { workspace = true, optional = true }
[target.'cfg(windows)'.build-dependencies]
gpu_alias = { package = "wgpu", version = "1" }
[dev-dependencies]
not_production = "1"
"#,
    )
    .unwrap();
    let mut dependencies = BTreeSet::new();
    collect_dependencies(
        &source,
        workspace["dependencies"].as_table().unwrap(),
        &mut dependencies,
    );
    assert_eq!(
        dependencies,
        BTreeSet::from(["wgpu".to_owned(), "winit".to_owned()])
    );
    let graph = BTreeMap::from([
        (
            "headless".to_owned(),
            BTreeSet::from(["neutral".to_owned()]),
        ),
        ("neutral".to_owned(), dependencies),
    ]);
    let path = dependency_path(&graph, "headless", native_backend).unwrap();
    assert_eq!(path.first().unwrap(), "headless");
    assert_eq!(path.len(), 3);
    assert!(native_backend(path.last().unwrap()));
}
