use anyhow::{Context, Result};
use bmux_config::{BmuxConfig, IrohSshAuthorizedKey};
use git_sshripped_recipient::fetch_github_user_keys;
use git_sshripped_ssh_agent::{fingerprint_for_public_key_line, list_agent_ed25519_keys};
use git_sshripped_ssh_identity::default_public_key_candidates;
use std::collections::BTreeMap;
use std::io::{self, IsTerminal};

#[derive(Debug, Clone)]
struct KeyEntry {
    fingerprint: String,
    public_key: String,
    source: String,
}

pub(super) fn run_access_status() -> Result<u8> {
    let config = BmuxConfig::load()?;
    let access = &config.connections.iroh_ssh_access;
    let status = if access.enabled {
        "enabled"
    } else {
        "disabled"
    };
    println!("iroh ssh access: {status}");
    println!("authorized keys: {}", access.allowlist.len());
    if access.enabled && access.allowlist.is_empty() {
        println!("warning: access is enabled but allowlist is empty");
        println!("fix: bmux access add --agent (or add keys via --key-file/--github-user)");
    }
    Ok(0)
}

pub(super) fn run_access_list(as_json: bool) -> Result<u8> {
    let config = BmuxConfig::load()?;
    let access = &config.connections.iroh_ssh_access;
    let mut entries = access
        .allowlist
        .iter()
        .map(|(fingerprint, key)| {
            serde_json::json!({
                "fingerprint": fingerprint,
                "label": key.label,
                "added_at_unix": key.added_at_unix,
                "public_key": key.public_key,
            })
        })
        .collect::<Vec<_>>();
    entries.sort_by(|left, right| {
        left["fingerprint"]
            .as_str()
            .unwrap_or_default()
            .cmp(right["fingerprint"].as_str().unwrap_or_default())
    });

    if as_json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "enabled": access.enabled,
                "keys": entries,
            }))
            .context("failed serializing access list")?
        );
        return Ok(0);
    }

    println!(
        "iroh ssh access: {}",
        if access.enabled {
            "enabled"
        } else {
            "disabled"
        }
    );
    if entries.is_empty() {
        println!("no authorized SSH keys");
        return Ok(0);
    }

    for entry in entries {
        let fingerprint = entry["fingerprint"].as_str().unwrap_or("-");
        let label = entry["label"].as_str().unwrap_or("-");
        println!("- {fingerprint} ({label})");
    }
    Ok(0)
}

pub(super) fn run_access_enable() -> Result<u8> {
    let mut config = BmuxConfig::load()?;
    if config.connections.iroh_ssh_access.allowlist.is_empty() {
        anyhow::bail!(
            "cannot enable iroh SSH access without at least one key; run `bmux access add ...` first"
        );
    }
    config.connections.iroh_ssh_access.enabled = true;
    config.save()?;
    println!("iroh ssh access enabled");
    println!("Tip: add a backup key to avoid lockout (`bmux access add ...`).");
    Ok(0)
}

pub(super) fn run_access_disable() -> Result<u8> {
    let mut config = BmuxConfig::load()?;
    config.connections.iroh_ssh_access.enabled = false;
    config.save()?;
    println!("iroh ssh access disabled");
    Ok(0)
}

pub(super) fn run_access_remove(fingerprint: &str, yes: bool) -> Result<u8> {
    let mut config = BmuxConfig::load()?;
    let access = &mut config.connections.iroh_ssh_access;
    if !access.allowlist.contains_key(fingerprint) {
        anyhow::bail!("SSH key not found in allowlist: {fingerprint}");
    }

    let removing_last = access.enabled && access.allowlist.len() == 1;
    if removing_last && !yes {
        anyhow::bail!(
            "removing the last key while SSH access is enabled can lock you out; add a backup key or rerun with --yes"
        );
    }

    access.allowlist.remove(fingerprint);
    if access.enabled && access.allowlist.is_empty() {
        access.enabled = false;
        println!("removed last key; iroh SSH access was disabled to prevent lockout");
    } else {
        println!("removed SSH key: {fingerprint}");
    }
    config.save()?;
    Ok(0)
}

pub(super) fn run_access_add(
    agent: bool,
    key_files: &[String],
    public_keys: &[String],
    github_users: &[String],
) -> Result<u8> {
    let mut config = BmuxConfig::load()?;
    let mut entries = collect_key_entries(agent, key_files, public_keys, github_users)?;
    if entries.is_empty() {
        if io::stdin().is_terminal() {
            let candidates = discover_local_candidates()?;
            entries = prompt_select_candidates(&candidates)?;
        }
        if entries.is_empty() {
            anyhow::bail!(
                "no keys selected; provide --agent/--key-file/--public-key/--github-user or run in interactive mode"
            );
        }
    }

    let inserted =
        merge_allowlist_entries(&mut config.connections.iroh_ssh_access.allowlist, &entries);
    config.save()?;
    println!("added {inserted} SSH key(s) to iroh allowlist");
    Ok(0)
}

pub(super) fn run_access_init(
    agent: bool,
    key_files: &[String],
    public_keys: &[String],
    github_users: &[String],
    yes: bool,
) -> Result<u8> {
    let mut config = BmuxConfig::load()?;
    let mut entries = collect_key_entries(agent, key_files, public_keys, github_users)?;

    if entries.is_empty() {
        if !io::stdin().is_terminal() {
            anyhow::bail!(
                "access init needs key input in non-interactive mode; pass --agent/--key-file/--public-key/--github-user"
            );
        }
        let candidates = discover_local_candidates()?;
        entries = prompt_select_candidates(&candidates)?;
    }

    if entries.is_empty() {
        anyhow::bail!("no keys selected for iroh SSH access init");
    }

    if !yes && io::stdin().is_terminal() {
        print_init_confirmation(&entries)?;
    }

    let inserted =
        merge_allowlist_entries(&mut config.connections.iroh_ssh_access.allowlist, &entries);
    config.connections.iroh_ssh_access.enabled = true;
    config.save()?;

    println!("iroh SSH access initialized (enabled)");
    println!(
        "authorized keys now: {}",
        config.connections.iroh_ssh_access.allowlist.len()
    );
    println!("added this run: {inserted}");
    println!("Important: add a backup key to avoid lockout (`bmux access add ...`).");
    Ok(0)
}

fn print_init_confirmation(entries: &[KeyEntry]) -> Result<()> {
    println!("The following SSH keys will be authorized and SSH access will be enabled:");
    for entry in entries {
        println!("- {} ({})", entry.fingerprint, entry.source);
    }
    println!(
        "If none of these keys are available later, you may lose remote access until SSH access is disabled locally."
    );
    println!("Continue? [y/N]");

    let mut input = String::new();
    io::stdin()
        .read_line(&mut input)
        .context("failed reading confirmation input")?;
    let accepted = matches!(input.trim().to_ascii_lowercase().as_str(), "y" | "yes");
    if !accepted {
        anyhow::bail!("access init cancelled");
    }
    Ok(())
}

fn merge_allowlist_entries(
    allowlist: &mut BTreeMap<String, IrohSshAuthorizedKey>,
    entries: &[KeyEntry],
) -> usize {
    let mut inserted = 0usize;
    for entry in entries {
        let key = IrohSshAuthorizedKey {
            public_key: entry.public_key.clone(),
            label: Some(entry.source.clone()),
            added_at_unix: Some(current_unix_timestamp()),
        };
        if allowlist.insert(entry.fingerprint.clone(), key).is_none() {
            inserted = inserted.saturating_add(1);
        }
    }
    inserted
}

fn collect_key_entries(
    agent: bool,
    key_files: &[String],
    public_keys: &[String],
    github_users: &[String],
) -> Result<Vec<KeyEntry>> {
    let mut entries = Vec::new();

    if agent {
        for key in list_agent_ed25519_keys()? {
            let public_key = key
                .public_key
                .to_openssh()
                .context("failed encoding SSH agent key")?;
            entries.push(KeyEntry {
                fingerprint: key.fingerprint,
                public_key,
                source: "ssh-agent".to_string(),
            });
        }
    }

    for key_file in key_files {
        let public_key = read_public_key_file(key_file)?;
        let fingerprint = fingerprint_for_public_key_line(&public_key)
            .ok_or_else(|| anyhow::anyhow!("invalid public key in file: {key_file}"))?;
        entries.push(KeyEntry {
            fingerprint,
            public_key,
            source: key_file.clone(),
        });
    }

    for public_key in public_keys {
        let normalized = normalize_public_key_line(public_key)?;
        let fingerprint = fingerprint_for_public_key_line(&normalized)
            .ok_or_else(|| anyhow::anyhow!("invalid --public-key input"))?;
        entries.push(KeyEntry {
            fingerprint,
            public_key: normalized,
            source: "inline".to_string(),
        });
    }

    for github_user in github_users {
        let fetched = fetch_github_user_keys(github_user)
            .with_context(|| format!("failed fetching keys for GitHub user '{github_user}'"))?;
        for key in fetched.keys {
            let normalized = normalize_public_key_line(&key)
                .with_context(|| format!("invalid key returned for GitHub user '{github_user}'"))?;
            let fingerprint = fingerprint_for_public_key_line(&normalized).ok_or_else(|| {
                anyhow::anyhow!("invalid key returned for GitHub user '{github_user}'")
            })?;
            entries.push(KeyEntry {
                fingerprint,
                public_key: normalized,
                source: format!("github:{github_user}"),
            });
        }
    }

    Ok(deduplicate_key_entries(entries))
}

fn discover_local_candidates() -> Result<Vec<KeyEntry>> {
    let mut entries = Vec::new();

    for key in list_agent_ed25519_keys()? {
        entries.push(KeyEntry {
            fingerprint: key.fingerprint,
            public_key: key
                .public_key
                .to_openssh()
                .context("failed encoding SSH agent key")?,
            source: "ssh-agent".to_string(),
        });
    }

    for path in default_public_key_candidates() {
        if !path.exists() {
            continue;
        }
        let raw = std::fs::read_to_string(&path)
            .with_context(|| format!("failed reading {}", path.display()))?;
        let normalized = normalize_public_key_line(&raw)
            .with_context(|| format!("invalid public key in {}", path.display()))?;
        let Some(fingerprint) = fingerprint_for_public_key_line(&normalized) else {
            continue;
        };
        entries.push(KeyEntry {
            fingerprint,
            public_key: normalized,
            source: path.display().to_string(),
        });
    }

    let deduped = deduplicate_key_entries(entries);
    if deduped.is_empty() {
        anyhow::bail!(
            "no local SSH keys discovered; use --public-key, --key-file, or --github-user"
        );
    }
    Ok(deduped)
}

fn prompt_select_candidates(candidates: &[KeyEntry]) -> Result<Vec<KeyEntry>> {
    println!("Discovered SSH keys. Select which key(s) to authorize:");
    for (index, entry) in candidates.iter().enumerate() {
        println!("{}. {} ({})", index + 1, entry.fingerprint, entry.source);
    }
    println!("Enter comma-separated numbers (example: 1,3), 'all', or press Enter to cancel.");

    let mut input = String::new();
    io::stdin()
        .read_line(&mut input)
        .context("failed reading key selection")?;
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Ok(Vec::new());
    }
    if trimmed.eq_ignore_ascii_case("all") {
        return Ok(candidates.to_vec());
    }

    let mut selected = Vec::new();
    for token in trimmed
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        let index = token
            .parse::<usize>()
            .with_context(|| format!("invalid selection '{token}'"))?;
        if index == 0 || index > candidates.len() {
            anyhow::bail!("selection out of range: {token}");
        }
        selected.push(candidates[index - 1].clone());
    }

    Ok(deduplicate_key_entries(selected))
}

fn read_public_key_file(path: &str) -> Result<String> {
    let expanded = if let Some(rest) = path.strip_prefix("~/") {
        dirs::home_dir()
            .map(|home| home.join(rest))
            .ok_or_else(|| anyhow::anyhow!("failed resolving home directory for path: {path}"))?
    } else {
        std::path::PathBuf::from(path)
    };
    let raw = std::fs::read_to_string(&expanded)
        .with_context(|| format!("failed reading {}", expanded.display()))?;
    normalize_public_key_line(&raw)
}

fn normalize_public_key_line(raw: &str) -> Result<String> {
    let line = raw
        .lines()
        .find(|value| !value.trim().is_empty())
        .map(str::trim)
        .ok_or_else(|| anyhow::anyhow!("public key is empty"))?;
    if fingerprint_for_public_key_line(line).is_none() {
        anyhow::bail!("invalid OpenSSH public key line");
    }
    Ok(line.to_string())
}

fn deduplicate_key_entries(entries: Vec<KeyEntry>) -> Vec<KeyEntry> {
    let mut by_fingerprint = BTreeMap::new();
    for entry in entries {
        by_fingerprint
            .entry(entry.fingerprint.clone())
            .or_insert(entry);
    }
    by_fingerprint.into_values().collect()
}

fn current_unix_timestamp() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |value| i64::try_from(value.as_secs()).unwrap_or(0))
}
