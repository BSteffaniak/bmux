//! `bmux kiosk ...`: SSH kiosk profiles and tokens (local administration).
use anyhow::Result;
use bmux_connections_admin::kiosk;
use bmux_plugin_sdk::PluginCommandError;
use clap::{Parser, Subcommand};
use std::sync::Arc;

/// Attach with an authorized kiosk grant, provided by the CLI.
pub type KioskAttachPort = Arc<dyn Fn(kiosk::AuthorizedAttach) -> Result<u8> + Send + Sync>;

#[derive(Parser)]
#[command(name = "bmux kiosk")]
pub(super) struct KioskArgs {
    #[command(subcommand)]
    pub(super) command: KioskCommand,
}
#[derive(Clone, Debug, Subcommand)]
pub(super) enum KioskCommand {
    /// Show kiosk profile status and effective defaults
    Status {
        /// Print output as JSON
        #[arg(long)]
        json: bool,
    },
    /// Initialize/update kiosk bootstrap files from config
    Init {
        /// Kiosk profile to reconcile (repeatable)
        #[arg(long, value_name = "PROFILE", conflicts_with = "all_profiles")]
        profile: Vec<String>,
        /// Reconcile all configured profiles
        #[arg(long, conflicts_with = "profile")]
        all_profiles: bool,
        /// Preview actions without writing files
        #[arg(long)]
        dry_run: bool,
        /// Skip confirmation prompts
        #[arg(long)]
        yes: bool,
    },
    /// Issue a kiosk token scoped to a profile
    IssueToken {
        /// Kiosk profile name
        profile: String,
        /// Optional session override stored in token
        #[arg(long)]
        session: Option<String>,
        /// Optional token TTL in seconds (defaults to profile/default TTL)
        #[arg(long)]
        ttl_secs: Option<u64>,
        /// Mark token as one-shot (defaults to profile/default `one_shot`)
        #[arg(long, conflicts_with = "multi_use")]
        one_shot: bool,
        /// Mark token as reusable until expiry
        #[arg(long, conflicts_with = "one_shot")]
        multi_use: bool,
    },
    /// Revoke one issued kiosk token by id
    RevokeToken {
        /// Token id to revoke
        token_id: String,
    },
    /// Attach using a kiosk token (intended for forced SSH command)
    Attach {
        /// Kiosk profile name
        profile: String,
        /// Raw kiosk token
        #[arg(long)]
        token: String,
    },
    /// Print generated sshd include text without writing files
    SshPrintConfig {
        /// Kiosk profile to print (repeatable)
        #[arg(long, value_name = "PROFILE", conflicts_with = "all_profiles")]
        profile: Vec<String>,
        /// Print all configured profiles
        #[arg(long, conflicts_with = "profile")]
        all_profiles: bool,
    },
}

/// Run `kiosk-<leaf>`. Argument errors keep clap's exit code and text.
pub fn run(
    leaf: &str,
    arguments: &[String],
    attach: &KioskAttachPort,
    target: Option<&str>,
) -> Result<Result<u8>, PluginCommandError> {
    let command = KioskArgs::try_parse_from(
        ["bmux", leaf]
            .into_iter()
            .chain(arguments.iter().map(String::as_str)),
    )
    .map_err(|error| PluginCommandError::new(error.exit_code(), error.to_string()))?
    .command;
    Ok(match command {
        KioskCommand::Status { json } => kiosk::run_kiosk_status(json),
        KioskCommand::Init {
            profile,
            all_profiles,
            dry_run,
            yes,
        } => kiosk::run_kiosk_init(&profile, all_profiles, dry_run, yes),
        KioskCommand::IssueToken {
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
        KioskCommand::RevokeToken { token_id } => kiosk::run_kiosk_revoke_token(&token_id),
        KioskCommand::SshPrintConfig {
            profile,
            all_profiles,
        } => kiosk::run_kiosk_ssh_print_config(&profile, all_profiles),
        KioskCommand::Attach { profile, token } => {
            kiosk::authorize_attach(&profile, &token, target).and_then(|grant| attach(grant))
        }
    })
}
