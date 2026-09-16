//! Private lifecycle for inherited-config development sandboxes.
use super::{
    RunSandboxOptions, SandboxPaths, exit_code_to_u8, read_manifest, resolve_bmux_binary,
    resolve_sandbox_target, sandbox_socket_alive,
};
use anyhow::{Context, Result, bail};
use bmux_config::{BmuxConfig, ConfigPaths};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const DESCRIPTOR: &str = "development.json";

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Development {
    version: u32,
    binary: PathBuf,
    binary_digest: String,
    cwd: PathBuf,
}

fn digest(path: &Path) -> Result<String> {
    let mut file = std::fs::File::open(path)?;
    let mut hash = Sha256::new();
    let mut buffer = [0; 8192];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hash.update(&buffer[..count]);
    }
    Ok(super::hex_encode_digest(hash.finalize()))
}

fn private_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

fn snapshot() -> Result<toml::Value> {
    let value = BmuxConfig::snapshot_raw_configuration()?;
    // Reading plugin artifacts is distinct from sharing their writable state.
    let config = BmuxConfig::load()?;
    let roots = super::super::plugin_runtime::resolve_plugin_search_paths(
        &config,
        &ConfigPaths::default(),
    )?;
    snapshot_value(value, roots)
}

fn snapshot_value(mut value: toml::Value, roots: Vec<PathBuf>) -> Result<toml::Value> {
    let plugins = value
        .as_table_mut()
        .context("configuration must be a table")?
        .entry("plugins")
        .or_insert_with(|| toml::Value::Table(toml::Table::new()));
    plugins
        .as_table_mut()
        .context("plugins must be a table")?
        .insert(
            "search_paths".into(),
            toml::Value::Array(
                roots
                    .into_iter()
                    .map(|path| toml::Value::String(path.to_string_lossy().into_owned()))
                    .collect(),
            ),
        );
    let server = value
        .as_table_mut()
        .context("configuration must be a table")?
        .entry("server")
        .or_insert_with(|| toml::Value::Table(toml::Table::new()));
    let gateway = server
        .as_table_mut()
        .context("server must be a table")?
        .entry("gateway")
        .or_insert_with(|| toml::Value::Table(toml::Table::new()));
    gateway
        .as_table_mut()
        .context("gateway must be a table")?
        .insert("enabled".into(), toml::Value::Boolean(false));
    Ok(value)
}

pub async fn run_inherited_sandbox(
    mut options: RunSandboxOptions<'_>,
    args: &[String],
) -> Result<u8> {
    if args.iter().any(|arg| {
        ["--runtime", "--target", "--config"]
            .iter()
            .any(|flag| arg == flag || arg.starts_with(&format!("{flag}=")))
    }) {
        bail!(
            "inherited-config sandboxes own --runtime, --target, and --config; omit these child overrides"
        );
    }
    if options.print_env {
        bail!(
            "--print-env is unavailable with --inherit-config: inherited environment may contain secrets"
        );
    }
    let config = snapshot()?;
    if options.json && args.is_empty() {
        bail!("--json requires an explicit non-interactive child command");
    }
    let binary = resolve_bmux_binary(options.bmux_bin)?.canonicalize()?;
    let descriptor = Development {
        version: 1,
        binary_digest: digest(&binary)?,
        binary,
        cwd: std::env::current_dir()?,
    };
    let sandbox = development_paths(options.name);
    let mut directory = std::fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        directory.mode(0o700);
    }
    directory.create(&sandbox.root_dir)?;
    sandbox.ensure_dirs()?;
    private_write(
        &sandbox.config_paths.config_file(),
        toml::to_string(&config)?.as_bytes(),
    )?;
    private_write(
        &sandbox.root_dir.join(DESCRIPTOR),
        &serde_json::to_vec(&descriptor)?,
    )?;
    eprintln!(
        "Sandbox: {}\nConfig: inherited snapshot; gateway disabled\nBinary: {}\nRuntime, data, state, logs: isolated\nExternal paths and services in configuration remain shared.\nReconnect: bmux sandbox attach {}\nStop: bmux sandbox stop {}",
        sandbox.root_dir.display(),
        descriptor.binary.display(),
        sandbox.root_dir.display(),
        sandbox.root_dir.display()
    );
    options.keep = true;
    super::run_sandbox_at(options, args, sandbox).await
}

fn development_paths(name: Option<&str>) -> SandboxPaths {
    let paths = SandboxPaths::new(name);
    #[cfg(unix)]
    if paths.config_paths.server_socket().as_os_str().len() >= 100 {
        // macOS's default TMPDIR alone can consume most of sockaddr_un.
        // Keep all owned paths together under a private short temporary root.
        return SandboxPaths::at_root(
            PathBuf::from("/tmp").join(format!("bmux-sbx-{}", uuid::Uuid::new_v4().simple())),
        );
    }
    paths
}

fn load(sandbox: &SandboxPaths) -> Result<Development> {
    let value: Development =
        serde_json::from_slice(&std::fs::read(sandbox.root_dir.join(DESCRIPTOR))?)?;
    if value.version != 1 || !value.binary.is_absolute() || !value.cwd.is_absolute() {
        bail!("unsupported or corrupt development sandbox descriptor");
    }
    Ok(value)
}

pub(super) fn apply_environment(command: &mut Command, sandbox: &SandboxPaths) -> bool {
    if !sandbox.root_dir.join(DESCRIPTOR).exists() {
        return false;
    }
    // Never inherit attachment identities, endpoints, config overrides, or slots.
    // Preserve the ordinary shell environment, including HOME and TERM.
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("BMUX_") {
            command.env_remove(key);
        }
    }
    command
        .env("BMUX_CONFIG_DIR", &sandbox.config_paths.config_dir)
        .env("BMUX_RUNTIME_DIR", &sandbox.config_paths.runtime_dir)
        .env("BMUX_DATA_DIR", &sandbox.config_paths.data_dir)
        .env("BMUX_STATE_DIR", &sandbox.config_paths.state_dir)
        .env("BMUX_LOG_DIR", &sandbox.log_dir)
        .env("BMUX_NO_BASE_CONFIG", "1")
        .env("BMUX_RUNTIME_NAME", "default")
        .env("BMUX_TARGET", "local");
    true
}

pub(super) fn configure_child(command: &mut Command, sandbox: &SandboxPaths) -> Result<()> {
    let descriptor = load(sandbox)?;
    command.current_dir(descriptor.cwd);
    command.args(["--runtime", "default", "--target", "local"]);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.arg0("bmux");
    }
    Ok(())
}

pub async fn run_inherited_sandbox_control(target: &str, stop: bool) -> Result<u8> {
    let root = resolve_sandbox_target(target)?.canonicalize()?;
    let manifest = read_manifest(&root)?;
    if Path::new(&manifest.paths.root).canonicalize()? != root {
        bail!("sandbox manifest root mismatch");
    }
    let mut sandbox = SandboxPaths::new(None);
    sandbox.root_dir = root.clone();
    sandbox.config_paths = ConfigPaths::new(
        root.join("config/bmux"),
        root.join("runtime"),
        root.join("data/bmux"),
        root.join("state"),
    );
    sandbox.log_dir = root.join("logs");
    let descriptor = load(&sandbox)?;
    if !sandbox_socket_alive(&root) {
        bail!(
            "sandbox server is not running; start a new sandbox with `sandbox dev --inherit-config`"
        );
    }
    if !stop && digest(&descriptor.binary)? != descriptor.binary_digest {
        bail!(
            "sandbox binary has changed since launch; stop this sandbox before testing the rebuilt binary"
        );
    }
    let mut command = Command::new(&descriptor.binary);
    if stop {
        command.args(["server", "stop"]);
    } else {
        command.arg("attach");
    }
    apply_environment(&mut command, &sandbox);
    configure_child(&mut command, &sandbox)?;
    command
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    let mut child = command
        .spawn()
        .context("failed starting sandbox control command")?;
    loop {
        if let Some(status) = child.try_wait()? {
            if stop && status.success() {
                let mut manifest = manifest.clone();
                manifest.status = "succeeded".into();
                manifest.updated_at_unix_ms = super::unix_millis_now_meta();
                super::write_manifest(&root, &manifest)?;
                super::upsert_sandbox_index_entry(&manifest)?;
            }
            return Ok(exit_code_to_u8(status.code().unwrap_or(1)));
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_preserves_unknown_settings_and_disables_gateway() {
        let original: toml::Value = toml::from_str(
            "[server.gateway]\nenabled = true\nlisten = '0.0.0.0:7443'\n[plugins.custom]\nopaque = ['one', 'two']\n",
        ).unwrap();
        let value = snapshot_value(original.clone(), vec![PathBuf::from("/plugins")]).unwrap();
        assert_eq!(value["plugins"]["custom"], original["plugins"]["custom"]);
        assert_eq!(value["server"]["gateway"]["enabled"].as_bool(), Some(false));
        assert_eq!(
            value["server"]["gateway"]["listen"],
            original["server"]["gateway"]["listen"]
        );
    }

    #[test]
    fn private_snapshot_is_create_only_and_rejects_unknown_versions() {
        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("snapshot");
        private_write(&file, b"secret").unwrap();
        assert!(private_write(&file, b"overwrite").is_err());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&file).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        let mut sandbox = SandboxPaths::new(None);
        sandbox.root_dir = temp.path().to_path_buf();
        private_write(
            &sandbox.root_dir.join(DESCRIPTOR),
            br#"{"version":99,"binary":"/bmux","binary_digest":"x","cwd":"/"}"#,
        )
        .unwrap();
        assert!(load(&sandbox).is_err());
    }

    #[test]
    fn environment_routes_owned_directories_into_sandbox() {
        let temp = tempfile::tempdir().unwrap();
        let mut sandbox = SandboxPaths::new(None);
        sandbox.root_dir = temp.path().to_path_buf();
        private_write(&sandbox.root_dir.join(DESCRIPTOR), b"{}").unwrap();
        let mut command = Command::new("bmux");
        assert!(apply_environment(&mut command, &sandbox));
        let environment = command
            .get_envs()
            .collect::<std::collections::BTreeMap<_, _>>();
        assert_eq!(
            environment[std::ffi::OsStr::new("BMUX_STATE_DIR")],
            Some(sandbox.config_paths.state_dir.as_os_str())
        );
        assert_eq!(
            environment[std::ffi::OsStr::new("BMUX_RUNTIME_DIR")],
            Some(sandbox.config_paths.runtime_dir.as_os_str())
        );
        assert_eq!(
            environment[std::ffi::OsStr::new("BMUX_DATA_DIR")],
            Some(sandbox.config_paths.data_dir.as_os_str())
        );
        assert_eq!(
            environment[std::ffi::OsStr::new("BMUX_NO_BASE_CONFIG")],
            Some(std::ffi::OsStr::new("1"))
        );
        assert!(!environment.contains_key(std::ffi::OsStr::new("HOME")));
    }
}
