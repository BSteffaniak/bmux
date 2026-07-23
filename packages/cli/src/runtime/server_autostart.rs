use anyhow::{Context, Result};
use serde::Serialize;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::Command;

use super::{ConnectionContext, active_runtime_name, server_is_running};

const OWNERSHIP_MARKER: &str = "Managed by bmux server autostart";
const SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
enum Ownership {
    NotInstalled,
    BmuxManaged,
    ExternallyManaged,
    Conflict,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[allow(dead_code)] // Unsupported is constructed only on targets without a native backend.
#[serde(rename_all = "kebab-case")]
enum ManagerState {
    Unsupported,
    NotRegistered,
    Registered,
    Running,
    Unknown,
}

#[derive(Debug, Serialize)]
struct AutostartStatus {
    platform: &'static str,
    supported: bool,
    runtime: String,
    service_id: String,
    declaration_path: Option<String>,
    ownership: Ownership,
    schema_version: Option<u32>,
    manager_state: ManagerState,
    server_running: bool,
    executable: Option<String>,
    executable_exists: bool,
    detail: Option<String>,
}

#[derive(Debug, Clone)]
struct ServiceSpec {
    platform: Platform,
    runtime: String,
    service_id: String,
    declaration_path: Option<PathBuf>,
    executable: PathBuf,
    arguments: Vec<String>,
    user_home: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)] // All variants are rendered in pure cross-platform tests; only one is native per build.
enum Platform {
    MacOs,
    Linux,
    Windows,
    Unsupported,
}

impl Platform {
    const fn current() -> Self {
        #[cfg(target_os = "macos")]
        {
            return Self::MacOs;
        }
        #[cfg(target_os = "linux")]
        {
            return Self::Linux;
        }
        #[cfg(target_os = "windows")]
        {
            return Self::Windows;
        }
        #[allow(unreachable_code)]
        Self::Unsupported
    }

    const fn name(self) -> &'static str {
        match self {
            Self::MacOs => "macos",
            Self::Linux => "linux",
            Self::Windows => "windows",
            Self::Unsupported => std::env::consts::OS,
        }
    }

    const fn supported(self) -> bool {
        !matches!(self, Self::Unsupported)
    }
}

#[derive(Debug)]
struct CommandResult {
    success: bool,
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

trait CommandRunner {
    fn run(&self, program: &OsStr, arguments: &[OsString]) -> Result<CommandResult>;
}

struct NativeCommandRunner;

impl CommandRunner for NativeCommandRunner {
    fn run(&self, program: &OsStr, arguments: &[OsString]) -> Result<CommandResult> {
        let output = Command::new(program)
            .args(arguments)
            .output()
            .with_context(|| format!("failed running {}", Path::new(program).display()))?;
        Ok(CommandResult {
            success: output.status.success(),
            code: output.status.code(),
            stdout: decode_command_output(&output.stdout),
            stderr: decode_command_output(&output.stderr),
        })
    }
}

fn decode_command_output(bytes: &[u8]) -> String {
    if bytes.starts_with(&[0xff, 0xfe]) {
        let units = bytes[2..]
            .chunks_exact(2)
            .map(|chunk| u16::from_le_bytes([chunk[0], chunk[1]]))
            .collect::<Vec<_>>();
        return String::from_utf16_lossy(&units).trim().to_string();
    }
    if bytes.starts_with(&[0xfe, 0xff]) {
        let units = bytes[2..]
            .chunks_exact(2)
            .map(|chunk| u16::from_be_bytes([chunk[0], chunk[1]]))
            .collect::<Vec<_>>();
        return String::from_utf16_lossy(&units).trim().to_string();
    }
    String::from_utf8_lossy(bytes).trim().to_string()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InstallPlan {
    Create,
    Update,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UninstallPlan {
    Noop,
    Remove,
}

fn plan_install(ownership: Ownership, manager_state: ManagerState) -> Result<InstallPlan> {
    if manager_state == ManagerState::Unknown {
        anyhow::bail!("native service manager state is unknown");
    }
    match ownership {
        Ownership::NotInstalled
            if matches!(
                manager_state,
                ManagerState::Registered | ManagerState::Running
            ) =>
        {
            anyhow::bail!("native service identity is externally registered")
        }
        Ownership::NotInstalled => Ok(InstallPlan::Create),
        Ownership::BmuxManaged => Ok(InstallPlan::Update),
        Ownership::ExternallyManaged => anyhow::bail!("declaration is externally managed"),
        Ownership::Conflict => anyhow::bail!("declaration ownership conflicts with bmux"),
    }
}

fn plan_uninstall(ownership: Ownership, manager_state: ManagerState) -> Result<UninstallPlan> {
    if manager_state == ManagerState::Unknown {
        anyhow::bail!("native service manager state is unknown");
    }
    match ownership {
        Ownership::NotInstalled
            if matches!(
                manager_state,
                ManagerState::Registered | ManagerState::Running
            ) =>
        {
            anyhow::bail!("native service identity is externally registered")
        }
        Ownership::NotInstalled => Ok(UninstallPlan::Noop),
        Ownership::BmuxManaged => Ok(UninstallPlan::Remove),
        Ownership::ExternallyManaged => anyhow::bail!("declaration is externally managed"),
        Ownership::Conflict => anyhow::bail!("declaration ownership conflicts with bmux"),
    }
}

#[derive(Debug)]
struct DeclarationInspection {
    ownership: Ownership,
    schema_version: Option<u32>,
    executable: Option<String>,
}

fn build_status(
    spec: ServiceSpec,
    inspection: &DeclarationInspection,
    manager_state: ManagerState,
    server_running: bool,
    detail: Option<String>,
) -> AutostartStatus {
    let ownership = if inspection.ownership == Ownership::NotInstalled
        && matches!(
            manager_state,
            ManagerState::Registered | ManagerState::Running
        ) {
        Ownership::ExternallyManaged
    } else {
        inspection.ownership
    };
    let executable = inspection
        .executable
        .clone()
        .or_else(|| Some(spec.executable.to_string_lossy().into_owned()));
    let executable_exists = executable
        .as_ref()
        .is_some_and(|path| Path::new(path).is_file());
    AutostartStatus {
        platform: spec.platform.name(),
        supported: spec.platform.supported(),
        runtime: spec.runtime,
        service_id: spec.service_id,
        declaration_path: spec
            .declaration_path
            .map(|path| path.to_string_lossy().into_owned()),
        ownership,
        schema_version: inspection.schema_version,
        manager_state,
        server_running,
        executable,
        executable_exists,
        detail,
    }
}

fn render_human_status(status: &AutostartStatus) -> Result<String> {
    use std::fmt::Write as _;
    let mut output = String::new();
    writeln!(output, "platform: {}", status.platform)?;
    writeln!(
        output,
        "supported: {}",
        if status.supported { "yes" } else { "no" }
    )?;
    writeln!(output, "runtime: {}", status.runtime)?;
    writeln!(output, "service id: {}", status.service_id)?;
    if let Some(path) = &status.declaration_path {
        writeln!(output, "declaration: {path}")?;
    }
    writeln!(
        output,
        "ownership: {}",
        serde_json::to_value(status.ownership)?
            .as_str()
            .unwrap_or("unknown")
    )?;
    writeln!(
        output,
        "manager: {}",
        serde_json::to_value(status.manager_state)?
            .as_str()
            .unwrap_or("unknown")
    )?;
    writeln!(
        output,
        "server running: {}",
        if status.server_running { "yes" } else { "no" }
    )?;
    if let Some(path) = &status.executable {
        writeln!(output, "executable: {path}")?;
        writeln!(
            output,
            "executable exists: {}",
            if status.executable_exists {
                "yes"
            } else {
                "no"
            }
        )?;
    }
    if let Some(detail) = &status.detail {
        writeln!(output, "detail: {detail}")?;
    }
    Ok(output)
}

pub(super) async fn run_server_autostart_install(
    no_start: bool,
    executable: Option<&str>,
    connection_context: ConnectionContext<'_>,
) -> Result<u8> {
    ensure_local_command(connection_context)?;
    let spec = service_spec(executable)?;
    ensure_supported(spec.platform)?;
    let inspection = inspect_declaration(&spec)?;
    let (manager_state, _) = manager_status(&spec, &NativeCommandRunner);
    if !no_start
        && server_is_running(ConnectionContext::new(Some("local"))).await?
        && !(inspection.ownership == Ownership::BmuxManaged
            && manager_state == ManagerState::Running)
    {
        anyhow::bail!(
            "a bmux server is already running outside this managed autostart service; stop it first or use --no-start"
        );
    }
    let declaration = render_declaration(&spec)?;
    install_with_runner(&spec, &declaration, no_start, &NativeCommandRunner)?;
    println!(
        "installed bmux server autostart for runtime '{}'{}",
        spec.runtime,
        if no_start {
            " (starts at next login)"
        } else {
            ""
        }
    );
    Ok(0)
}

pub(super) fn run_server_autostart_uninstall(
    connection_context: ConnectionContext<'_>,
) -> Result<u8> {
    ensure_local_command(connection_context)?;
    let spec = service_spec(None)?;
    ensure_supported(spec.platform)?;
    uninstall_with_runner(&spec, &NativeCommandRunner)?;
    println!(
        "removed bmux server autostart for runtime '{}'",
        spec.runtime
    );
    Ok(0)
}

pub(super) async fn run_server_autostart_status(
    json: bool,
    connection_context: ConnectionContext<'_>,
) -> Result<u8> {
    ensure_local_command(connection_context)?;
    let spec = service_spec_for_status()?;
    let inspection = inspect_declaration(&spec)?;
    let (manager_state, detail) = manager_status(&spec, &NativeCommandRunner);
    let server_running = server_is_running(ConnectionContext::new(Some("local")))
        .await
        .unwrap_or(false);
    let status = build_status(spec, &inspection, manager_state, server_running, detail);
    if json {
        println!("{}", serde_json::to_string_pretty(&status)?);
    } else {
        print!("{}", render_human_status(&status)?);
    }
    Ok(u8::from(
        !status.supported || status.ownership == Ownership::Conflict,
    ))
}

pub(super) fn run_server_autostart_print(
    executable: Option<&str>,
    connection_context: ConnectionContext<'_>,
) -> Result<u8> {
    ensure_local_command(connection_context)?;
    let spec = service_spec(executable)?;
    ensure_supported(spec.platform)?;
    print!("{}", render_declaration(&spec)?);
    Ok(0)
}

fn ensure_local_command(connection_context: ConnectionContext<'_>) -> Result<()> {
    if connection_context
        .target_override
        .is_some_and(|target| target != "local")
    {
        anyhow::bail!("server autostart is local-only; remove --target or use --target local");
    }
    Ok(())
}

fn ensure_supported(platform: Platform) -> Result<()> {
    if platform.supported() {
        Ok(())
    } else {
        anyhow::bail!(
            "native server autostart is not supported on {}; use 'bmux server autostart print' on a supported target",
            platform.name()
        )
    }
}

fn service_spec(executable: Option<&str>) -> Result<ServiceSpec> {
    service_spec_for(Platform::current(), &active_runtime_name(), executable)
}

fn service_spec_for_status() -> Result<ServiceSpec> {
    let executable = resolve_executable_for_status();
    service_spec_with_executable(Platform::current(), &active_runtime_name(), executable)
}

fn resolve_executable_for_status() -> PathBuf {
    resolve_executable(None).unwrap_or_else(|_| PathBuf::from("bmux"))
}

fn service_spec_for(
    platform: Platform,
    runtime: &str,
    executable: Option<&str>,
) -> Result<ServiceSpec> {
    let executable = resolve_executable(executable)?;
    service_spec_with_executable(platform, runtime, executable)
}

fn explicit_config_path() -> Option<PathBuf> {
    std::env::var_os("BMUX_CONFIG")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("BMUX_CONFIG_DIR")
                .map(PathBuf::from)
                .map(|directory| directory.join("bmux.toml"))
        })
}

fn service_spec_with_executable(
    platform: Platform,
    runtime: &str,
    executable: PathBuf,
) -> Result<ServiceSpec> {
    validate_runtime_component(runtime)?;
    let (service_id, declaration_path) = match platform {
        Platform::MacOs => {
            let id = if runtime == "default" {
                "dev.bmux.server".to_string()
            } else {
                format!("dev.bmux.server.{runtime}")
            };
            let home = dirs::home_dir().context("cannot resolve home directory")?;
            let path = home
                .join("Library/LaunchAgents")
                .join(format!("{id}.plist"));
            (id, Some(path))
        }
        Platform::Linux => {
            let id = if runtime == "default" {
                "bmux-server.service".to_string()
            } else {
                format!("bmux-server-{runtime}.service")
            };
            let config_home = std::env::var_os("XDG_CONFIG_HOME")
                .filter(|value| !value.is_empty())
                .map(PathBuf::from)
                .or_else(|| dirs::home_dir().map(|home| home.join(".config")))
                .context("cannot resolve XDG config directory")?;
            (id.clone(), Some(config_home.join("systemd/user").join(id)))
        }
        Platform::Windows => {
            let id = if runtime == "default" {
                "bmux-server".to_string()
            } else {
                format!("bmux-server-{runtime}")
            };
            (id, None)
        }
        Platform::Unsupported => (format!("bmux-server-{runtime}"), None),
    };
    let mut arguments = vec![
        "--runtime".to_string(),
        runtime.to_string(),
        "server".to_string(),
        "start".to_string(),
    ];
    if let Some(config_path) = explicit_config_path() {
        arguments.splice(
            0..0,
            [
                "--config".to_string(),
                config_path.to_string_lossy().into_owned(),
            ],
        );
    }
    Ok(ServiceSpec {
        platform,
        runtime: runtime.to_string(),
        service_id,
        declaration_path,
        executable,
        arguments,
        user_home: dirs::home_dir().unwrap_or_else(|| PathBuf::from("/")),
    })
}

fn validate_runtime_component(runtime: &str) -> Result<()> {
    if runtime.is_empty()
        || !runtime
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.'))
        || runtime.starts_with('.')
        || runtime.ends_with('.')
        || runtime.contains("..")
    {
        anyhow::bail!("runtime name is not safe for a native service identity: {runtime}");
    }
    Ok(())
}

fn resolve_executable(explicit: Option<&str>) -> Result<PathBuf> {
    if let Some(explicit) = explicit {
        return validate_executable(PathBuf::from(explicit));
    }
    if let Some(argv0) = std::env::args_os().next() {
        let candidate = PathBuf::from(&argv0);
        if candidate.components().count() > 1 && candidate.is_file() {
            return validate_executable(absolutize(candidate)?);
        }
        if candidate.components().count() == 1
            && let Some(found) = find_on_path(&candidate)
        {
            return validate_executable(found);
        }
    }
    validate_executable(
        std::env::current_exe().context("failed resolving current bmux executable")?,
    )
}

fn absolutize(path: PathBuf) -> Result<PathBuf> {
    if path.is_absolute() {
        Ok(path)
    } else {
        Ok(std::env::current_dir()
            .context("failed resolving current directory")?
            .join(path))
    }
}

fn find_on_path(name: &Path) -> Option<PathBuf> {
    std::env::var_os("PATH")
        .into_iter()
        .flat_map(|value| std::env::split_paths(&value).collect::<Vec<_>>())
        .map(|directory| directory.join(name))
        .find(|candidate| candidate.is_file())
}

fn validate_executable(path: PathBuf) -> Result<PathBuf> {
    let path = absolutize(path)?;
    if !path.is_file() {
        anyhow::bail!(
            "bmux executable does not exist or is not a file: {}",
            path.display()
        );
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        if path.metadata()?.permissions().mode() & 0o111 == 0 {
            anyhow::bail!("bmux executable is not executable: {}", path.display());
        }
    }
    Ok(path)
}

fn render_declaration(spec: &ServiceSpec) -> Result<String> {
    match spec.platform {
        Platform::MacOs => Ok(render_launchd_plist(spec)),
        Platform::Linux => Ok(render_systemd_unit(spec)),
        Platform::Windows => Ok(render_windows_task_xml(spec)),
        Platform::Unsupported => anyhow::bail!("unsupported autostart platform"),
    }
}

fn ownership_marker(spec: &ServiceSpec) -> String {
    format!(
        "{OWNERSHIP_MARKER}; schema={SCHEMA_VERSION}; platform={}; runtime={}; service={}",
        spec.platform.name(),
        spec.runtime,
        spec.service_id
    )
}

fn render_launchd_plist(spec: &ServiceSpec) -> String {
    let mut arguments = String::new();
    for argument in std::iter::once(spec.executable.to_string_lossy().into_owned())
        .chain(spec.arguments.iter().cloned())
    {
        arguments.push_str("    <string>");
        arguments.push_str(&escape_xml(&argument));
        arguments.push_str("</string>\n");
    }
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
<!-- {} -->\n\
<plist version=\"1.0\">\n<dict>\n  <key>Label</key>\n  <string>{}</string>\n  <key>ProgramArguments</key>\n  <array>\n{}  </array>\n  <key>RunAtLoad</key>\n  <true/>\n  <key>KeepAlive</key>\n  <dict>\n    <key>Crashed</key>\n    <true/>\n    <key>SuccessfulExit</key>\n    <false/>\n  </dict>\n  <key>ProcessType</key>\n  <string>Background</string>\n  <key>WorkingDirectory</key>\n  <string>{}</string>\n  <key>StandardOutPath</key>\n  <string>{}</string>\n  <key>StandardErrorPath</key>\n  <string>{}</string>\n</dict>\n</plist>\n",
        escape_xml(&ownership_marker(spec)),
        escape_xml(&spec.service_id),
        arguments,
        escape_xml(&launchd_working_directory(spec)),
        escape_xml(&launchd_log_path(spec, "out").to_string_lossy()),
        escape_xml(&launchd_log_path(spec, "err").to_string_lossy()),
    )
}

fn launchd_working_directory(spec: &ServiceSpec) -> String {
    spec.user_home.to_string_lossy().into_owned()
}

fn launchd_log_path(spec: &ServiceSpec, stream: &str) -> PathBuf {
    spec.user_home
        .join("Library/Logs/bmux")
        .join(format!("autostart-{}-{stream}.log", spec.runtime))
}

fn render_systemd_unit(spec: &ServiceSpec) -> String {
    let executable = systemd_quote(&spec.executable.to_string_lossy());
    let arguments = spec
        .arguments
        .iter()
        .map(|argument| systemd_quote(argument))
        .collect::<Vec<_>>()
        .join(" ");
    format!(
        "# {}\n\
[Unit]\nDescription=bmux server ({})\nAfter=graphical-session.target\nPartOf=graphical-session.target\n\n\
[Service]\nType=simple\nExecStart={executable} {arguments}\nRestart=on-failure\nRestartSec=1s\n\n\
[Install]\nWantedBy=default.target\n",
        ownership_marker(spec),
        spec.runtime
    )
}

fn render_windows_task_xml(spec: &ServiceSpec) -> String {
    let arguments = spec
        .arguments
        .iter()
        .map(|argument| windows_command_line_quote(argument))
        .collect::<Vec<_>>()
        .join(" ");
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-16\"?>\n\
<Task version=\"1.4\" xmlns=\"http://schemas.microsoft.com/windows/2004/02/mit/task\">\n\
  <RegistrationInfo><Description>{}</Description></RegistrationInfo>\n\
  <Triggers><LogonTrigger><Enabled>true</Enabled></LogonTrigger></Triggers>\n\
  <Principals><Principal id=\"Author\"><LogonType>InteractiveToken</LogonType><RunLevel>LeastPrivilege</RunLevel></Principal></Principals>\n\
  <Settings><MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy><RestartOnFailure><Interval>PT1M</Interval><Count>3</Count></RestartOnFailure><ExecutionTimeLimit>PT0S</ExecutionTimeLimit><Enabled>true</Enabled></Settings>\n\
  <Actions Context=\"Author\"><Exec><Command>{}</Command><Arguments>{}</Arguments></Exec></Actions>\n\
</Task>\n",
        escape_xml(&ownership_marker(spec)),
        escape_xml(&spec.executable.to_string_lossy()),
        escape_xml(&arguments)
    )
}

fn escape_xml(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn systemd_quote(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

fn windows_command_line_quote(value: &str) -> String {
    if !value.is_empty() && !value.chars().any(|ch| ch.is_whitespace() || ch == '"') {
        return value.to_string();
    }
    let mut quoted = String::from("\"");
    let mut backslashes = 0;
    for ch in value.chars() {
        if ch == '\\' {
            backslashes += 1;
        } else if ch == '"' {
            quoted.push_str(&"\\".repeat(backslashes * 2 + 1));
            quoted.push('"');
            backslashes = 0;
        } else {
            quoted.push_str(&"\\".repeat(backslashes));
            backslashes = 0;
            quoted.push(ch);
        }
    }
    quoted.push_str(&"\\".repeat(backslashes * 2));
    quoted.push('"');
    quoted
}

fn inspect_declaration(spec: &ServiceSpec) -> Result<DeclarationInspection> {
    if spec.platform == Platform::Windows {
        return inspect_windows_task(spec, &NativeCommandRunner);
    }
    let Some(path) = spec.declaration_path.as_deref() else {
        return Ok(DeclarationInspection {
            ownership: Ownership::NotInstalled,
            schema_version: None,
            executable: None,
        });
    };
    inspect_file_for_spec(path, spec)
}

#[cfg(test)]
fn inspect_file(path: &Path) -> Result<DeclarationInspection> {
    inspect_file_inner(path, None)
}

fn inspect_file_for_spec(path: &Path, spec: &ServiceSpec) -> Result<DeclarationInspection> {
    inspect_file_inner(path, Some(spec))
}

fn inspect_file_inner(
    path: &Path,
    expected_spec: Option<&ServiceSpec>,
) -> Result<DeclarationInspection> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(DeclarationInspection {
                ownership: Ownership::NotInstalled,
                schema_version: None,
                executable: None,
            });
        }
        Err(error) => {
            return Err(error).with_context(|| format!("failed inspecting {}", path.display()));
        }
    };
    if metadata.file_type().is_symlink() || metadata.permissions().readonly() {
        return Ok(DeclarationInspection {
            ownership: Ownership::ExternallyManaged,
            schema_version: None,
            executable: None,
        });
    }
    if !metadata.is_file() {
        return Ok(DeclarationInspection {
            ownership: Ownership::Conflict,
            schema_version: None,
            executable: None,
        });
    }
    let contents = std::fs::read_to_string(path)
        .with_context(|| format!("failed reading {}", path.display()))?;
    let expected_marker = ownership_marker_for_file(path, &contents).filter(|marker| {
        expected_spec.is_none_or(|spec| ownership_marker_matches_spec(marker, spec))
    });
    let owned = expected_marker.is_some();
    let externally_managed = !owned && looks_like_native_declaration(path, &contents);
    let schema_version = expected_marker
        .as_deref()
        .and_then(parse_ownership_marker)
        .map(|fields| fields.schema_version);
    let executable = extract_declared_executable(&contents);
    Ok(DeclarationInspection {
        ownership: if owned {
            Ownership::BmuxManaged
        } else if externally_managed {
            Ownership::ExternallyManaged
        } else {
            Ownership::Conflict
        },
        schema_version,
        executable,
    })
}

fn looks_like_native_declaration(path: &Path, contents: &str) -> bool {
    match path.extension().and_then(OsStr::to_str) {
        Some("plist") => {
            contents.contains("<key>Label</key>")
                && contents.contains("<key>ProgramArguments</key>")
        }
        Some("service") => contents.contains("[Service]") && contents.contains("ExecStart="),
        _ => false,
    }
}

fn ownership_marker_matches_spec(marker: &str, spec: &ServiceSpec) -> bool {
    parse_ownership_marker(marker).is_some_and(|fields| {
        fields.schema_version <= SCHEMA_VERSION
            && fields.platform == spec.platform.name()
            && fields.runtime == spec.runtime
            && fields.service == spec.service_id
    })
}

fn ownership_marker_for_file(path: &Path, contents: &str) -> Option<String> {
    let marker = contents.lines().find_map(|line| {
        let trimmed = line.trim();
        trimmed
            .strip_prefix("<!-- ")
            .and_then(|value| value.strip_suffix(" -->"))
            .or_else(|| trimmed.strip_prefix("# "))
    })?;
    let fields = parse_ownership_marker(marker)?;
    if fields.schema_version > SCHEMA_VERSION {
        return None;
    }
    let file_name = path.file_name()?.to_str()?;
    let expected_service = match fields.platform.as_str() {
        "macos" => file_name.strip_suffix(".plist")?,
        "linux" => file_name,
        _ => return None,
    };
    (fields.service == expected_service
        && native_service_id(&fields.platform, &fields.runtime)? == fields.service)
        .then(|| marker.to_string())
}

struct OwnershipFields {
    schema_version: u32,
    platform: String,
    runtime: String,
    service: String,
}

fn parse_ownership_marker(marker: &str) -> Option<OwnershipFields> {
    let mut parts = marker.split("; ");
    if parts.next()? != OWNERSHIP_MARKER {
        return None;
    }
    let schema_version = parts.next()?.strip_prefix("schema=")?.parse().ok()?;
    let platform = parts.next()?.strip_prefix("platform=")?.to_string();
    let runtime = parts.next()?.strip_prefix("runtime=")?.to_string();
    let service = parts.next()?.strip_prefix("service=")?.to_string();
    if parts.next().is_some() || validate_runtime_component(&runtime).is_err() {
        return None;
    }
    Some(OwnershipFields {
        schema_version,
        platform,
        runtime,
        service,
    })
}

fn native_service_id(platform: &str, runtime: &str) -> Option<String> {
    match platform {
        "macos" => Some(if runtime == "default" {
            "dev.bmux.server".to_string()
        } else {
            format!("dev.bmux.server.{runtime}")
        }),
        "linux" => Some(if runtime == "default" {
            "bmux-server.service".to_string()
        } else {
            format!("bmux-server-{runtime}.service")
        }),
        "windows" => Some(if runtime == "default" {
            "bmux-server".to_string()
        } else {
            format!("bmux-server-{runtime}")
        }),
        _ => None,
    }
}

fn extract_declared_executable(contents: &str) -> Option<String> {
    if let Some(exec) = contents
        .lines()
        .find_map(|line| line.strip_prefix("ExecStart="))
    {
        return parse_first_quoted(exec);
    }
    let marker = "<key>ProgramArguments</key>";
    let after = contents.split_once(marker)?.1;
    let string_start = after.find("<string>")? + "<string>".len();
    let string_end = after[string_start..].find("</string>")? + string_start;
    Some(unescape_xml(&after[string_start..string_end]))
}

fn parse_first_quoted(value: &str) -> Option<String> {
    let value = value.trim();
    if let Some(rest) = value.strip_prefix('"') {
        let mut escaped = false;
        let mut result = String::new();
        for ch in rest.chars() {
            if escaped {
                result.push(ch);
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                return Some(result);
            } else {
                result.push(ch);
            }
        }
        None
    } else {
        value.split_whitespace().next().map(ToString::to_string)
    }
}

fn unescape_xml(value: &str) -> String {
    value
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&gt;", ">")
        .replace("&lt;", "<")
        .replace("&amp;", "&")
}

fn directory_lacks_owner_write(metadata: &std::fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        metadata.permissions().mode() & 0o200 == 0
    }
    #[cfg(not(unix))]
    {
        metadata.permissions().readonly()
    }
}

fn ensure_mutable_declaration_path(path: &Path) -> Result<()> {
    if path.starts_with("/nix/store") {
        anyhow::bail!(
            "declaration path is in immutable Nix store: {}",
            path.display()
        );
    }
    let mut current = path.parent();
    while let Some(ancestor) = current {
        match std::fs::symlink_metadata(ancestor) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() {
                    anyhow::bail!(
                        "declaration path has externally managed symlink ancestor: {}",
                        ancestor.display()
                    );
                }
                if metadata.permissions().readonly() || directory_lacks_owner_write(&metadata) {
                    anyhow::bail!("declaration directory is read-only: {}", ancestor.display());
                }
                let canonical = ancestor.canonicalize().with_context(|| {
                    format!(
                        "failed resolving declaration directory {}",
                        ancestor.display()
                    )
                })?;
                if canonical.starts_with("/nix/store") {
                    anyhow::bail!(
                        "declaration directory resolves into immutable Nix store: {}",
                        canonical.display()
                    );
                }
                break;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("failed inspecting {}", ancestor.display()));
            }
        }
        current = ancestor.parent();
    }
    Ok(())
}

fn install_with_runner(
    spec: &ServiceSpec,
    declaration: &str,
    no_start: bool,
    runner: &dyn CommandRunner,
) -> Result<()> {
    if spec.platform == Platform::Windows {
        return install_windows_task(spec, declaration, no_start, runner);
    }
    if spec.platform == Platform::MacOs
        && !cfg!(test)
        && let Some(parent) = launchd_log_path(spec, "out").parent()
    {
        std::fs::create_dir_all(parent).with_context(|| {
            format!("failed creating launchd log directory {}", parent.display())
        })?;
    }
    let path = spec
        .declaration_path
        .as_deref()
        .context("missing declaration path")?;
    ensure_mutable_declaration_path(path)?;
    let inspection = inspect_file_for_spec(path, spec)?;
    let (manager_state, _) = manager_status(spec, runner);
    let plan = plan_install(inspection.ownership, manager_state).map_err(|error| {
        anyhow::anyhow!(
            "refusing to install autostart declaration {}: {error}",
            path.display()
        )
    })?;
    let previous = matches!(plan, InstallPlan::Update)
        .then(|| std::fs::read(path))
        .transpose()
        .with_context(|| format!("failed preserving {} before update", path.display()))?;
    let had_previous = previous.is_some();
    atomic_write_owned(path, declaration.as_bytes(), spec)?;
    if let Err(error) = manager_install(spec, no_start, runner) {
        let rollback = previous.map_or_else(
            || {
                std::fs::remove_file(path).with_context(|| {
                    format!("failed removing {} after install failure", path.display())
                })
            },
            |previous| atomic_write_owned(path, &previous, spec),
        );
        if let Err(rollback_error) = rollback {
            return Err(error).context(format!(
                "native manager install failed and declaration rollback also failed: {rollback_error:#}"
            ));
        }
        if had_previous {
            let rollback_start = manager_install(spec, false, runner);
            if let Err(rollback_error) = rollback_start {
                return Err(error).context(format!(
                    "native manager install failed; declaration rollback succeeded but restoring the prior service registration failed: {rollback_error:#}"
                ));
            }
        }
        return Err(error).context("native manager install failed; declaration was rolled back");
    }
    Ok(())
}

fn uninstall_with_runner(spec: &ServiceSpec, runner: &dyn CommandRunner) -> Result<()> {
    if spec.platform == Platform::Windows {
        return uninstall_windows_task(spec, runner);
    }
    let path = spec
        .declaration_path
        .as_deref()
        .context("missing declaration path")?;
    ensure_mutable_declaration_path(path)?;
    let inspection = inspect_file_for_spec(path, spec)?;
    let (manager_state, _) = manager_status(spec, runner);
    let plan = plan_uninstall(inspection.ownership, manager_state).map_err(|error| {
        anyhow::anyhow!(
            "refusing to uninstall autostart declaration {}: {error}",
            path.display()
        )
    })?;
    if plan == UninstallPlan::Noop {
        return Ok(());
    }
    manager_uninstall(spec, runner)?;
    let current = inspect_file_for_spec(path, spec)?;
    if current.ownership != Ownership::BmuxManaged {
        anyhow::bail!(
            "autostart declaration ownership changed during uninstall: {}",
            path.display()
        );
    }
    std::fs::remove_file(path).with_context(|| format!("failed removing {}", path.display()))?;
    #[cfg(target_os = "linux")]
    if spec.platform == Platform::Linux {
        run_checked(
            runner,
            "systemctl",
            &["--user", "daemon-reload"],
            "reload systemd user manager",
        )?;
    }
    Ok(())
}

fn atomic_write_owned(path: &Path, bytes: &[u8], spec: &ServiceSpec) -> Result<()> {
    use std::io::Write as _;

    let parent = path
        .parent()
        .context("autostart declaration has no parent")?;
    std::fs::create_dir_all(parent)
        .with_context(|| format!("failed creating {}", parent.display()))?;
    let temp = parent.join(format!(
        ".{}.bmux-tmp-{}",
        path.file_name().unwrap_or_default().to_string_lossy(),
        std::process::id()
    ));
    let result = (|| -> Result<()> {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        let mut file = options
            .open(&temp)
            .with_context(|| format!("failed creating {}", temp.display()))?;
        file.write_all(bytes)
            .with_context(|| format!("failed writing {}", temp.display()))?;
        file.sync_all()
            .with_context(|| format!("failed syncing {}", temp.display()))?;
        let current = inspect_file_for_spec(path, spec)?;
        if !matches!(
            current.ownership,
            Ownership::NotInstalled | Ownership::BmuxManaged
        ) {
            anyhow::bail!(
                "autostart declaration ownership changed before update: {}",
                path.display()
            );
        }
        std::fs::rename(&temp, path)
            .with_context(|| format!("failed replacing {}", path.display()))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result
}

#[cfg(all(target_os = "macos", not(test)))]
#[allow(clippy::wildcard_imports)] // Private cfg backend delegates to its sibling implementation.
mod native {
    use super::*;

    pub(super) fn manager_install(
        spec: &ServiceSpec,
        no_start: bool,
        runner: &dyn CommandRunner,
    ) -> Result<()> {
        manager_install_launchd(spec, no_start, runner)
    }

    pub(super) fn manager_uninstall(spec: &ServiceSpec, runner: &dyn CommandRunner) -> Result<()> {
        manager_uninstall_launchd(spec, runner)
    }

    pub(super) fn manager_status(
        spec: &ServiceSpec,
        runner: &dyn CommandRunner,
    ) -> (ManagerState, Option<String>) {
        manager_status_launchd(spec, runner)
    }
}

#[cfg(all(target_os = "linux", not(test)))]
#[allow(clippy::wildcard_imports)] // Private cfg backend delegates to its sibling implementation.
mod native {
    use super::*;

    pub(super) fn manager_install(
        spec: &ServiceSpec,
        no_start: bool,
        runner: &dyn CommandRunner,
    ) -> Result<()> {
        manager_install_systemd(spec, no_start, runner)
    }

    pub(super) fn manager_uninstall(spec: &ServiceSpec, runner: &dyn CommandRunner) -> Result<()> {
        manager_uninstall_systemd(spec, runner)
    }

    pub(super) fn manager_status(
        spec: &ServiceSpec,
        runner: &dyn CommandRunner,
    ) -> (ManagerState, Option<String>) {
        manager_status_systemd(spec, runner)
    }
}

#[cfg(all(target_os = "windows", not(test)))]
#[allow(clippy::wildcard_imports)] // Private cfg backend delegates to its sibling implementation.
mod native {
    use super::*;

    pub(super) fn manager_status(
        spec: &ServiceSpec,
        runner: &dyn CommandRunner,
    ) -> (ManagerState, Option<String>) {
        manager_status_windows(spec, runner)
    }
}

#[cfg(all(
    not(test),
    not(any(target_os = "macos", target_os = "linux", target_os = "windows"))
))]
#[allow(clippy::wildcard_imports)] // Private cfg backend delegates to its sibling implementation.
mod native {
    use super::*;

    pub(super) fn manager_install(
        _spec: &ServiceSpec,
        _no_start: bool,
        _runner: &dyn CommandRunner,
    ) -> Result<()> {
        anyhow::bail!("unsupported autostart platform")
    }

    pub(super) fn manager_uninstall(
        _spec: &ServiceSpec,
        _runner: &dyn CommandRunner,
    ) -> Result<()> {
        anyhow::bail!("unsupported autostart platform")
    }

    pub(super) fn manager_status(
        spec: &ServiceSpec,
        _runner: &dyn CommandRunner,
    ) -> (ManagerState, Option<String>) {
        (
            ManagerState::Unsupported,
            Some(format!(
                "no native autostart backend for {}",
                spec.platform.name()
            )),
        )
    }
}

fn manager_install(spec: &ServiceSpec, no_start: bool, runner: &dyn CommandRunner) -> Result<()> {
    #[cfg(test)]
    return match spec.platform {
        Platform::MacOs => manager_install_launchd(spec, no_start, runner),
        Platform::Linux => manager_install_systemd(spec, no_start, runner),
        Platform::Windows => unreachable!("windows install handled separately"),
        Platform::Unsupported => anyhow::bail!("unsupported autostart platform"),
    };
    #[cfg(all(not(test), target_os = "windows"))]
    {
        let _ = (spec, no_start, runner);
        unreachable!("windows install handled separately")
    }
    #[cfg(all(not(test), not(target_os = "windows")))]
    native::manager_install(spec, no_start, runner)
}

fn manager_uninstall(spec: &ServiceSpec, runner: &dyn CommandRunner) -> Result<()> {
    #[cfg(test)]
    return match spec.platform {
        Platform::MacOs => manager_uninstall_launchd(spec, runner),
        Platform::Linux => manager_uninstall_systemd(spec, runner),
        Platform::Windows => unreachable!("windows uninstall handled separately"),
        Platform::Unsupported => anyhow::bail!("unsupported autostart platform"),
    };
    #[cfg(all(not(test), target_os = "windows"))]
    {
        let _ = (spec, runner);
        unreachable!("windows uninstall handled separately")
    }
    #[cfg(all(not(test), not(target_os = "windows")))]
    native::manager_uninstall(spec, runner)
}

#[cfg(any(target_os = "macos", test))]
fn manager_install_launchd(
    spec: &ServiceSpec,
    no_start: bool,
    runner: &dyn CommandRunner,
) -> Result<()> {
    let domain = launchd_domain(runner)?;
    let path = spec
        .declaration_path
        .as_deref()
        .context("missing launchd path")?;
    let target = format!("{domain}/{}", spec.service_id);
    let current = runner.run(OsStr::new("launchctl"), &os_args(&["print", &target]))?;
    if current.success {
        let result = runner.run(OsStr::new("launchctl"), &os_args(&["bootout", &target]))?;
        if !result.success && !launchd_absent(&result) {
            anyhow::bail!(
                "failed to boot out existing launchd agent: {}",
                command_error(&result)
            );
        }
    } else if !launchd_absent(&current) {
        anyhow::bail!(
            "failed to inspect existing launchd agent: {}",
            command_error(&current)
        );
    }
    if !no_start {
        run_checked(
            runner,
            "launchctl",
            &["bootstrap", &domain, &path.to_string_lossy()],
            "bootstrap launchd agent",
        )?;
    }
    Ok(())
}

#[cfg(all(not(target_os = "macos"), not(test)))]
#[allow(dead_code)] // Non-native stub keeps pure cross-platform tests callable.
fn manager_install_launchd(
    _spec: &ServiceSpec,
    _no_start: bool,
    _runner: &dyn CommandRunner,
) -> Result<()> {
    anyhow::bail!("launchd backend is unavailable on this target")
}

#[cfg(any(target_os = "macos", test))]
fn manager_uninstall_launchd(spec: &ServiceSpec, runner: &dyn CommandRunner) -> Result<()> {
    let domain = launchd_domain(runner)?;
    let target = format!("{domain}/{}", spec.service_id);
    let result = runner.run(OsStr::new("launchctl"), &os_args(&["bootout", &target]))?;
    if !result.success && !launchd_absent(&result) {
        anyhow::bail!(
            "failed to boot out launchd agent: {}",
            command_error(&result)
        );
    }
    Ok(())
}

#[cfg(all(not(target_os = "macos"), not(test)))]
#[allow(dead_code)] // Non-native stub keeps pure cross-platform tests callable.
fn manager_uninstall_launchd(_spec: &ServiceSpec, _runner: &dyn CommandRunner) -> Result<()> {
    anyhow::bail!("launchd backend is unavailable on this target")
}

#[cfg(any(target_os = "macos", test))]
fn launchd_domain(runner: &dyn CommandRunner) -> Result<String> {
    let result = runner.run(OsStr::new("id"), &os_args(&["-u"]))?;
    if !result.success
        || result.stdout.is_empty()
        || !result.stdout.chars().all(|ch| ch.is_ascii_digit())
    {
        anyhow::bail!(
            "failed resolving user id for launchd: {}",
            command_error(&result)
        );
    }
    Ok(format!("gui/{}", result.stdout))
}

#[cfg(any(target_os = "macos", test))]
fn launchd_absent(result: &CommandResult) -> bool {
    result.code == Some(3)
        || result.code == Some(5)
        || result.stderr.contains("Could not find service")
        || result.stderr.contains("No such process")
}

#[cfg(any(target_os = "linux", test))]
#[allow(dead_code)] // Compiled in tests to exercise non-native backends.
fn manager_install_systemd(
    spec: &ServiceSpec,
    no_start: bool,
    runner: &dyn CommandRunner,
) -> Result<()> {
    run_checked(
        runner,
        "systemctl",
        &["--user", "daemon-reload"],
        "reload systemd user manager",
    )?;
    run_checked(
        runner,
        "systemctl",
        &["--user", "enable", &spec.service_id],
        "enable systemd user service",
    )?;
    if !no_start {
        run_checked(
            runner,
            "systemctl",
            &["--user", "restart", &spec.service_id],
            "start systemd user service",
        )?;
    }
    Ok(())
}

#[cfg(all(not(target_os = "linux"), not(test)))]
#[allow(dead_code)] // Compiled in tests to exercise non-native backends.
fn manager_install_systemd(
    _spec: &ServiceSpec,
    _no_start: bool,
    _runner: &dyn CommandRunner,
) -> Result<()> {
    anyhow::bail!("systemd backend is unavailable on this target")
}

#[cfg(any(target_os = "linux", test))]
#[allow(dead_code)] // Compiled in tests to exercise non-native backends.
fn manager_uninstall_systemd(spec: &ServiceSpec, runner: &dyn CommandRunner) -> Result<()> {
    let result = runner.run(
        OsStr::new("systemctl"),
        &os_args(&["--user", "disable", "--now", &spec.service_id]),
    )?;
    if !result.success
        && !result.stderr.contains("does not exist")
        && !result.stderr.contains("not loaded")
    {
        anyhow::bail!(
            "failed to disable systemd user service: {}",
            command_error(&result)
        );
    }
    Ok(())
}

#[cfg(all(not(target_os = "linux"), not(test)))]
#[allow(dead_code)] // Compiled in tests to exercise non-native backends.
fn manager_uninstall_systemd(_spec: &ServiceSpec, _runner: &dyn CommandRunner) -> Result<()> {
    anyhow::bail!("systemd backend is unavailable on this target")
}

fn manager_status(
    spec: &ServiceSpec,
    runner: &dyn CommandRunner,
) -> (ManagerState, Option<String>) {
    #[cfg(test)]
    return match spec.platform {
        Platform::MacOs => manager_status_launchd(spec, runner),
        Platform::Linux => manager_status_systemd(spec, runner),
        Platform::Windows => manager_status_windows(spec, runner),
        Platform::Unsupported => (
            ManagerState::Unsupported,
            Some("unsupported autostart platform".to_string()),
        ),
    };
    #[cfg(not(test))]
    native::manager_status(spec, runner)
}

#[cfg(any(target_os = "macos", test))]
fn manager_status_launchd(
    spec: &ServiceSpec,
    runner: &dyn CommandRunner,
) -> (ManagerState, Option<String>) {
    let domain = match launchd_domain(runner) {
        Ok(domain) => domain,
        Err(error) => return (ManagerState::Unknown, Some(error.to_string())),
    };
    let target = format!("{domain}/{}", spec.service_id);
    match runner.run(OsStr::new("launchctl"), &os_args(&["print", &target])) {
        Ok(result) if result.success => {
            let running = result
                .stdout
                .lines()
                .any(|line| line.trim_start().starts_with("pid ="));
            (
                if running {
                    ManagerState::Running
                } else {
                    ManagerState::Registered
                },
                None,
            )
        }
        Ok(result) if launchd_absent(&result) => (ManagerState::NotRegistered, None),
        Ok(result) => (ManagerState::Unknown, Some(command_error(&result))),
        Err(error) => (ManagerState::Unknown, Some(error.to_string())),
    }
}

#[cfg(all(not(target_os = "macos"), not(test)))]
#[allow(dead_code)] // Non-native stub keeps pure cross-platform tests callable.
fn manager_status_launchd(
    _spec: &ServiceSpec,
    _runner: &dyn CommandRunner,
) -> (ManagerState, Option<String>) {
    (
        ManagerState::Unsupported,
        Some("launchd backend unavailable".to_string()),
    )
}

#[cfg(any(target_os = "linux", test))]
#[allow(dead_code)] // Compiled in tests to exercise non-native backends.
fn manager_status_systemd(
    spec: &ServiceSpec,
    runner: &dyn CommandRunner,
) -> (ManagerState, Option<String>) {
    let active = runner.run(
        OsStr::new("systemctl"),
        &os_args(&["--user", "is-active", &spec.service_id]),
    );
    match active {
        Ok(result) if result.success && result.stdout == "active" => (ManagerState::Running, None),
        Ok(_) => {
            match runner.run(
                OsStr::new("systemctl"),
                &os_args(&["--user", "is-enabled", &spec.service_id]),
            ) {
                Ok(result) if result.success => (ManagerState::Registered, None),
                Ok(result) if matches!(result.stdout.as_str(), "disabled" | "not-found") => {
                    (ManagerState::NotRegistered, None)
                }
                Ok(result) => (ManagerState::Unknown, Some(command_error(&result))),
                Err(error) => (ManagerState::Unknown, Some(error.to_string())),
            }
        }
        Err(error) => (ManagerState::Unknown, Some(error.to_string())),
    }
}

#[cfg(all(not(target_os = "linux"), not(test)))]
#[allow(dead_code)] // Compiled in tests to exercise non-native backends.
fn manager_status_systemd(
    _spec: &ServiceSpec,
    _runner: &dyn CommandRunner,
) -> (ManagerState, Option<String>) {
    (
        ManagerState::Unsupported,
        Some("systemd backend unavailable".to_string()),
    )
}

#[cfg(any(target_os = "windows", test))]
fn inspect_windows_task(
    spec: &ServiceSpec,
    runner: &dyn CommandRunner,
) -> Result<DeclarationInspection> {
    let result = runner.run(
        OsStr::new("schtasks.exe"),
        &os_args(&["/Query", "/TN", &spec.service_id, "/XML"]),
    )?;
    if !result.success {
        if result.code == Some(1) {
            return Ok(DeclarationInspection {
                ownership: Ownership::NotInstalled,
                schema_version: None,
                executable: None,
            });
        }
        anyhow::bail!("failed querying scheduled task: {}", command_error(&result));
    }
    let owned = result
        .stdout
        .split("<Description>")
        .nth(1)
        .is_some_and(|rest| {
            rest.split("</Description>")
                .next()
                .is_some_and(|description| {
                    ownership_marker_matches_spec(&unescape_xml(description), spec)
                })
        });
    let executable = extract_windows_executable(&result.stdout);
    Ok(DeclarationInspection {
        ownership: if owned {
            Ownership::BmuxManaged
        } else {
            Ownership::Conflict
        },
        schema_version: owned.then_some(SCHEMA_VERSION),
        executable,
    })
}

#[cfg(all(not(target_os = "windows"), not(test)))]
#[allow(clippy::unnecessary_wraps)] // Matches the native backend signature at the cfg boundary.
fn inspect_windows_task(
    _spec: &ServiceSpec,
    _runner: &dyn CommandRunner,
) -> Result<DeclarationInspection> {
    Ok(DeclarationInspection {
        ownership: Ownership::NotInstalled,
        schema_version: None,
        executable: None,
    })
}

#[cfg(any(target_os = "windows", test))]
fn extract_windows_executable(contents: &str) -> Option<String> {
    let start = contents.find("<Command>")? + "<Command>".len();
    let end = contents[start..].find("</Command>")? + start;
    Some(unescape_xml(&contents[start..end]))
}

#[cfg(any(target_os = "windows", test))]
fn install_windows_task(
    spec: &ServiceSpec,
    declaration: &str,
    no_start: bool,
    runner: &dyn CommandRunner,
) -> Result<()> {
    let inspection = inspect_windows_task(spec, runner)?;
    if !matches!(
        inspection.ownership,
        Ownership::NotInstalled | Ownership::BmuxManaged
    ) {
        anyhow::bail!(
            "refusing to overwrite unrecognized scheduled task {}",
            spec.service_id
        );
    }
    let temp = std::env::temp_dir().join(format!(
        "bmux-autostart-{}-{}.xml",
        std::process::id(),
        spec.runtime
    ));
    std::fs::write(
        &temp,
        declaration
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect::<Vec<_>>(),
    )?;
    let result = run_checked(
        runner,
        "schtasks.exe",
        &[
            "/Create",
            "/F",
            "/TN",
            &spec.service_id,
            "/XML",
            &temp.to_string_lossy(),
        ],
        "register scheduled task",
    );
    let _ = std::fs::remove_file(&temp);
    result?;
    if !no_start {
        run_checked(
            runner,
            "schtasks.exe",
            &["/Run", "/TN", &spec.service_id],
            "start scheduled task",
        )?;
    }
    Ok(())
}

#[cfg(all(not(target_os = "windows"), not(test)))]
fn install_windows_task(
    _spec: &ServiceSpec,
    _declaration: &str,
    _no_start: bool,
    _runner: &dyn CommandRunner,
) -> Result<()> {
    anyhow::bail!("Windows Task Scheduler backend is unavailable on this target")
}

#[cfg(any(target_os = "windows", test))]
fn uninstall_windows_task(spec: &ServiceSpec, runner: &dyn CommandRunner) -> Result<()> {
    let inspection = inspect_windows_task(spec, runner)?;
    match inspection.ownership {
        Ownership::NotInstalled => return Ok(()),
        Ownership::BmuxManaged => {}
        _ => anyhow::bail!(
            "refusing to remove unrecognized scheduled task {}",
            spec.service_id
        ),
    }
    run_checked(
        runner,
        "schtasks.exe",
        &["/Delete", "/F", "/TN", &spec.service_id],
        "delete scheduled task",
    )
}

#[cfg(all(not(target_os = "windows"), not(test)))]
fn uninstall_windows_task(_spec: &ServiceSpec, _runner: &dyn CommandRunner) -> Result<()> {
    anyhow::bail!("Windows Task Scheduler backend is unavailable on this target")
}

#[cfg(any(target_os = "windows", test))]
#[allow(dead_code)] // Compiled in tests to exercise non-native backends.
fn manager_status_windows(
    spec: &ServiceSpec,
    runner: &dyn CommandRunner,
) -> (ManagerState, Option<String>) {
    let result = runner.run(
        OsStr::new("schtasks.exe"),
        &os_args(&["/Query", "/TN", &spec.service_id, "/FO", "LIST", "/V"]),
    );
    match result {
        Ok(result) if result.success => {
            let status = result.stdout.lines().find_map(|line| {
                let (key, value) = line.split_once(':')?;
                key.trim()
                    .eq_ignore_ascii_case("status")
                    .then(|| value.trim())
            });
            let running = status.is_some_and(|value| value.eq_ignore_ascii_case("running"));
            (
                if running {
                    ManagerState::Running
                } else {
                    ManagerState::Registered
                },
                None,
            )
        }
        Ok(result) if result.code == Some(1) => (ManagerState::NotRegistered, None),
        Ok(result) => (ManagerState::Unknown, Some(command_error(&result))),
        Err(error) => (ManagerState::Unknown, Some(error.to_string())),
    }
}

#[cfg(all(not(target_os = "windows"), not(test)))]
#[allow(dead_code)] // Compiled in tests to exercise non-native backends.
fn manager_status_windows(
    _spec: &ServiceSpec,
    _runner: &dyn CommandRunner,
) -> (ManagerState, Option<String>) {
    (
        ManagerState::Unsupported,
        Some("Windows Task Scheduler backend unavailable".to_string()),
    )
}

fn run_checked(
    runner: &dyn CommandRunner,
    program: &str,
    arguments: &[&str],
    operation: &str,
) -> Result<()> {
    let result = runner.run(OsStr::new(program), &os_args(arguments))?;
    if result.success {
        Ok(())
    } else {
        anyhow::bail!("failed to {operation}: {}", command_error(&result))
    }
}

fn os_args(arguments: &[&str]) -> Vec<OsString> {
    arguments.iter().map(OsString::from).collect()
}

fn command_error(result: &CommandResult) -> String {
    if result.stderr.is_empty() {
        format!("exit status {:?}: {}", result.code, result.stdout)
    } else {
        format!("exit status {:?}: {}", result.code, result.stderr)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::time::{SystemTime, UNIX_EPOCH};

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[derive(Default)]
    struct FakeRunner {
        calls: Mutex<Vec<(OsString, Vec<OsString>)>>,
        results: Mutex<Vec<CommandResult>>,
    }

    impl FakeRunner {
        fn with_results(results: Vec<CommandResult>) -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                results: Mutex::new(results.into_iter().rev().collect()),
            }
        }

        fn calls(&self) -> Vec<(OsString, Vec<OsString>)> {
            self.calls.lock().expect("calls lock").clone()
        }

        fn assert_exhausted(&self) {
            assert!(self.results.lock().expect("results lock").is_empty());
        }
    }

    impl CommandRunner for FakeRunner {
        fn run(&self, program: &OsStr, arguments: &[OsString]) -> Result<CommandResult> {
            self.calls
                .lock()
                .expect("calls lock")
                .push((program.to_os_string(), arguments.to_vec()));
            self.results
                .lock()
                .expect("results lock")
                .pop()
                .context("missing fake command result")
        }
    }

    struct ErrorRunner {
        message: &'static str,
    }

    impl CommandRunner for ErrorRunner {
        fn run(&self, _program: &OsStr, _arguments: &[OsString]) -> Result<CommandResult> {
            anyhow::bail!(self.message)
        }
    }

    fn success(stdout: &str) -> CommandResult {
        CommandResult {
            success: true,
            code: Some(0),
            stdout: stdout.to_string(),
            stderr: String::new(),
        }
    }

    fn failure(code: i32, stderr: &str) -> CommandResult {
        CommandResult {
            success: false,
            code: Some(code),
            stdout: String::new(),
            stderr: stderr.to_string(),
        }
    }

    fn temp_path(name: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time")
            .as_nanos();
        std::env::temp_dir()
            .join(format!(
                "bmux-autostart-test-{}-{unique}",
                std::process::id()
            ))
            .join(name)
    }

    fn spec(platform: Platform, runtime: &str, executable: &str) -> ServiceSpec {
        let mut spec = service_spec_with_executable(platform, runtime, PathBuf::from(executable))
            .expect("spec");
        spec.user_home = PathBuf::from("/home/test");
        spec.declaration_path = match platform {
            Platform::MacOs => Some(temp_path(&format!("{}.plist", spec.service_id))),
            Platform::Linux => Some(temp_path(&spec.service_id)),
            _ => None,
        };
        spec
    }

    #[test]
    fn fake_launchctl_install_update_no_start_status_and_uninstall_are_exact() {
        let spec = spec(Platform::MacOs, "work", "/opt/bmux");
        let path = spec.declaration_path.as_deref().expect("path");
        let domain = "gui/501";
        let target = "gui/501/dev.bmux.server.work";

        let install = FakeRunner::with_results(vec![
            success("501"),
            failure(5, "Could not find service"),
            success(""),
        ]);
        manager_install_launchd(&spec, false, &install).expect("install launchd");
        assert_eq!(
            install.calls(),
            vec![
                (OsString::from("id"), os_args(&["-u"])),
                (OsString::from("launchctl"), os_args(&["print", target]),),
                (
                    OsString::from("launchctl"),
                    os_args(&["bootstrap", domain, &path.to_string_lossy()]),
                ),
            ]
        );
        install.assert_exhausted();

        let update_no_start =
            FakeRunner::with_results(vec![success("501"), success("pid = 1"), success("")]);
        manager_install_launchd(&spec, true, &update_no_start).expect("update no-start");
        assert_eq!(
            update_no_start.calls(),
            vec![
                (OsString::from("id"), os_args(&["-u"])),
                (OsString::from("launchctl"), os_args(&["print", target]),),
                (OsString::from("launchctl"), os_args(&["bootout", target]),),
            ]
        );

        let running = FakeRunner::with_results(vec![success("501"), success("pid = 42")]);
        assert_eq!(
            manager_status_launchd(&spec, &running),
            (ManagerState::Running, None)
        );

        let uninstall = FakeRunner::with_results(vec![success("501"), success("")]);
        manager_uninstall_launchd(&spec, &uninstall).expect("uninstall launchd");
        assert_eq!(
            uninstall.calls(),
            vec![
                (OsString::from("id"), os_args(&["-u"])),
                (OsString::from("launchctl"), os_args(&["bootout", target]),),
            ]
        );
    }

    #[test]
    fn fake_systemctl_install_no_start_status_and_uninstall_are_exact() {
        let spec = spec(Platform::Linux, "work", "/opt/bmux");
        let install = FakeRunner::with_results(vec![success(""), success(""), success("")]);
        manager_install_systemd(&spec, false, &install).expect("install systemd");
        assert_eq!(
            install.calls(),
            vec![
                (
                    OsString::from("systemctl"),
                    os_args(&["--user", "daemon-reload"]),
                ),
                (
                    OsString::from("systemctl"),
                    os_args(&["--user", "enable", "bmux-server-work.service"]),
                ),
                (
                    OsString::from("systemctl"),
                    os_args(&["--user", "restart", "bmux-server-work.service"]),
                ),
            ]
        );

        let no_start = FakeRunner::with_results(vec![success(""), success("")]);
        manager_install_systemd(&spec, true, &no_start).expect("enable systemd");
        assert_eq!(no_start.calls().len(), 2);

        let inactive = FakeRunner::with_results(vec![failure(3, "inactive"), success("enabled")]);
        assert_eq!(
            manager_status_systemd(&spec, &inactive),
            (ManagerState::Registered, None)
        );

        let uninstall = FakeRunner::with_results(vec![success("")]);
        manager_uninstall_systemd(&spec, &uninstall).expect("uninstall systemd");
        assert_eq!(
            uninstall.calls(),
            vec![(
                OsString::from("systemctl"),
                os_args(&["--user", "disable", "--now", "bmux-server-work.service",]),
            )]
        );
    }

    #[test]
    fn fake_scheduler_register_start_status_conflict_and_uninstall_are_exact() {
        let spec = spec(Platform::Windows, "work", r"C:\bmux.exe");
        let declaration = render_windows_task_xml(&spec);
        let absent_xml = failure(1, "not found");
        let install = FakeRunner::with_results(vec![absent_xml, success(""), success("")]);
        install_windows_task(&spec, &declaration, false, &install).expect("install task");
        let calls = install.calls();
        assert_eq!(
            calls[0],
            (
                OsString::from("schtasks.exe"),
                os_args(&["/Query", "/TN", "bmux-server-work", "/XML"]),
            )
        );
        assert_eq!(calls[1].0, OsString::from("schtasks.exe"));
        assert_eq!(
            &calls[1].1[..5],
            &os_args(&["/Create", "/F", "/TN", "bmux-server-work", "/XML"])
        );
        assert_eq!(
            calls[2],
            (
                OsString::from("schtasks.exe"),
                os_args(&["/Run", "/TN", "bmux-server-work"]),
            )
        );

        let running = FakeRunner::with_results(vec![success("Status: Running")]);
        assert_eq!(
            manager_status_windows(&spec, &running),
            (ManagerState::Running, None)
        );

        let foreign = FakeRunner::with_results(vec![success(
            "<Task><Description>foreign</Description></Task>",
        )]);
        assert!(install_windows_task(&spec, &declaration, true, &foreign).is_err());
        assert_eq!(foreign.calls().len(), 1);

        let owned_xml = render_windows_task_xml(&spec);
        let uninstall = FakeRunner::with_results(vec![success(&owned_xml), success("")]);
        uninstall_windows_task(&spec, &uninstall).expect("uninstall task");
        assert_eq!(uninstall.calls().len(), 2);
    }

    #[test]
    fn fake_managers_surface_permission_malformed_and_nonzero_failures() {
        let launchd_spec = spec(Platform::MacOs, "work", "/opt/bmux");
        let malformed_uid = FakeRunner::with_results(vec![success("not-a-uid")]);
        assert_eq!(
            manager_status_launchd(&launchd_spec, &malformed_uid).0,
            ManagerState::Unknown
        );

        let linux_spec = spec(Platform::Linux, "work", "/opt/bmux");
        let permission = FakeRunner::with_results(vec![failure(1, "permission denied")]);
        assert!(manager_install_systemd(&linux_spec, false, &permission).is_err());

        let malformed =
            FakeRunner::with_results(vec![failure(3, "inactive"), failure(1, "garbage")]);
        assert_eq!(
            manager_status_systemd(&linux_spec, &malformed).0,
            ManagerState::Unknown
        );

        let windows_spec = spec(Platform::Windows, "work", r"C:\bmux.exe");
        let scheduler_error = FakeRunner::with_results(vec![failure(5, "access denied")]);
        assert_eq!(
            manager_status_windows(&windows_spec, &scheduler_error).0,
            ManagerState::Unknown
        );

        let timeout = ErrorRunner {
            message: "manager command timed out",
        };
        assert_eq!(
            manager_status_launchd(&launchd_spec, &timeout).0,
            ManagerState::Unknown
        );
        assert_eq!(
            manager_status_systemd(&linux_spec, &timeout).0,
            ManagerState::Unknown
        );
        assert_eq!(
            manager_status_windows(&windows_spec, &timeout).0,
            ManagerState::Unknown
        );
    }

    #[test]
    fn test_mutation_paths_are_confined_to_bmux_temp_directories() {
        for platform in [Platform::MacOs, Platform::Linux] {
            let spec = spec(platform, "work", "/bin/bmux");
            let path = spec.declaration_path.expect("test declaration path");
            assert!(path.starts_with(std::env::temp_dir()));
            assert!(path.to_string_lossy().contains("bmux-autostart-test-"));
        }
    }

    #[test]
    fn explicit_config_environment_is_embedded_as_cli_argument() {
        let _guard = ENV_LOCK.lock().expect("env lock");
        let original_config = std::env::var_os("BMUX_CONFIG");
        let original_dir = std::env::var_os("BMUX_CONFIG_DIR");
        // SAFETY: ENV_LOCK serializes environment mutation in this module.
        unsafe {
            std::env::remove_var("BMUX_CONFIG");
            std::env::set_var("BMUX_CONFIG_DIR", "/home/test/config");
        }
        let spec =
            service_spec_with_executable(Platform::MacOs, "work", PathBuf::from("/bin/bmux"))
                .expect("spec");
        assert_eq!(
            &spec.arguments[..4],
            [
                "--config",
                "/home/test/config/bmux.toml",
                "--runtime",
                "work"
            ]
        );
        // SAFETY: ENV_LOCK serializes environment mutation in this module.
        unsafe {
            match original_config {
                Some(value) => std::env::set_var("BMUX_CONFIG", value),
                None => std::env::remove_var("BMUX_CONFIG"),
            }
            match original_dir {
                Some(value) => std::env::set_var("BMUX_CONFIG_DIR", value),
                None => std::env::remove_var("BMUX_CONFIG_DIR"),
            }
        }
    }

    #[test]
    fn autostart_and_manual_daemon_arguments_remain_distinct_and_runtime_qualified() {
        let _guard = ENV_LOCK.lock().expect("env lock");
        let original_config = std::env::var_os("BMUX_CONFIG");
        let original_dir = std::env::var_os("BMUX_CONFIG_DIR");
        // SAFETY: ENV_LOCK serializes environment mutation in this module.
        unsafe {
            std::env::remove_var("BMUX_CONFIG");
            std::env::remove_var("BMUX_CONFIG_DIR");
        }
        let autostart =
            service_spec_with_executable(Platform::MacOs, "work", PathBuf::from("/bin/bmux"))
                .expect("autostart spec");
        assert_eq!(
            autostart.arguments,
            ["--runtime", "work", "server", "start"]
        );
        assert!(
            !autostart
                .arguments
                .iter()
                .any(|argument| argument == "--daemon")
        );
        assert!(
            !autostart
                .arguments
                .iter()
                .any(|argument| argument == "--foreground-internal")
        );
        // SAFETY: ENV_LOCK serializes environment mutation in this module.
        unsafe {
            match original_config {
                Some(value) => std::env::set_var("BMUX_CONFIG", value),
                None => std::env::remove_var("BMUX_CONFIG"),
            }
            match original_dir {
                Some(value) => std::env::set_var("BMUX_CONFIG_DIR", value),
                None => std::env::remove_var("BMUX_CONFIG_DIR"),
            }
        }
    }

    #[test]
    fn install_and_uninstall_plans_cover_ownership_and_manager_states() {
        assert_eq!(
            plan_install(Ownership::NotInstalled, ManagerState::NotRegistered).expect("create"),
            InstallPlan::Create
        );
        assert_eq!(
            plan_install(Ownership::BmuxManaged, ManagerState::Running).expect("update"),
            InstallPlan::Update
        );
        assert!(plan_install(Ownership::NotInstalled, ManagerState::Registered).is_err());
        assert!(plan_install(Ownership::ExternallyManaged, ManagerState::NotRegistered).is_err());
        assert!(plan_install(Ownership::Conflict, ManagerState::NotRegistered).is_err());
        assert!(plan_install(Ownership::NotInstalled, ManagerState::Unknown).is_err());

        assert_eq!(
            plan_uninstall(Ownership::NotInstalled, ManagerState::NotRegistered).expect("noop"),
            UninstallPlan::Noop
        );
        assert_eq!(
            plan_uninstall(Ownership::BmuxManaged, ManagerState::Running).expect("remove"),
            UninstallPlan::Remove
        );
        assert!(plan_uninstall(Ownership::NotInstalled, ManagerState::Running).is_err());
        assert!(plan_uninstall(Ownership::ExternallyManaged, ManagerState::Registered).is_err());
        assert!(plan_uninstall(Ownership::Conflict, ManagerState::NotRegistered).is_err());
        assert!(plan_uninstall(Ownership::BmuxManaged, ManagerState::Unknown).is_err());
    }

    #[test]
    fn status_serialization_and_human_output_are_stable() {
        let spec = spec(Platform::Linux, "work", "/missing/bmux");
        let inspection = DeclarationInspection {
            ownership: Ownership::NotInstalled,
            schema_version: None,
            executable: Some("/missing/bmux".to_string()),
        };
        let status = build_status(
            spec,
            &inspection,
            ManagerState::Registered,
            false,
            Some("externally registered".to_string()),
        );
        let json = serde_json::to_value(&status).expect("status json");
        assert_eq!(json["platform"], "linux");
        assert_eq!(json["runtime"], "work");
        assert_eq!(json["ownership"], "externally-managed");
        assert_eq!(json["manager_state"], "registered");
        assert_eq!(json["server_running"], false);
        assert_eq!(json["executable_exists"], false);
        let human = render_human_status(&status).expect("human status");
        assert!(human.contains("ownership: externally-managed\n"));
        assert!(human.contains("manager: registered\n"));
        assert!(human.contains("detail: externally registered\n"));
    }

    #[test]
    fn missing_installed_executable_is_reported_and_owned_declaration_can_be_updated() {
        let spec = spec(Platform::Linux, "work", "/missing/old-bmux");
        let path = spec.declaration_path.clone().expect("path");
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        std::fs::write(&path, render_systemd_unit(&spec)).expect("old declaration");
        let inspection = inspect_file_for_spec(&path, &spec).expect("inspection");
        let status = build_status(
            spec.clone(),
            &inspection,
            ManagerState::Registered,
            false,
            None,
        );
        assert!(!status.executable_exists);

        let mut repaired = spec;
        repaired.executable = std::env::current_exe().expect("test executable");
        let declaration = render_systemd_unit(&repaired);
        let runner = FakeRunner::with_results(vec![
            failure(3, "inactive"),
            success("enabled"),
            success(""),
            success(""),
        ]);
        install_with_runner(&repaired, &declaration, true, &runner).expect("repair declaration");
        let contents = std::fs::read_to_string(&path).expect("updated declaration");
        assert!(contents.contains(&repaired.executable.to_string_lossy().to_string()));
        let _ = std::fs::remove_dir_all(path.parent().expect("parent"));
    }

    #[test]
    fn unsupported_platform_status_and_render_behavior_are_explicit() {
        let spec = spec(Platform::Unsupported, "default", "/bin/bmux");
        assert!(!spec.platform.supported());
        assert!(render_declaration(&spec).is_err());
        let inspection = DeclarationInspection {
            ownership: Ownership::NotInstalled,
            schema_version: None,
            executable: None,
        };
        let status = build_status(
            spec,
            &inspection,
            ManagerState::Unsupported,
            false,
            Some("unsupported".to_string()),
        );
        assert!(!status.supported);
        assert_eq!(status.manager_state, ManagerState::Unsupported);
    }

    #[test]
    fn runtime_service_identities_are_stable_and_reject_unsafe_names() {
        assert_eq!(
            spec(Platform::MacOs, "default", "/bin/bmux").service_id,
            "dev.bmux.server"
        );
        assert_eq!(
            spec(Platform::MacOs, "work", "/bin/bmux").service_id,
            "dev.bmux.server.work"
        );
        assert_eq!(
            spec(Platform::Linux, "default", "/bin/bmux").service_id,
            "bmux-server.service"
        );
        assert_eq!(
            spec(Platform::Linux, "work", "/bin/bmux").service_id,
            "bmux-server-work.service"
        );
        assert_eq!(
            spec(Platform::Windows, "work", r"C:\bmux.exe").service_id,
            "bmux-server-work"
        );
        for invalid in ["", ".", "..", ".hidden", "trailing.", "a/b", "a\\b", "a..b"] {
            assert!(
                validate_runtime_component(invalid).is_err(),
                "accepted {invalid:?}"
            );
        }
    }

    #[test]
    fn ownership_accepts_previous_schema_for_safe_upgrade_but_rejects_future_schema() {
        let spec = spec(Platform::Linux, "work", "/bin/bmux");
        let current = ownership_marker(&spec);
        assert!(ownership_marker_matches_spec(&current, &spec));
        let previous = current.replace("schema=1", "schema=0");
        assert!(ownership_marker_matches_spec(&previous, &spec));
        let future = current.replace("schema=1", "schema=2");
        assert!(!ownership_marker_matches_spec(&future, &spec));
        let other_runtime = current.replace("runtime=work", "runtime=other");
        assert!(!ownership_marker_matches_spec(&other_runtime, &spec));
    }

    #[test]
    fn executable_override_validation_precedes_process_fallback() {
        let missing = temp_path("missing-bmux");
        let error = resolve_executable(Some(&missing.to_string_lossy()))
            .expect_err("explicit missing executable must fail");
        assert!(error.to_string().contains("does not exist"));

        let executable = std::env::current_exe().expect("test executable");
        assert_eq!(
            resolve_executable(Some(&executable.to_string_lossy())).expect("explicit executable"),
            executable
        );
    }

    #[test]
    fn declaration_generation_matches_stable_golden_content() {
        let _guard = ENV_LOCK.lock().expect("env lock");
        let original_config = std::env::var_os("BMUX_CONFIG");
        let original_dir = std::env::var_os("BMUX_CONFIG_DIR");
        // SAFETY: ENV_LOCK serializes environment mutation in this module.
        unsafe {
            std::env::remove_var("BMUX_CONFIG");
            std::env::remove_var("BMUX_CONFIG_DIR");
        }
        let launchd = render_launchd_plist(&spec(Platform::MacOs, "work", "/opt/bmux"));
        assert_eq!(launchd, include_str!("testdata/autostart-launchd.plist"));
        let systemd = render_systemd_unit(&spec(Platform::Linux, "work", "/opt/bmux"));
        assert_eq!(systemd, include_str!("testdata/autostart-systemd.service"));
        let windows = render_windows_task_xml(&spec(Platform::Windows, "work", r"C:\bmux.exe"));
        assert_eq!(windows, include_str!("testdata/autostart-windows.xml"));
        // SAFETY: ENV_LOCK serializes environment mutation in this module.
        unsafe {
            match original_config {
                Some(value) => std::env::set_var("BMUX_CONFIG", value),
                None => std::env::remove_var("BMUX_CONFIG"),
            }
            match original_dir {
                Some(value) => std::env::set_var("BMUX_CONFIG_DIR", value),
                None => std::env::remove_var("BMUX_CONFIG_DIR"),
            }
        }
    }

    #[test]
    fn launchd_declaration_uses_foreground_arguments_and_failure_restart() {
        let spec = spec(Platform::MacOs, "work", "/Applications/Bmux & Tools/bmux");
        let plist = render_launchd_plist(&spec);
        assert!(plist.contains(OWNERSHIP_MARKER));
        assert!(plist.contains("<string>/Applications/Bmux &amp; Tools/bmux</string>"));
        assert!(plist.contains("<string>server</string>"));
        assert!(plist.contains("<string>start</string>"));
        assert!(!plist.contains("--daemon"));
        assert!(plist.contains("<key>Crashed</key>\n    <true/>"));
        assert!(plist.contains("<key>SuccessfulExit</key>\n    <false/>"));
        assert!(plist.contains("<key>RunAtLoad</key>\n  <true/>"));
    }

    #[test]
    fn systemd_declaration_quotes_paths_and_uses_failure_restart() {
        let spec = spec(Platform::Linux, "work", "/opt/Bmux Tools/bmux");
        let unit = render_systemd_unit(&spec);
        assert!(unit.contains(
            "ExecStart=\"/opt/Bmux Tools/bmux\" \"--runtime\" \"work\" \"server\" \"start\""
        ));
        assert!(unit.contains("Restart=on-failure"));
        assert!(unit.contains("WantedBy=default.target"));
        assert!(!unit.contains("--daemon"));
    }

    #[test]
    fn windows_declaration_escapes_paths_and_uses_logon_trigger() {
        let spec = spec(
            Platform::Windows,
            "work",
            r"C:\Program Files\Bmux & Co\bmux.exe",
        );
        let xml = render_windows_task_xml(&spec);
        assert!(xml.contains("<LogonTrigger>"));
        assert!(xml.contains(r"C:\Program Files\Bmux &amp; Co\bmux.exe"));
        assert!(xml.contains("<RunLevel>LeastPrivilege</RunLevel>"));
        assert!(xml.contains("<RestartOnFailure>"));
        assert!(!xml.contains("--daemon"));
    }

    #[test]
    fn mutable_path_checks_reject_nix_store_symlink_ancestors_and_read_only_dirs() {
        assert!(ensure_mutable_declaration_path(Path::new("/nix/store/example/service")).is_err());

        let root = temp_path("root");
        let real = root.join("real");
        std::fs::create_dir_all(&real).expect("real dir");
        #[cfg(unix)]
        {
            let link = root.join("link");
            std::os::unix::fs::symlink(&real, &link).expect("directory symlink");
            assert!(ensure_mutable_declaration_path(&link.join("service")).is_err());
        }

        let read_only = root.join("read-only");
        std::fs::create_dir_all(&read_only).expect("read-only dir");
        let mut permissions = std::fs::metadata(&read_only)
            .expect("metadata")
            .permissions();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            permissions.set_mode(0o500);
        }
        #[cfg(not(unix))]
        permissions.set_readonly(true);
        std::fs::set_permissions(&read_only, permissions).expect("set read-only");
        assert!(ensure_mutable_declaration_path(&read_only.join("service")).is_err());
        let mut permissions = std::fs::metadata(&read_only)
            .expect("metadata")
            .permissions();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            permissions.set_mode(0o700);
        }
        #[cfg(not(unix))]
        permissions.set_readonly(false);
        std::fs::set_permissions(&read_only, permissions).expect("restore writable");
        let _ = std::fs::remove_dir_all(root.parent().expect("parent"));
    }

    #[test]
    fn inspection_distinguishes_absent_owned_external_and_conflict() {
        let spec = spec(Platform::Linux, "default", "/bin/bmux");
        let path = temp_path(&spec.service_id);
        assert_eq!(
            inspect_file(&path).expect("absent").ownership,
            Ownership::NotInstalled
        );
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        std::fs::write(&path, render_systemd_unit(&spec)).expect("write owned");
        assert_eq!(
            inspect_file(&path).expect("owned").ownership,
            Ownership::BmuxManaged
        );
        std::fs::write(
            &path,
            "[Service]\nExecStart=\"/nix/store/example/bin/bmux\"\n",
        )
        .expect("write external");
        assert_eq!(
            inspect_file(&path).expect("external file").ownership,
            Ownership::ExternallyManaged
        );
        std::fs::write(&path, "foreign").expect("write foreign");
        assert_eq!(
            inspect_file(&path).expect("conflict").ownership,
            Ownership::Conflict
        );
        std::fs::remove_file(&path).expect("remove");
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink("/nix/store/example", &path).expect("symlink");
            assert_eq!(
                inspect_file(&path).expect("external").ownership,
                Ownership::ExternallyManaged
            );
        }
        let _ = std::fs::remove_dir_all(path.parent().expect("parent"));
    }

    #[test]
    fn atomic_write_refuses_unrecognized_existing_declaration() {
        let spec = spec(Platform::Linux, "default", "/bin/bmux");
        let path = temp_path(&spec.service_id);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        std::fs::write(&path, "foreign").expect("write foreign");
        assert!(atomic_write_owned(&path, b"replacement", &spec).is_err());
        assert_eq!(std::fs::read_to_string(&path).expect("read"), "foreign");
        let _ = std::fs::remove_dir_all(path.parent().expect("parent"));
    }

    #[test]
    fn failed_manager_install_rolls_back_new_declaration() {
        let spec = spec(Platform::MacOs, "default", "/bin/bmux");
        let path = spec.declaration_path.clone().expect("path");
        let absent = || CommandResult {
            success: false,
            code: Some(5),
            stdout: String::new(),
            stderr: "Could not find service".to_string(),
        };
        let runner = FakeRunner::with_results(vec![
            success("501"),
            absent(),
            success("501"),
            absent(),
            CommandResult {
                success: false,
                code: Some(1),
                stdout: String::new(),
                stderr: "bootstrap failed".to_string(),
            },
        ]);
        let declaration = render_launchd_plist(&spec);
        assert!(install_with_runner(&spec, &declaration, false, &runner).is_err());
        assert!(!path.exists());
        let _ = std::fs::remove_dir_all(path.parent().expect("parent"));
    }

    #[test]
    fn failed_update_restores_previous_declaration_and_registration() {
        let spec = spec(Platform::MacOs, "default", "/bin/bmux");
        let path = spec.declaration_path.clone().expect("path");
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        let previous = render_launchd_plist(&spec);
        std::fs::write(&path, &previous).expect("previous declaration");
        let target = "gui/501/dev.bmux.server";
        let runner = FakeRunner::with_results(vec![
            success("501"),
            success("pid = 1"),
            success("501"),
            success(""),
            failure(1, "bootstrap failed"),
            success("501"),
            failure(5, "Could not find service"),
            success(""),
            failure(5, "Could not find service"),
            success(""),
        ]);
        let mut changed_spec = spec;
        changed_spec.executable = PathBuf::from("/new/bmux");
        let current = render_launchd_plist(&changed_spec);
        let error = install_with_runner(&changed_spec, &current, false, &runner)
            .expect_err("update should fail");
        assert!(
            error.to_string().contains("declaration was rolled back"),
            "{error:#}"
        );
        assert_eq!(std::fs::read_to_string(&path).expect("restored"), previous);
        assert_eq!(
            runner.calls().last(),
            Some(&(
                OsString::from("launchctl"),
                os_args(&["bootstrap", "gui/501", &path.to_string_lossy(),]),
            ))
        );
        assert!(
            runner.calls().iter().any(|call| {
                call == &(OsString::from("launchctl"), os_args(&["bootout", target]))
            })
        );
        let _ = std::fs::remove_dir_all(path.parent().expect("parent"));
    }

    #[test]
    fn windows_command_line_quoting_handles_spaces_quotes_and_backslashes() {
        assert_eq!(windows_command_line_quote("plain"), "plain");
        assert_eq!(windows_command_line_quote("two words"), "\"two words\"");
        assert_eq!(windows_command_line_quote("a\"b"), "\"a\\\"b\"");
    }

    #[test]
    fn command_output_decodes_utf8_and_utf16() {
        assert_eq!(decode_command_output(b"active\n"), "active");
        let utf16 = "Running\r\n"
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect::<Vec<_>>();
        let mut bom = vec![0xff, 0xfe];
        bom.extend(utf16);
        assert_eq!(decode_command_output(&bom), "Running");
    }

    #[test]
    fn fake_runner_records_exact_argument_vectors_without_shell() {
        let runner = FakeRunner::with_results(vec![success("")]);
        run_checked(&runner, "systemctl", &["--user", "daemon-reload"], "reload").expect("run");
        let calls = runner.calls.lock().expect("calls lock");
        assert_eq!(calls[0].0, OsString::from("systemctl"));
        assert_eq!(calls[0].1, os_args(&["--user", "daemon-reload"]));
        drop(calls);
    }
}
