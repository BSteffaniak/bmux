//! Administrative commands operate in the invoking local process, not a peer runtime.
mod access;
mod args;
mod deploy;
mod health;
pub(super) use health::nested_probe;
mod targets;
mod trust;

use args::{AccessArgs, AccessCommand, RemoteArgs, RemoteCommand, RemoteTrustCommand, TrustArgs};
use bmux_plugin_sdk::{NativeCommandContext, NativeCommandInvocationSource, PluginCommandError};
use clap::Parser;

fn parse<P: Parser>(leaf: &str, arguments: &[String]) -> Result<P, PluginCommandError> {
    P::try_parse_from(
        ["bmux", leaf]
            .into_iter()
            .chain(arguments.iter().map(String::as_str)),
    )
    .map_err(|e| PluginCommandError::new(e.exit_code(), e.to_string()))
}

use bmux_connections_admin::{kiosk, remote};
use std::sync::Arc;
type ProbePort = Arc<dyn Fn(&str) -> anyhow::Result<Box<dyn remote::ProbeTransport>> + Send + Sync>;
type TrustPort =
    Arc<dyn Fn(&str, &bmux_config::ConnectionTargetConfig) -> anyhow::Result<()> + Send + Sync>;
type IrohParser = fn(&str) -> anyhow::Result<(String, Option<String>, bool)>;
type SessionsPort = Arc<dyn Fn(&str) -> anyhow::Result<Vec<String>> + Send + Sync>;
type KioskAttachPort = Arc<dyn Fn(kiosk::AuthorizedAttach) -> anyhow::Result<u8> + Send + Sync>;
type ProxyPort = Arc<dyn Fn(&[String]) -> anyhow::Result<Option<u8>> + Send + Sync>;
#[derive(Clone)]
pub struct AdminPorts {
    pub probe: ProbePort,
    pub trust: TrustPort,
    pub parse_iroh: IrohParser,
    pub sessions: SessionsPort,
    pub kiosk_attach: KioskAttachPort,
    pub kiosk_proxy: ProxyPort,
    pub target: Option<String>,
}
fn test(ports: &AdminPorts, target: &str) -> anyhow::Result<u8> {
    remote::run_remote_test(target, (ports.probe)(target)?.as_ref())
}
fn kiosk_command(
    ports: &AdminPorts,
    leaf: &str,
    args: &[String],
) -> Result<anyhow::Result<u8>, PluginCommandError> {
    use args::KioskCommand as K;
    let command = parse::<args::KioskArgs>(leaf, args)?.command;
    let proxy_arguments = ["kiosk".to_owned(), leaf.to_owned()]
        .into_iter()
        .chain(args.iter().cloned())
        .collect::<Vec<_>>();
    Ok((|| -> anyhow::Result<u8> {
        if let Some(status) = (ports.kiosk_proxy)(&proxy_arguments)? {
            return Ok(status);
        }

        match command {
            K::Status { json } => kiosk::run_kiosk_status(json),
            K::Init {
                profile,
                all_profiles,
                dry_run,
                yes,
            } => kiosk::run_kiosk_init(&profile, all_profiles, dry_run, yes),
            K::IssueToken {
                profile,
                session,
                ttl_secs,
                one_shot,
                multi_use,
            } => kiosk::run_kiosk_issue_token(
                &profile,
                session.as_deref(),
                ttl_secs,
                one_shot,
                multi_use,
            ),
            K::RevokeToken { token_id } => kiosk::run_kiosk_revoke_token(&token_id),
            K::SshPrintConfig {
                profile,
                all_profiles,
            } => kiosk::run_kiosk_ssh_print_config(&profile, all_profiles),
            K::Attach { profile, token } => (ports.kiosk_attach)(kiosk::authorize_attach(
                &profile,
                &token,
                ports.target.as_deref(),
            )?),
        }
    })())
}
pub(super) fn run(
    context: &NativeCommandContext,
    ports: &AdminPorts,
) -> Result<i32, PluginCommandError> {
    if context.invocation_source != NativeCommandInvocationSource::LocalProcess
        || context.caller_client_id.is_some()
    {
        return Err(PluginCommandError::failed(
            "administration requires a standalone local process",
        ));
    }
    let name = context.command.as_str();
    let args = &context.arguments;
    let code = if let Some(leaf) = name.strip_prefix("kiosk-") {
        kiosk_command(ports, leaf, args)?
    } else if let Some(leaf) = name.strip_prefix("access-") {
        match parse::<AccessArgs>(leaf, args)?.command {
            AccessCommand::Status => access::run_access_status(),
            AccessCommand::List { json } => access::run_access_list(json),
            AccessCommand::Remove { fingerprint, yes } => {
                access::run_access_remove(&fingerprint, yes)
            }
            AccessCommand::Enable => access::run_access_enable(),
            AccessCommand::Disable => access::run_access_disable(),
            AccessCommand::Init {
                agent,
                key_file,
                public_key,
                github_user,
                yes,
            } => access::run_access_init(agent, &key_file, &public_key, &github_user, yes),
            AccessCommand::Add {
                agent,
                key_file,
                public_key,
                github_user,
            } => access::run_access_add(agent, &key_file, &public_key, &github_user),
        }
    } else if let Some(leaf) = name.strip_prefix("remote-trust-") {
        match parse::<TrustArgs>(leaf, args)?.command {
            RemoteTrustCommand::List => trust::run_remote_trust_list(),
            RemoteTrustCommand::Add { target, yes } => {
                crate::block_on_connection(trust::run_remote_trust_add(&target, yes))
            }
            RemoteTrustCommand::Remove { endpoint } => trust::run_remote_trust_remove(&endpoint),
        }
    } else {
        let leaf = match name {
            "remote-test" => "test",
            "remote-doctor" => "doctor",
            "remote-init" => "init",
            "remote-complete-sessions" => "complete-sessions",
            "remote-install-server" => "install-server",
            "remote-upgrade" => "upgrade",
            "remote-targets" => "targets",
            "remote-complete-targets" => "complete-targets",
            _ => return Err(PluginCommandError::failed("unknown administrative command")),
        };
        match parse::<RemoteArgs>(leaf, args)?.command {
            RemoteCommand::Test { target } => test(ports, &target),
            RemoteCommand::Doctor { target, fix } => (ports.probe)(&target)
                .and_then(|probe| remote::run_remote_doctor(&target, probe.as_ref(), fix)),
            RemoteCommand::Init {
                name,
                ssh,
                tls,
                iroh,
                user,
                port,
                set_default,
            } => remote::run_remote_init(
                remote::RemoteInitOptions {
                    name: &name,
                    ssh: ssh.as_deref(),
                    tls: tls.as_deref(),
                    iroh: iroh.as_deref(),
                    user: user.as_deref(),
                    port,
                    set_default,
                },
                ports.parse_iroh,
                |n, t| (ports.trust)(n, t),
                |t| test(ports, t),
            ),
            RemoteCommand::CompleteSessions { target } => (ports.sessions)(&target)
                .and_then(|names| remote::complete_sessions(&target, names)),
            RemoteCommand::Targets { json } => remote::run_remote_list(json),
            RemoteCommand::CompleteTargets => targets::run_remote_complete_targets(),
            RemoteCommand::Upgrade { target } => {
                crate::block_on_connection(deploy::run_remote_upgrade(target.as_deref()))
            }
            RemoteCommand::InstallServer { target } => {
                crate::block_on_connection(deploy::run_remote_install_server(&target))
            }
        }
    };
    code.map(i32::from)
        .map_err(|e| PluginCommandError::failed(format!("{e:#}")))
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn access_accepts_repeated_values() {
        let input = [
            "--key-file=a",
            "--key-file",
            "b",
            "--public-key=x",
            "--public-key=y",
        ]
        .map(String::from);
        let AccessCommand::Add {
            key_file,
            public_key,
            ..
        } = parse::<AccessArgs>("add", &input).unwrap().command
        else {
            panic!()
        };
        assert_eq!(key_file, ["a", "b"]);
        assert_eq!(public_key, ["x", "y"]);
    }
}
