use anyhow::{Context, Result};
use bmux_config::{
    BmuxConfig, ConfigPaths, ConnectionTargetConfig, ConnectionTransport, RemoteServerStartMode,
};
use bmux_remote_ssh::{
    SshTarget, build_ssh_command, parse_host_port_with_default, run_ssh_bmux_command,
    run_ssh_bmux_command_silent, ssh_destination,
};
use std::ffi::OsString;
#[derive(Debug)]
enum ResolvedTarget {
    Local,
    Ssh(SshTarget),
    Tls,
    Iroh,
}
async fn resolve_target_reference(config: &BmuxConfig, target: &str) -> Result<ResolvedTarget> {
    let target = crate::hosted::expand_share_link(config, &ConfigPaths::default(), target).await?;
    resolve_target_reference_inner(config, &target)
}
fn resolve_target_reference_inner(config: &BmuxConfig, target: &str) -> Result<ResolvedTarget> {
    if target.trim().is_empty() || target == "local" {
        return Ok(ResolvedTarget::Local);
    }
    if let Some(name) = target.trim().strip_prefix("bmux://") {
        let mapped = config.connections.share_links.get(name).ok_or_else(|| {
            anyhow::anyhow!("share link not found: bmux://{name}; run 'bmux share' or 'bmux hosts'")
        })?;
        return resolve_target_reference_inner(config, mapped);
    }
    if let Some(named) = config.connections.targets.get(target) {
        return resolve_named_target(target, named);
    }
    if target.trim().starts_with("https://") {
        return parse_https_target(target);
    }
    if target.trim().starts_with("iroh://") {
        validate_iroh_target(target)?;
        return Ok(ResolvedTarget::Iroh);
    }
    if target.trim().starts_with("tls://") {
        return parse_inline_tls_target(target);
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
                server_start_mode: target.server_start_mode,
            }))
        }
        ConnectionTransport::Tls => {
            anyhow::ensure!(
                target.host.as_deref().is_some_and(|v| !v.trim().is_empty()),
                "TLS target '{name}' requires host"
            );
            Ok(ResolvedTarget::Tls)
        }
        ConnectionTransport::Iroh => {
            anyhow::ensure!(
                target
                    .endpoint_id
                    .as_deref()
                    .or(target.host.as_deref())
                    .is_some_and(|v| !v.trim().is_empty()),
                "iroh target '{name}' requires endpoint_id"
            );
            Ok(ResolvedTarget::Iroh)
        }
    }
}
fn parse_inline_ssh_target(target: &str) -> Result<ResolvedTarget> {
    bmux_remote_ssh::parse_inline_ssh_target(target).map(ResolvedTarget::Ssh)
}
fn parse_inline_tls_target(target: &str) -> Result<ResolvedTarget> {
    super::trust::validate_tls_target(target)?;
    Ok(ResolvedTarget::Tls)
}
fn parse_https_target(target: &str) -> Result<ResolvedTarget> {
    let raw = target
        .trim()
        .strip_prefix("https://")
        .ok_or_else(|| anyhow::anyhow!("hosted target must start with https://"))?;
    let host = raw.split('/').next().unwrap_or_default();
    if host.trim().is_empty() {
        anyhow::bail!("hosted target must include a host");
    }
    parse_host_port_with_default(host, 443)?;
    Ok(ResolvedTarget::Tls)
}
pub(super) async fn run_remote_install_server(target: &str) -> Result<u8> {
    let config = BmuxConfig::load()?;
    let resolved = resolve_target_reference(&config, target).await?;
    match resolved {
        ResolvedTarget::Ssh(ssh_target) => {
            run_remote_install_server_for_target(&ssh_target)?;
            println!("remote install-server completed for '{}'", ssh_target.label);
            Ok(0)
        }
        ResolvedTarget::Tls => {
            anyhow::bail!(
                "install-server is only supported for SSH targets; install and run bmux gateway on the remote host"
            );
        }
        ResolvedTarget::Iroh => {
            anyhow::bail!(
                "install-server is not supported for iroh targets; run install on the host machine"
            );
        }
        ResolvedTarget::Local => {
            println!("local target does not require remote install");
            Ok(0)
        }
    }
}
pub(super) async fn run_remote_upgrade(target: Option<&str>) -> Result<u8> {
    let config = BmuxConfig::load()?;
    if let Some(target) = target {
        let resolved = resolve_target_reference(&config, target).await?;
        match resolved {
            ResolvedTarget::Ssh(ssh_target) => {
                run_remote_upgrade_for_target(&ssh_target)?;
                println!("remote upgrade completed for '{}'", ssh_target.label);
                return Ok(0);
            }
            ResolvedTarget::Tls | ResolvedTarget::Iroh => {
                anyhow::bail!("remote upgrade currently supports SSH targets only");
            }
            ResolvedTarget::Local => {
                println!("local target does not require remote upgrade");
                return Ok(0);
            }
        }
    }

    let mut upgraded = 0usize;
    for (name, target_config) in &config.connections.targets {
        if target_config.transport != ConnectionTransport::Ssh {
            continue;
        }
        let ResolvedTarget::Ssh(ssh_target) = resolve_named_target(name, target_config)? else {
            continue;
        };
        run_remote_upgrade_for_target(&ssh_target)?;
        upgraded = upgraded.saturating_add(1);
    }
    println!("remote upgrade completed for {upgraded} SSH target(s)");
    Ok(0)
}
fn run_remote_install_server_for_target(target: &SshTarget) -> Result<()> {
    let mut command = build_ssh_command(
        target,
        &[
            OsString::from("sh"),
            OsString::from("-lc"),
            OsString::from(
                "command -v bmux >/dev/null 2>&1 || cargo install --locked bmux_cli --bin bmux",
            ),
        ],
        false,
    );
    let status = command
        .status()
        .with_context(|| format!("failed running install command on '{}'", target.label))?;
    if !status.success() {
        anyhow::bail!(
            "remote install command failed on '{}'; ensure cargo is installed and reachable on the remote host",
            target.label
        );
    }
    ensure_remote_server_ready(target)
}
fn run_remote_upgrade_for_target(target: &SshTarget) -> Result<()> {
    let mut command = build_ssh_command(
        target,
        &[
            OsString::from("sh"),
            OsString::from("-lc"),
            OsString::from("cargo install --locked --force bmux_cli --bin bmux"),
        ],
        false,
    );
    let status = command
        .status()
        .with_context(|| format!("failed running upgrade command on '{}'", target.label))?;
    if !status.success() {
        anyhow::bail!(
            "remote upgrade command failed on '{}'; verify cargo/network access on remote host",
            target.label
        );
    }
    Ok(())
}
fn ensure_remote_server_ready(target: &SshTarget) -> Result<()> {
    let status = run_ssh_bmux_command_silent(
        target,
        &[OsString::from("server"), OsString::from("status")],
        false,
    )?;
    if status == 0 {
        return Ok(());
    }

    match target.server_start_mode {
        RemoteServerStartMode::RequireRunning => {
            anyhow::bail!(
                "remote bmux server is not running on '{}' and server_start_mode=require_running.\nstart it with: ssh {} {} server start --daemon",
                target.label,
                ssh_destination(target),
                target.remote_bmux_path
            );
        }
        RemoteServerStartMode::Auto => {
            tracing::debug!(target = %target.label, "remote server missing; attempting auto start");
            println!(
                "remote bmux server is not running on '{}'; starting it automatically...",
                target.label
            );
            let start_status = run_ssh_bmux_command(
                target,
                &[
                    OsString::from("server"),
                    OsString::from("start"),
                    OsString::from("--daemon"),
                ],
                false,
            )?;
            if start_status != 0 {
                anyhow::bail!(
                    "failed to auto-start remote bmux server on '{}'",
                    target.label
                );
            }
            let verify_status = run_ssh_bmux_command_silent(
                target,
                &[OsString::from("server"), OsString::from("status")],
                false,
            )?;
            if verify_status != 0 {
                anyhow::bail!(
                    "remote bmux server on '{}' did not become ready after auto-start",
                    target.label
                );
            }
            Ok(())
        }
    }
}
fn validate_iroh_target(input: &str) -> Result<()> {
    let trimmed = input.trim();
    let raw = trimmed.strip_prefix("iroh://").unwrap_or(trimmed).trim();
    let (endpoint_raw, query_raw) = match raw.split_once('?') {
        Some((endpoint, query)) => (endpoint.trim(), Some(query)),
        None => (raw, None),
    };
    if endpoint_raw.is_empty() {
        anyhow::bail!("iroh target must include an endpoint id");
    }

    if let Some(query) = query_raw {
        for pair in query.split('&') {
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            if key.trim().eq_ignore_ascii_case("compression") {
                let value = value.trim();
                anyhow::ensure!(
                    ["auto", "none", "zstd"]
                        .iter()
                        .any(|s| value.eq_ignore_ascii_case(s)),
                    "unsupported iroh compression mode '{value}' (expected auto|none|zstd)"
                );
            }
        }
    }
    Ok(())
}
