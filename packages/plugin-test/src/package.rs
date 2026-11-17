//! Package a plugin crate as a real dynamic library with the shared packager.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex, OnceLock};

use bmux_plugin::DefinitionArtifact;
use bmux_plugin_sdk::PluginError;

/// Libraries already packaged by this process, by crate manifest.
static PACKAGED: Mutex<BTreeMap<String, PathBuf>> = Mutex::new(BTreeMap::new());

/// Libraries already loaded by this process, by crate manifest. A library
/// image is installed into one host runtime once and never unloaded, so
/// every `TestHost` of the process shares the artifact (each still creates
/// its own instances).
static LOADED: Mutex<BTreeMap<String, Arc<DefinitionArtifact>>> = Mutex::new(BTreeMap::new());

/// The process's host runtime for dynamic plugins: their plug-rt table is
/// bound to it for the life of the process, like a server's runtime, so it
/// outlives any one test's runtime.
fn host_runtime() -> bmux_plugin_sdk::Result<&'static tokio::runtime::Runtime> {
    static RUNTIME: OnceLock<std::io::Result<tokio::runtime::Runtime>> = OnceLock::new();
    RUNTIME
        .get_or_init(|| {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .thread_name("bmux-test-host")
                .enable_all()
                .build()
        })
        .as_ref()
        .map_err(|error| failure(format!("test host runtime: {error}")))
}

/// The loaded dylib artifact of the crate at `manifest`, packaged and loaded
/// once per process.
pub fn dylib_artifact(manifest: &str) -> bmux_plugin_sdk::Result<Arc<DefinitionArtifact>> {
    let mut loaded = LOADED
        .lock()
        .map_err(|_| failure("dylib artifact cache poisoned"))?;
    if let Some(artifact) = loaded.get(manifest) {
        return Ok(Arc::clone(artifact));
    }
    let library = backend_dylib(manifest)?;
    let _host = host_runtime()?.enter();
    // SAFETY: the library was just built from the crate under test by the
    // shared packager; it is trusted test code.
    let artifact = Arc::new(unsafe { DefinitionArtifact::load(&library) }?);
    loaded.insert(manifest.to_owned(), Arc::clone(&artifact));
    drop(loaded);
    Ok(artifact)
}

fn failure(details: impl Into<String>) -> PluginError {
    PluginError::ServiceProtocol {
        details: details.into(),
    }
}

/// The packager script of the bmux checkout this harness was built from.
fn packager() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/package-plugin.py")
}

/// `<target>` and whether the running test binary is a release build, from
/// the binary's own path (`<target>/<profile>/deps/<test>`).
fn target_of_current_test() -> bmux_plugin_sdk::Result<(PathBuf, bool)> {
    let exe = std::env::current_exe().map_err(|error| failure(error.to_string()))?;
    let profile = exe
        .parent()
        .and_then(Path::parent)
        .ok_or_else(|| failure("test binary is not under <target>/<profile>/deps"))?;
    let target = profile
        .parent()
        .ok_or_else(|| failure("test binary has no target directory"))?;
    let release = profile.file_name().is_some_and(|name| name == "release");
    Ok((target.to_path_buf(), release))
}

/// Package the crate at `manifest` as a backend dylib (its own `plugin!`
/// entry, `dynamic-backend`), once per process. The packager builds it with
/// `cargo rustc --crate-type cdylib` in `<target>/plugin-dylib`, so the
/// workspace's own builds are untouched.
fn backend_dylib(manifest: &str) -> bmux_plugin_sdk::Result<PathBuf> {
    let mut packaged = PACKAGED
        .lock()
        .map_err(|_| failure("dylib packaging cache poisoned"))?;
    if let Some(library) = packaged.get(manifest) {
        return Ok(library.clone());
    }
    let (target, release) = target_of_current_test()?;
    let crate_dir = Path::new(manifest)
        .parent()
        .ok_or_else(|| failure(format!("{manifest} has no crate directory")))?;
    let label = crate_dir.file_name().map_or_else(
        || "plugin".into(),
        |name| name.to_string_lossy().into_owned(),
    );
    let mut command = Command::new("python3");
    command
        .arg("-B")
        .arg(packager())
        .arg("--implementation-manifest")
        .arg(manifest)
        .args(["--component", "backend", "--build", "--artifact-id"])
        .arg(format!("test-host-{label}"))
        .arg("--cargo-target-dir")
        .arg(target.join("plugin-dylib"));
    if release {
        command.arg("--release");
    }
    let output = command
        .output()
        .map_err(|error| failure(format!("run the plugin packager (python3 3.11+): {error}")))?;
    if !output.status.success() {
        return Err(failure(format!(
            "packaging {manifest} failed:\n{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    // The packager prints the library Cargo reported; never guess a name.
    let library = String::from_utf8_lossy(&output.stdout)
        .lines()
        .last()
        .map(PathBuf::from)
        .filter(|library| library.is_file())
        .ok_or_else(|| failure(format!("the packager reported no library for {manifest}")))?;
    packaged.insert(manifest.to_owned(), library.clone());
    drop(packaged);
    Ok(library)
}
