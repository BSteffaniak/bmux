use super::*;
use bmux_config::{ConnectionTargetConfig, ConnectionTransport};
use std::ffi::OsString;

#[derive(Debug, Clone)]
enum ResolvedTarget {
    Local,
    Ssh(SshTarget),
}

#[derive(Debug, Clone)]
struct SshTarget {
    label: String,
    host: String,
    user: Option<String>,
    port: Option<u16>,
    identity_file: Option<PathBuf>,
    known_hosts_file: Option<PathBuf>,
    strict_host_key_checking: bool,
    jump: Option<String>,
    remote_bmux_path: String,
    connect_timeout_ms: u64,
}

pub(super) fn should_proxy_to_target(cli: &Cli) -> Result<bool> {
    let Some(command) = cli.command.as_ref() else {
        return Ok(false);
    };
    if matches!(command, Command::Connect { .. } | Command::Remote { .. }) {
        return Ok(false);
    }
    let config = BmuxConfig::load()?;
    let target = resolve_effective_target(&config, cli.target.as_deref())?;
    Ok(matches!(target, ResolvedTarget::Ssh(_)))
}

pub(super) async fn run_target_proxy_from_current_argv(cli: &Cli) -> Result<u8> {
    let config = BmuxConfig::load()?;
    let target = resolve_effective_target(&config, cli.target.as_deref())?;
    let ResolvedTarget::Ssh(target) = target else {
        return Ok(1);
    };
    let argv = std::env::args_os().collect::<Vec<_>>();
    let remote_args = strip_target_argument(&argv);
    let needs_tty = command_needs_tty(cli.command.as_ref());
    run_ssh_bmux_command(&target, &remote_args, needs_tty)
}

pub(super) async fn run_connect(
    target: &str,
    session: Option<&str>,
    follow: Option<&str>,
    global: bool,
) -> Result<u8> {
    if session.is_some() && follow.is_some() {
        anyhow::bail!("--follow cannot be used with an explicit session argument");
    }

    let config = BmuxConfig::load()?;
    let resolved = resolve_target_reference(&config, target)?;
    match resolved {
        ResolvedTarget::Local => {
            let target_session = if let Some(session) = session {
                Some(session.to_string())
            } else {
                resolve_local_attach_session().await?
            };
            run_session_attach(target_session.as_deref(), follow, global).await
        }
        ResolvedTarget::Ssh(ssh_target) => {
            let target_session = if follow.is_some() {
                None
            } else if let Some(session) = session {
                Some(session.to_string())
            } else {
                resolve_remote_attach_session(&ssh_target)?
            };
            run_remote_attach(&ssh_target, target_session.as_deref(), follow, global)
        }
    }
}

pub(super) fn run_remote_list(as_json: bool) -> Result<u8> {
    let config = BmuxConfig::load()?;
    let mut entries = config
        .connections
        .targets
        .iter()
        .map(|(name, value)| {
            let transport = match value.transport {
                ConnectionTransport::Local => "local",
                ConnectionTransport::Ssh => "ssh",
            };
            serde_json::json!({
                "name": name,
                "transport": transport,
                "host": value.host,
                "user": value.user,
                "port": value.port,
                "default_session": value.default_session,
            })
        })
        .collect::<Vec<_>>();
    entries.sort_by(|left, right| {
        left["name"]
            .as_str()
            .unwrap_or_default()
            .cmp(right["name"].as_str().unwrap_or_default())
    });

    if as_json {
        println!(
            "{}",
            serde_json::to_string_pretty(&entries).context("failed encoding target list")?
        );
        return Ok(0);
    }

    if entries.is_empty() {
        println!("no configured targets");
        return Ok(0);
    }

    for entry in &entries {
        let name = entry["name"].as_str().unwrap_or("-");
        let transport = entry["transport"].as_str().unwrap_or("-");
        let host = entry["host"].as_str().unwrap_or("-");
        println!("{name}\t{transport}\t{host}");
    }
    Ok(0)
}

pub(super) async fn run_remote_test(target: &str) -> Result<u8> {
    let config = BmuxConfig::load()?;
    let resolved = resolve_target_reference(&config, target)?;
    match resolved {
        ResolvedTarget::Local => {
            let mut client = connect(ConnectionPolicyScope::Normal, "bmux-cli-remote-test").await?;
            client.ping().await.map_err(map_cli_client_error)?;
            println!("target '{target}' OK (local)");
            Ok(0)
        }
        ResolvedTarget::Ssh(ssh_target) => {
            run_ssh_bmux_command(
                &ssh_target,
                &[
                    OsString::from("server"),
                    OsString::from("status"),
                    OsString::from("--json"),
                ],
                false,
            )?;
            println!("target '{}' OK (ssh)", ssh_target.label);
            Ok(0)
        }
    }
}

pub(super) async fn run_remote_doctor(target: &str) -> Result<u8> {
    let config = BmuxConfig::load()?;
    let resolved = resolve_target_reference(&config, target)?;
    match resolved {
        ResolvedTarget::Local => {
            println!("target '{target}' transport: local");
            let mut client =
                connect(ConnectionPolicyScope::Normal, "bmux-cli-remote-doctor").await?;
            client.ping().await.map_err(map_cli_client_error)?;
            println!("local server reachable");
            Ok(0)
        }
        ResolvedTarget::Ssh(ssh_target) => {
            let version = ProcessCommand::new("ssh")
                .arg("-V")
                .output()
                .context("failed executing ssh -V")?;
            if !version.status.success() {
                anyhow::bail!("ssh binary unavailable");
            }
            let stderr = String::from_utf8_lossy(&version.stderr);
            if !stderr.trim().is_empty() {
                println!("{}", stderr.trim());
            }
            run_ssh_bmux_command(&ssh_target, &[OsString::from("--version")], false)?;
            run_ssh_bmux_command(
                &ssh_target,
                &[
                    OsString::from("server"),
                    OsString::from("status"),
                    OsString::from("--json"),
                ],
                false,
            )?;
            println!("target '{}' doctor: OK", ssh_target.label);
            Ok(0)
        }
    }
}

async fn resolve_local_attach_session() -> Result<Option<String>> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        anyhow::bail!(
            "session argument is required in non-interactive mode.\nList sessions: bmux list-sessions"
        );
    }
    let mut client = connect(
        ConnectionPolicyScope::Normal,
        "bmux-cli-connect-local-picker",
    )
    .await?;
    let sessions = client.list_sessions().await.map_err(map_cli_client_error)?;
    select_session_interactively("local", &sessions)
}

fn resolve_remote_attach_session(target: &SshTarget) -> Result<Option<String>> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        anyhow::bail!(
            "session argument is required in non-interactive mode.\nList sessions: bmux --target {} list-sessions",
            target.label
        );
    }
    let sessions = fetch_remote_sessions(target)?;
    select_session_interactively(&target.label, &sessions)
}

fn select_session_interactively(
    label: &str,
    sessions: &[SessionSummary],
) -> Result<Option<String>> {
    if sessions.is_empty() {
        anyhow::bail!(
            "No sessions found on target '{label}'.\nCreate one: bmux --target {label} new-session <name>"
        );
    }
    if sessions.len() == 1 {
        let selected = &sessions[0];
        let value = selected
            .name
            .clone()
            .unwrap_or_else(|| selected.id.to_string());
        println!("auto-selected session: {value}");
        return Ok(Some(value));
    }

    println!("Available sessions on '{label}':");
    for (index, session) in sessions.iter().enumerate() {
        let name = session
            .name
            .clone()
            .unwrap_or_else(|| session.id.to_string());
        println!("{}: {}", index + 1, name);
    }
    print!("Select session [1-{}]: ", sessions.len());
    io::stdout().flush().context("failed flushing prompt")?;
    let mut input = String::new();
    io::stdin()
        .read_line(&mut input)
        .context("failed reading session selection")?;
    let selection = input
        .trim()
        .parse::<usize>()
        .context("invalid session selection")?;
    if selection == 0 || selection > sessions.len() {
        anyhow::bail!("invalid session selection: {selection}");
    }
    let session = &sessions[selection - 1];
    Ok(Some(
        session
            .name
            .clone()
            .unwrap_or_else(|| session.id.to_string()),
    ))
}

fn run_remote_attach(
    target: &SshTarget,
    session: Option<&str>,
    follow: Option<&str>,
    global: bool,
) -> Result<u8> {
    let mut args = vec![OsString::from("attach")];
    if let Some(session) = session {
        args.push(OsString::from(session));
    }
    if let Some(follow) = follow {
        args.push(OsString::from("--follow"));
        args.push(OsString::from(follow));
    }
    if global {
        args.push(OsString::from("--global"));
    }
    run_ssh_bmux_command(target, &args, true)
}

fn fetch_remote_sessions(target: &SshTarget) -> Result<Vec<SessionSummary>> {
    let mut command = build_ssh_command(
        target,
        &[OsString::from("list-sessions"), OsString::from("--json")],
        false,
    );
    let output = command
        .output()
        .with_context(|| format!("failed executing ssh to {}", target.label))?;
    if !output.status.success() {
        anyhow::bail!(
            "failed listing remote sessions for '{}': {}",
            target.label,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    serde_json::from_slice::<Vec<SessionSummary>>(&output.stdout)
        .context("failed parsing remote list-sessions output")
}

fn run_ssh_bmux_command(target: &SshTarget, args: &[OsString], force_tty: bool) -> Result<u8> {
    let mut command = build_ssh_command(target, args, force_tty);
    let status = command
        .status()
        .with_context(|| format!("failed executing ssh target {}", target.label))?;
    Ok(exit_code_from_status(status))
}

fn build_ssh_command(target: &SshTarget, args: &[OsString], force_tty: bool) -> ProcessCommand {
    let mut command = ProcessCommand::new("ssh");
    if force_tty {
        command.arg("-t");
    }
    if let Some(port) = target.port {
        command.arg("-p");
        command.arg(port.to_string());
    }
    if let Some(path) = target.identity_file.as_ref() {
        command.arg("-i");
        command.arg(path);
    }
    if let Some(jump) = target.jump.as_ref() {
        command.arg("-J");
        command.arg(jump);
    }
    command.arg("-o");
    command.arg(format!(
        "StrictHostKeyChecking={}",
        if target.strict_host_key_checking {
            "yes"
        } else {
            "no"
        }
    ));
    if let Some(known_hosts) = target.known_hosts_file.as_ref() {
        command.arg("-o");
        command.arg(format!("UserKnownHostsFile={}", known_hosts.display()));
    }
    command.arg("-o");
    let timeout_secs = (target.connect_timeout_ms.saturating_add(999)) / 1000;
    command.arg(format!("ConnectTimeout={timeout_secs}"));
    let destination = target.user.as_ref().map_or_else(
        || target.host.clone(),
        |user| format!("{user}@{}", target.host),
    );
    command.arg(destination);
    command.arg(&target.remote_bmux_path);
    command.args(args);
    command
}

fn strip_target_argument(argv: &[OsString]) -> Vec<OsString> {
    if argv.len() <= 1 {
        return Vec::new();
    }
    let mut filtered = Vec::new();
    let mut index = 1;
    while index < argv.len() {
        let value = argv[index].to_string_lossy();
        if value == "--target" {
            index = index.saturating_add(2);
            continue;
        }
        if value.starts_with("--target=") {
            index = index.saturating_add(1);
            continue;
        }
        filtered.push(argv[index].clone());
        index = index.saturating_add(1);
    }
    filtered
}

fn command_needs_tty(command: Option<&Command>) -> bool {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return false;
    }
    matches!(
        command,
        Some(Command::Attach { .. })
            | Some(Command::Session {
                command: SessionCommand::Attach { .. }
            })
    )
}

fn exit_code_from_status(status: std::process::ExitStatus) -> u8 {
    status
        .code()
        .and_then(|code| u8::try_from(code).ok())
        .unwrap_or(1)
}

fn resolve_effective_target(
    config: &BmuxConfig,
    cli_target: Option<&str>,
) -> Result<ResolvedTarget> {
    if let Some(value) = cli_target {
        return resolve_target_reference(config, value);
    }
    if let Ok(value) = std::env::var("BMUX_TARGET")
        && !value.trim().is_empty()
    {
        return resolve_target_reference(config, value.trim());
    }
    if let Some(default) = config.connections.default_target.as_deref()
        && !default.trim().is_empty()
    {
        return resolve_target_reference(config, default.trim());
    }
    Ok(ResolvedTarget::Local)
}

fn resolve_target_reference(config: &BmuxConfig, target: &str) -> Result<ResolvedTarget> {
    if target.trim().is_empty() || target == "local" {
        return Ok(ResolvedTarget::Local);
    }
    if let Some(named) = config.connections.targets.get(target) {
        return resolve_named_target(target, named);
    }
    parse_inline_ssh_target(target)
}

fn resolve_named_target(name: &str, target: &ConnectionTargetConfig) -> Result<ResolvedTarget> {
    match target.transport {
        ConnectionTransport::Local => Ok(ResolvedTarget::Local),
        ConnectionTransport::Ssh => {
            let host = target
                .host
                .as_deref()
                .filter(|value| !value.trim().is_empty())
                .unwrap_or(name)
                .to_string();
            Ok(ResolvedTarget::Ssh(SshTarget {
                label: name.to_string(),
                host,
                user: target.user.clone(),
                port: target.port,
                identity_file: target.identity_file.clone(),
                known_hosts_file: target.known_hosts_file.clone(),
                strict_host_key_checking: target.strict_host_key_checking,
                jump: target.jump.clone(),
                remote_bmux_path: target.remote_bmux_path.clone(),
                connect_timeout_ms: target.connect_timeout_ms.max(1),
            }))
        }
    }
}

fn parse_inline_ssh_target(target: &str) -> Result<ResolvedTarget> {
    let mut raw = target.trim();
    if let Some(without_scheme) = raw.strip_prefix("ssh://") {
        raw = without_scheme;
    }
    let (user, host_port) = if let Some((user, rest)) = raw.split_once('@') {
        (Some(user.to_string()), rest)
    } else {
        (None, raw)
    };
    let (host, port) = if let Some((host, port_raw)) = host_port.rsplit_once(':') {
        if port_raw.is_empty() {
            (host_port.to_string(), None)
        } else {
            let parsed = port_raw
                .parse::<u16>()
                .with_context(|| format!("invalid SSH port in target '{target}'"))?;
            (host.to_string(), Some(parsed))
        }
    } else {
        (host_port.to_string(), None)
    };
    if host.trim().is_empty() {
        anyhow::bail!("target must include a host");
    }
    Ok(ResolvedTarget::Ssh(SshTarget {
        label: target.to_string(),
        host,
        user,
        port,
        identity_file: None,
        known_hosts_file: None,
        strict_host_key_checking: true,
        jump: None,
        remote_bmux_path: "bmux".to_string(),
        connect_timeout_ms: 8_000,
    }))
}
