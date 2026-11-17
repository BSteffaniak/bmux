//! Repository-wide BPDL contract checks.
//!
//! Every contract schema in the repository (`**/bpdl/*.bpdl`) must keep its
//! pinned canonical v2 interface identity. Identities are wire evidence that
//! other builds and other languages negotiate against, so a compiler change
//! must never perturb them silently.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

use bmux_plugin_schema::ast::Schema;
use bmux_plugin_schema::codegen_rust::{ImportInfo, ImportMap};

const IDENTITIES: &str = "tests/fixtures/contract-identities.txt";

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("plugin-schema should live under packages/plugin-schema")
        .to_path_buf()
}

/// Contract schemas live in a crate's `bpdl/` directory.
fn contract_schema_paths() -> Vec<PathBuf> {
    let root = repo_root();
    let mut paths = Vec::new();
    for group in ["packages", "plugins"] {
        let Ok(crates) = fs::read_dir(root.join(group)) else {
            continue;
        };
        for entry in crates.filter_map(Result::ok) {
            let Ok(files) = fs::read_dir(entry.path().join("bpdl")) else {
                continue;
            };
            for file in files.filter_map(Result::ok) {
                let path = file.path();
                if path.extension().and_then(|ext| ext.to_str()) == Some("bpdl") {
                    paths.push(path);
                }
            }
        }
    }
    paths.sort();
    paths
}

struct RepoSchemas {
    /// Relative path -> source text.
    sources: BTreeMap<String, String>,
    /// Plugin id -> isolated schema, used to resolve imports.
    by_plugin: BTreeMap<String, Schema>,
}

fn repo_schemas() -> RepoSchemas {
    let root = repo_root();
    let mut sources = BTreeMap::new();
    let mut by_plugin = BTreeMap::new();
    for path in contract_schema_paths() {
        let source = fs::read_to_string(&path).expect("schema should be readable");
        let relative = path
            .strip_prefix(&root)
            .expect("schema under repo root")
            .to_string_lossy()
            .replace('\\', "/");
        let schema = bmux_plugin_schema::compile(&source)
            .unwrap_or_else(|error| panic!("{relative}: {error}"));
        assert!(
            by_plugin
                .insert(schema.plugin.plugin_id.clone(), schema)
                .is_none(),
            "{relative}: duplicate contract schema plugin id"
        );
        sources.insert(relative, source);
    }
    RepoSchemas { sources, by_plugin }
}

fn resolved(repo: &RepoSchemas, relative: &str) -> (Schema, ImportMap) {
    let source = &repo.sources[relative];
    let isolated = bmux_plugin_schema::compile(source).expect("isolated compile");
    let mut schemas = BTreeMap::new();
    let mut imports = ImportMap::new();
    for import in &isolated.imports {
        let imported = repo
            .by_plugin
            .get(&import.plugin_id)
            .unwrap_or_else(|| panic!("{relative}: import {} not in repo", import.plugin_id))
            .clone();
        schemas.insert(import.alias.clone(), imported.clone());
        imports.insert(
            import.alias.clone(),
            ImportInfo {
                crate_path: String::new(),
                schema: imported,
            },
        );
    }
    let schema = bmux_plugin_schema::compile_with_imports(source, &schemas)
        .unwrap_or_else(|error| panic!("{relative}: {error}"));
    (schema, imports)
}

fn hex(bytes: &[u8]) -> String {
    if bytes.is_empty() {
        return "unavailable".to_string();
    }
    bytes.iter().fold(String::new(), |mut out, byte| {
        let _ = write!(out, "{byte:02x}");
        out
    })
}

fn current_identities(repo: &RepoSchemas) -> String {
    let mut out = String::new();
    for relative in repo.sources.keys() {
        let (schema, imports) = resolved(repo, relative);
        for interface in &schema.interfaces {
            let id = interface.interface_version.map_or_else(
                || interface.name.clone(),
                |version| format!("{}/v{version}", interface.name),
            );
            let digest =
                bmux_plugin_schema::canonical::interface_contract(&schema, interface, &imports);
            let _ = writeln!(out, "{relative} {id} {}", hex(&digest));
        }
    }
    out
}

#[test]
fn every_repo_contract_keeps_its_pinned_identity() {
    let repo = repo_schemas();
    let current = current_identities(&repo);
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join(IDENTITIES);
    if std::env::var_os("BMUX_BPDL_BLESS_NEW_CONTRACTS").is_some() {
        // Blessing may only add lines: a changed or removed identity is a
        // wire break and must be a deliberate new interface version instead.
        let pinned = fs::read_to_string(&fixture).unwrap_or_default();
        for line in pinned.lines() {
            assert!(
                current.lines().any(|current| current == line),
                "refusing to bless a changed or removed contract identity: {line}"
            );
        }
        fs::create_dir_all(fixture.parent().expect("fixture dir")).expect("create fixture dir");
        fs::write(&fixture, &current).expect("write identities");
        return;
    }
    let pinned = fs::read_to_string(&fixture).expect("pinned identities fixture");
    let mut problems = Vec::new();
    for line in pinned.lines() {
        if !current.lines().any(|current| current == line) {
            problems.push(format!("changed or removed: {line}"));
        }
    }
    for line in current.lines() {
        if !pinned.lines().any(|pinned| pinned == line) {
            problems.push(format!(
                "unpinned (run with BMUX_BPDL_BLESS_NEW_CONTRACTS=1 if new): {line}"
            ));
        }
    }
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}

/// Packages ordered so every import precedes its importer.
fn dependency_order(repo: &RepoSchemas) -> Vec<String> {
    fn visit(id: &str, repo: &RepoSchemas, done: &mut Vec<String>, active: &mut Vec<String>) {
        if done.iter().any(|seen| seen == id) {
            return;
        }
        assert!(
            !active.iter().any(|seen| seen == id),
            "import cycle through {id}"
        );
        active.push(id.to_string());
        for import in &repo.by_plugin[id].imports {
            visit(&import.plugin_id, repo, done, active);
        }
        active.pop();
        done.push(id.to_string());
    }
    let mut done = Vec::new();
    for id in repo.by_plugin.keys() {
        visit(id, repo, &mut done, &mut Vec::new());
    }
    done
}

#[test]
fn every_repo_contract_lowers_to_valid_wit() {
    let repo = repo_schemas();
    let mut set = bmux_plugin_schema::ir::PackageSet::new();
    for schema in repo.by_plugin.values() {
        set.insert(bmux_plugin_schema::ir::lower(schema));
    }
    let mut resolve = wit_parser::Resolve::new();
    for id in dependency_order(&repo) {
        let package = set.get(&id).expect("lowered package");
        let source = bmux_plugin_schema::wit::emit(&set, package)
            .unwrap_or_else(|error| panic!("{id}: {error}"));
        resolve
            .push_source(&format!("{id}.wit"), &source)
            .unwrap_or_else(|error| panic!("{id} lowered to invalid WIT: {error:?}\n{source}"));
    }
}
