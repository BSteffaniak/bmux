use super::{
    AttachExitReason, ConnectionContext, KernelClientFactory, SSH_RECONNECT_MAX_ATTEMPTS,
    connect_attach_target_with_kernel, connect_with_context, map_cli_client_error,
    reconnect_backoff_ms, run_session_attach_with_client,
};
use crate::connection::ConnectionPolicyScope;
use anyhow::{Context, Result};
use bmux_config::{BmuxConfig, ConfigPaths, KioskProfileConfig, KioskRole, KioskSandboxMode};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;
use uuid::Uuid;

#[derive(Debug, Clone, serde::Serialize)]
struct ResolvedKioskProfile {
    name: String,
    session: Option<String>,
    target: Option<String>,
    role: KioskRole,
    ssh_user: String,
    allow_detach: bool,
    token_ttl_secs: u64,
    one_shot: bool,
    sandbox: KioskSandboxMode,
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
struct KioskTokenStore {
    tokens: BTreeMap<String, KioskTokenRecord>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct KioskTokenRecord {
    token_id: String,
    profile: String,
    session: Option<String>,
    role: KioskRole,
    one_shot: bool,
    issued_at_unix: i64,
    expires_at_unix: i64,
    used_at_unix: Option<i64>,
    revoked_at_unix: Option<i64>,
    secret_sha256_hex: String,
}

pub(super) fn run_kiosk_status(as_json: bool) -> Result<u8> {
    let config = BmuxConfig::load()?;
    let profiles = resolve_profiles(&config);

    if as_json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "enabled": config.kiosk.defaults.enabled,
                "profiles": profiles,
            }))
            .context("failed serializing kiosk status json")?
        );
        return Ok(0);
    }

    println!(
        "kiosk: {}",
        if config.kiosk.defaults.enabled {
            "enabled"
        } else {
            "disabled"
        }
    );
    println!("profiles: {}", profiles.len());
    for profile in profiles.values() {
        println!(
            "- {} ssh_user={} role={:?} one_shot={} ttl={}s target={} session={}",
            profile.name,
            profile.ssh_user,
            profile.role,
            profile.one_shot,
            profile.token_ttl_secs,
            profile.target.as_deref().unwrap_or("local"),
            profile.session.as_deref().unwrap_or("-")
        );
    }
    Ok(0)
}

pub(super) fn run_kiosk_issue_token(
    profile: &str,
    session: Option<&str>,
    ttl_secs: Option<u64>,
    one_shot: bool,
    multi_use: bool,
) -> Result<u8> {
    let config = BmuxConfig::load()?;
    let profiles = resolve_profiles(&config);
    let resolved = profiles.get(profile).ok_or_else(|| {
        anyhow::anyhow!(
            "unknown kiosk profile '{profile}' (known: {})",
            profiles.keys().cloned().collect::<Vec<_>>().join(", ")
        )
    })?;

    let now = current_unix_timestamp();
    let effective_one_shot = if one_shot {
        true
    } else if multi_use {
        false
    } else {
        resolved.one_shot
    };
    let ttl = ttl_secs.unwrap_or(resolved.token_ttl_secs);
    let expires_at_unix = now.saturating_add(i64::try_from(ttl).unwrap_or(i64::MAX));

    let token_id = Uuid::new_v4().to_string();
    let token_secret = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
    let secret_sha256_hex = sha256_hex(token_secret.as_bytes());
    let token_value = format!("k1.{token_id}.{token_secret}");

    let mut store = load_token_store()?;
    store.tokens.insert(
        token_id.clone(),
        KioskTokenRecord {
            token_id: token_id.clone(),
            profile: profile.to_string(),
            session: session
                .map(ToOwned::to_owned)
                .or_else(|| resolved.session.clone()),
            role: resolved.role,
            one_shot: effective_one_shot,
            issued_at_unix: now,
            expires_at_unix,
            used_at_unix: None,
            revoked_at_unix: None,
            secret_sha256_hex,
        },
    );
    save_token_store(&store)?;

    println!("kiosk token issued");
    println!("profile: {profile}");
    println!("token_id: {token_id}");
    println!("expires_at_unix: {expires_at_unix}");
    println!("one_shot: {effective_one_shot}");
    println!();
    println!("token: {token_value}");
    println!();
    println!(
        "authorized_keys example:\nrestrict,command=\"bmux kiosk attach {profile} --token {token_value}\" <public-key>"
    );
    Ok(0)
}

pub(super) fn run_kiosk_revoke_token(token_id: &str) -> Result<u8> {
    let mut store = load_token_store()?;
    let Some(record) = store.tokens.get_mut(token_id) else {
        anyhow::bail!("kiosk token id not found: {token_id}");
    };
    record.revoked_at_unix = Some(current_unix_timestamp());
    save_token_store(&store)?;
    println!("revoked kiosk token: {token_id}");
    Ok(0)
}

pub(super) async fn run_kiosk_attach(
    profile: &str,
    token: &str,
    connection_context: ConnectionContext<'_>,
) -> Result<u8> {
    let config = BmuxConfig::load()?;
    let profiles = resolve_profiles(&config);
    let resolved = profiles.get(profile).ok_or_else(|| {
        anyhow::anyhow!(
            "unknown kiosk profile '{profile}' (known: {})",
            profiles.keys().cloned().collect::<Vec<_>>().join(", ")
        )
    })?;

    let (token_id, token_secret) = parse_raw_token(token)?;
    let mut store = load_token_store()?;
    let record = store
        .tokens
        .get_mut(token_id)
        .ok_or_else(|| anyhow::anyhow!("unknown kiosk token id: {token_id}"))?;

    if record.profile != profile {
        anyhow::bail!("token profile mismatch");
    }
    if record.revoked_at_unix.is_some() {
        anyhow::bail!("kiosk token is revoked");
    }
    if record.expires_at_unix < current_unix_timestamp() {
        anyhow::bail!("kiosk token is expired");
    }
    if record.one_shot && record.used_at_unix.is_some() {
        anyhow::bail!("kiosk token already used");
    }
    let provided_hash = sha256_hex(token_secret.as_bytes());
    if provided_hash != record.secret_sha256_hex {
        anyhow::bail!("invalid kiosk token secret");
    }

    record.used_at_unix = Some(current_unix_timestamp());
    let session = record.session.clone();
    save_token_store(&store)?;

    let effective_target =
        resolve_kiosk_attach_target(profile, resolved.target.as_deref(), connection_context)?;
    let (client, kernel_client_factory) = if let Some(target) = effective_target.as_deref() {
        connect_attach_target_with_kernel(target, "bmux-cli-kiosk-attach").await?
    } else {
        let client = connect_with_context(
            ConnectionPolicyScope::Normal,
            "bmux-cli-kiosk-attach",
            connection_context,
        )
        .await?;
        (client, None)
    };
    run_kiosk_attach_with_reconnect(
        client,
        kernel_client_factory,
        effective_target.as_deref(),
        resolved.allow_detach,
        session.as_deref(),
    )
    .await
}

fn resolve_kiosk_attach_target(
    profile: &str,
    profile_target: Option<&str>,
    connection_context: ConnectionContext<'_>,
) -> Result<Option<String>> {
    let target_override = connection_context
        .target_override
        .map(str::trim)
        .filter(|value| !value.is_empty());
    if let Some(pinned_target) = profile_target {
        if let Some(override_target) = target_override
            && override_target != pinned_target
        {
            anyhow::bail!(
                "kiosk profile '{profile}' pins target '{pinned_target}' and cannot be overridden by --target '{override_target}'"
            );
        }
        return Ok(Some(pinned_target.to_string()));
    }
    Ok(target_override.map(ToString::to_string))
}

async fn run_kiosk_attach_with_reconnect(
    mut client: bmux_client::BmuxClient,
    mut kernel_client_factory: Option<KernelClientFactory>,
    reconnect_target: Option<&str>,
    allow_detach: bool,
    session: Option<&str>,
) -> Result<u8> {
    let mut attempt = 0usize;
    loop {
        client
            .set_attach_policy(allow_detach)
            .await
            .map_err(map_cli_client_error)?;
        let outcome =
            run_session_attach_with_client(client, session, None, false, kernel_client_factory)
                .await?;
        if outcome.exit_reason != AttachExitReason::StreamClosed {
            return Ok(outcome.status_code);
        }

        let Some(target) = reconnect_target else {
            return Ok(outcome.status_code);
        };
        if attempt >= SSH_RECONNECT_MAX_ATTEMPTS {
            println!(
                "kiosk remote connection closed; giving up after {SSH_RECONNECT_MAX_ATTEMPTS} reconnect attempts"
            );
            return Ok(1);
        }

        attempt = attempt.saturating_add(1);
        let backoff = Duration::from_millis(reconnect_backoff_ms(attempt));
        println!(
            "kiosk remote connection closed; reconnecting to '{target}' (attempt {attempt}/{}) in {}ms...",
            SSH_RECONNECT_MAX_ATTEMPTS,
            backoff.as_millis()
        );
        tokio::time::sleep(backoff).await;

        let (new_client, new_kernel_factory) =
            connect_attach_target_with_kernel(target, "bmux-cli-kiosk-attach-reconnect").await?;
        client = new_client;
        kernel_client_factory = new_kernel_factory;
    }
}

pub(super) fn run_kiosk_init(
    profiles: &[String],
    all_profiles: bool,
    dry_run: bool,
    yes: bool,
) -> Result<u8> {
    let config = BmuxConfig::load()?;
    let resolved_profiles = resolve_profiles(&config);
    let selected_names = select_profiles(&resolved_profiles, profiles, all_profiles)?;
    let selected = selected_names
        .iter()
        .filter_map(|name| resolved_profiles.get(name).cloned())
        .collect::<Vec<_>>();

    let paths = ConfigPaths::default();
    let wrapper_dir = config
        .kiosk
        .files
        .wrapper_dir
        .unwrap_or_else(|| paths.config_dir.join("kiosk/wrappers"));
    let sshd_include_path = config
        .kiosk
        .files
        .sshd_include_path
        .unwrap_or_else(|| paths.config_dir.join("kiosk/sshd_config.generated"));

    let mut write_plan = Vec::new();
    let include = render_sshd_include(&selected, &wrapper_dir);
    write_plan.push((sshd_include_path, include));
    for profile in &selected {
        let script_path = wrapper_dir.join(format!("{}.sh", profile.name));
        write_plan.push((script_path, render_wrapper_script(profile)));
    }

    println!("kiosk init plan:");
    for (path, _) in &write_plan {
        println!("- {}", path.display());
    }

    if dry_run {
        println!("dry-run: no files were written");
        return Ok(0);
    }

    if !yes && !confirm_apply()? {
        println!("kiosk init cancelled");
        return Ok(1);
    }

    for (path, content) in write_plan {
        write_text_file(&path, &content)?;
        if is_shell_script_path(&path) {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mut perms = std::fs::metadata(&path)?.permissions();
                perms.set_mode(0o750);
                std::fs::set_permissions(&path, perms).with_context(|| {
                    format!("failed setting executable mode on {}", path.display())
                })?;
            }
        }
    }

    println!("kiosk init complete");
    Ok(0)
}

pub(super) fn run_kiosk_ssh_print_config(profiles: &[String], all_profiles: bool) -> Result<u8> {
    let config = BmuxConfig::load()?;
    let resolved_profiles = resolve_profiles(&config);
    let selected_names = select_profiles(&resolved_profiles, profiles, all_profiles)?;
    let selected = selected_names
        .iter()
        .filter_map(|name| resolved_profiles.get(name).cloned())
        .collect::<Vec<_>>();
    let paths = ConfigPaths::default();
    let wrapper_dir = config
        .kiosk
        .files
        .wrapper_dir
        .unwrap_or_else(|| paths.config_dir.join("kiosk/wrappers"));
    print!("{}", render_sshd_include(&selected, &wrapper_dir));
    Ok(0)
}

fn resolve_profiles(config: &BmuxConfig) -> BTreeMap<String, ResolvedKioskProfile> {
    let mut profiles = BTreeMap::new();
    if config.kiosk.profiles.is_empty() {
        profiles.insert(
            "default".to_string(),
            apply_profile_overrides(
                "default",
                &config.kiosk.defaults,
                &KioskProfileConfig::default(),
            ),
        );
        return profiles;
    }

    for (name, profile) in &config.kiosk.profiles {
        profiles.insert(
            name.clone(),
            apply_profile_overrides(name, &config.kiosk.defaults, profile),
        );
    }
    profiles
}

fn apply_profile_overrides(
    name: &str,
    defaults: &bmux_config::KioskDefaultsConfig,
    profile: &KioskProfileConfig,
) -> ResolvedKioskProfile {
    ResolvedKioskProfile {
        name: name.to_string(),
        session: profile.session.clone(),
        target: profile.target.clone(),
        role: profile.role.unwrap_or(defaults.role),
        ssh_user: profile
            .ssh_user
            .clone()
            .unwrap_or_else(|| defaults.ssh_user.clone()),
        allow_detach: profile.allow_detach.unwrap_or(defaults.allow_detach),
        token_ttl_secs: profile.token_ttl_secs.unwrap_or(defaults.token_ttl_secs),
        one_shot: profile.one_shot.unwrap_or(defaults.one_shot),
        sandbox: profile.sandbox.unwrap_or(defaults.sandbox),
    }
}

fn select_profiles(
    resolved: &BTreeMap<String, ResolvedKioskProfile>,
    profiles: &[String],
    all_profiles: bool,
) -> Result<Vec<String>> {
    if all_profiles || profiles.is_empty() {
        return Ok(resolved.keys().cloned().collect());
    }

    let available: BTreeSet<&str> = resolved.keys().map(String::as_str).collect();
    for profile in profiles {
        if !available.contains(profile.as_str()) {
            anyhow::bail!(
                "unknown kiosk profile '{profile}' (known: {})",
                resolved.keys().cloned().collect::<Vec<_>>().join(", ")
            );
        }
    }
    Ok(profiles.to_vec())
}

fn render_sshd_include(profiles: &[ResolvedKioskProfile], wrapper_dir: &Path) -> String {
    let mut by_user: BTreeMap<String, Vec<&ResolvedKioskProfile>> = BTreeMap::new();
    for profile in profiles {
        by_user
            .entry(profile.ssh_user.clone())
            .or_default()
            .push(profile);
    }

    let mut out = String::new();
    out.push_str("# Generated by bmux kiosk init\n");
    out.push_str("# Include this from sshd_config with: Include /path/to/this/file\n\n");
    for (ssh_user, user_profiles) in by_user {
        let primary = user_profiles
            .iter()
            .find(|profile| profile.name == "default")
            .copied()
            .unwrap_or(user_profiles[0]);
        let wrapper = wrapper_dir.join(format!("{}.sh", primary.name));
        let _ = writeln!(out, "Match User {ssh_user}");
        out.push_str("    PasswordAuthentication no\n");
        out.push_str("    KbdInteractiveAuthentication no\n");
        out.push_str("    PubkeyAuthentication yes\n");
        out.push_str("    AuthenticationMethods publickey\n");
        out.push_str("    PermitTTY yes\n");
        out.push_str("    PermitUserRC no\n");
        out.push_str("    X11Forwarding no\n");
        out.push_str("    AllowTcpForwarding no\n");
        out.push_str("    AllowAgentForwarding no\n");
        out.push_str("    PermitTunnel no\n");
        out.push_str("    GatewayPorts no\n");
        let _ = writeln!(out, "    ForceCommand {}", wrapper.display());
        if user_profiles.len() > 1 {
            let profile_names = user_profiles
                .iter()
                .map(|profile| profile.name.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            let _ = writeln!(
                out,
                "    # multiple profiles share this user: {profile_names} (ForceCommand uses '{}')",
                primary.name
            );
        }
        out.push('\n');
    }
    out
}

fn render_wrapper_script(profile: &ResolvedKioskProfile) -> String {
    format!(
        "#!/bin/sh\nset -eu\nif [ -z \"${{BMUX_KIOSK_TOKEN:-}}\" ]; then\n  printf '%s\\n' \"BMUX_KIOSK_TOKEN is required\" >&2\n  exit 64\nfi\nexec bmux kiosk attach {profile} --token \"$BMUX_KIOSK_TOKEN\"\n",
        profile = profile.name
    )
}

fn confirm_apply() -> Result<bool> {
    if !io::stdin().is_terminal() {
        anyhow::bail!("kiosk init requires --yes in non-interactive mode");
    }
    print!("Apply kiosk init changes? [y/N]: ");
    io::stdout()
        .flush()
        .context("failed flushing kiosk init prompt")?;
    let mut input = String::new();
    io::stdin()
        .read_line(&mut input)
        .context("failed reading kiosk init confirmation")?;
    let normalized = input.trim().to_ascii_lowercase();
    Ok(normalized == "y" || normalized == "yes")
}

fn write_text_file(path: &Path, content: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed creating parent directory {}", parent.display()))?;
    }
    std::fs::write(path, content).with_context(|| format!("failed writing {}", path.display()))
}

fn is_shell_script_path(path: &Path) -> bool {
    path.extension().is_some_and(|ext| ext == "sh")
}

fn parse_raw_token(token: &str) -> Result<(&str, &str)> {
    let mut parts = token.splitn(3, '.');
    let prefix = parts.next().unwrap_or_default();
    let token_id = parts.next().unwrap_or_default();
    let secret = parts.next().unwrap_or_default();
    if prefix != "k1" || token_id.is_empty() || secret.is_empty() {
        anyhow::bail!("invalid kiosk token format");
    }
    Ok((token_id, secret))
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let digest = hasher.finalize();
    let mut output = String::with_capacity(digest.len() * 2);
    for byte in digest {
        let _ = write!(output, "{byte:02x}");
    }
    output
}

fn load_token_store() -> Result<KioskTokenStore> {
    let path = token_store_path(&ConfigPaths::default());
    if !path.exists() {
        return Ok(KioskTokenStore::default());
    }
    let bytes =
        std::fs::read(&path).with_context(|| format!("failed reading {}", path.display()))?;
    serde_json::from_slice::<KioskTokenStore>(&bytes)
        .with_context(|| format!("failed parsing {}", path.display()))
}

fn save_token_store(store: &KioskTokenStore) -> Result<()> {
    let path = token_store_path(&ConfigPaths::default());
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed creating token store dir {}", parent.display()))?;
    }
    let encoded =
        serde_json::to_vec_pretty(store).context("failed serializing kiosk token store")?;
    std::fs::write(&path, encoded).with_context(|| format!("failed writing {}", path.display()))
}

fn token_store_path(paths: &ConfigPaths) -> PathBuf {
    paths.state_dir().join("runtime").join("kiosk-tokens.json")
}

fn current_unix_timestamp() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| {
            i64::try_from(duration.as_secs()).unwrap_or(i64::MAX)
        })
}

#[cfg(test)]
mod tests {
    use super::resolve_kiosk_attach_target;
    use crate::connection::ConnectionContext;

    #[test]
    fn kiosk_attach_target_uses_profile_pin_when_present() {
        let resolved =
            resolve_kiosk_attach_target("demo", Some("prod-ssh"), ConnectionContext::new(None))
                .expect("target resolution should succeed");
        assert_eq!(resolved.as_deref(), Some("prod-ssh"));
    }

    #[test]
    fn kiosk_attach_target_rejects_conflicting_cli_override() {
        let error = resolve_kiosk_attach_target(
            "demo",
            Some("prod-ssh"),
            ConnectionContext::new(Some("staging-ssh")),
        )
        .expect_err("conflicting override should fail");
        assert!(
            error
                .to_string()
                .contains("cannot be overridden by --target 'staging-ssh'")
        );
    }

    #[test]
    fn kiosk_attach_target_uses_cli_override_without_profile_pin() {
        let resolved = resolve_kiosk_attach_target(
            "demo",
            None,
            ConnectionContext::new(Some("tls://demo.example.com")),
        )
        .expect("target resolution should succeed");
        assert_eq!(resolved.as_deref(), Some("tls://demo.example.com"));
    }
}
