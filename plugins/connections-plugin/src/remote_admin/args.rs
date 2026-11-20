use clap::{Parser, Subcommand};
#[derive(Parser)]
#[command(name = "bmux access")]
pub(super) struct AccessArgs {
    #[command(subcommand)]
    pub(super) command: AccessCommand,
}
#[derive(Clone, Debug, Subcommand)]
pub(super) enum AccessCommand {
    /// Show iroh SSH access status
    Status,
    /// Initialize iroh SSH access and enable it immediately
    Init {
        /// Add all currently loaded SSH agent keys
        #[arg(long)]
        agent: bool,
        /// Add a public key from file (repeatable)
        #[arg(long, value_name = "PATH")]
        key_file: Vec<String>,
        /// Add a public key line directly (repeatable)
        #[arg(long, value_name = "KEY")]
        public_key: Vec<String>,
        /// Import public keys from a GitHub username (repeatable)
        #[arg(long, value_name = "USER")]
        github_user: Vec<String>,
        /// Skip interactive confirmation prompts
        #[arg(long)]
        yes: bool,
    },
    /// Add SSH keys to iroh allowlist
    Add {
        /// Add all currently loaded SSH agent keys
        #[arg(long)]
        agent: bool,
        /// Add a public key from file (repeatable)
        #[arg(long, value_name = "PATH")]
        key_file: Vec<String>,
        /// Add a public key line directly (repeatable)
        #[arg(long, value_name = "KEY")]
        public_key: Vec<String>,
        /// Import public keys from a GitHub username (repeatable)
        #[arg(long, value_name = "USER")]
        github_user: Vec<String>,
    },
    /// List currently authorized SSH keys
    List {
        /// Print output as JSON
        #[arg(long)]
        json: bool,
    },
    /// Remove an SSH key from the allowlist
    Remove {
        /// Key fingerprint to remove
        fingerprint: String,
        /// Skip interactive confirmation prompts
        #[arg(long)]
        yes: bool,
    },
    /// Enable iroh SSH access enforcement
    Enable,
    /// Disable iroh SSH access enforcement
    Disable,
}

#[derive(Parser)]
#[command(name = "bmux remote trust")]
pub(super) struct TrustArgs {
    #[command(subcommand)]
    pub(super) command: RemoteTrustCommand,
}
#[derive(Clone, Debug, Subcommand)]
pub(super) enum RemoteTrustCommand {
    /// List effective TLS gateway trust pins
    List,
    /// Probe and trust a TLS gateway
    Add {
        /// TLS target URL or host[:port]
        target: String,
        /// Trust without an interactive confirmation prompt
        #[arg(long)]
        yes: bool,
    },
    /// Remove a local TLS gateway trust pin
    Remove {
        /// Endpoint key (`host:port`)
        endpoint: String,
    },
}

#[derive(Parser)]
#[command(name = "bmux remote")]
pub(super) struct RemoteArgs {
    #[command(subcommand)]
    pub(super) command: RemoteCommand,
}
#[derive(Subcommand)]
pub(super) enum RemoteCommand {
    Targets {
        #[arg(long)]
        json: bool,
    },
    /// Verify connectivity to a configured target
    Test {
        /// Target name or ssh destination
        target: String,
    },
    /// Diagnose remote connectivity and runtime readiness
    Doctor {
        /// Target name or ssh destination
        target: String,
        /// Apply safe automatic fixes when possible
        #[arg(long)]
        fix: bool,
    },
    /// Create and validate a named remote target profile
    Init {
        /// Name for the target profile
        name: String,
        /// Configure as an SSH target (user@host[:port] or host)
        #[arg(long, conflicts_with_all = ["tls", "iroh"])]
        ssh: Option<String>,
        /// Configure as a TLS target (host[:port])
        #[arg(long, conflicts_with_all = ["ssh", "iroh"])]
        tls: Option<String>,
        /// Configure as an iroh target (`endpoint_id[?relay=https://...][&auth=ssh]`)
        #[arg(long, conflicts_with_all = ["ssh", "tls"])]
        iroh: Option<String>,
        /// SSH username override
        #[arg(long)]
        user: Option<String>,
        /// SSH/TLS port override
        #[arg(long)]
        port: Option<u16>,
        /// Mark as default target
        #[arg(long)]
        set_default: bool,
    },
    /// Print session names for a target
    CompleteSessions {
        /// Target name
        target: String,
    },
    /// Ensure remote bmux runtime is installed and reachable
    InstallServer {
        /// Target name
        target: String,
    },
    /// Upgrade remote bmux runtime for one or all targets
    Upgrade {
        /// Target name (omit to upgrade all configured targets)
        target: Option<String>,
    },
    /// Print target names for completion
    CompleteTargets,
}

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
