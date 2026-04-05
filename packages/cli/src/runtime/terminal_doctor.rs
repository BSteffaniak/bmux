use anyhow::{Context, Result};
use bmux_cli_schema::TraceFamily;
use bmux_config::{BmuxConfig, ConfigPaths, ResolvedTimeout, TerminfoAutoInstall};
use std::io::{self, IsTerminal};
use std::process::Command as ProcessCommand;

use super::attach::runtime::{describe_timeout, effective_attach_keybindings};
use super::{
    ProtocolDirection, ProtocolProfile, ProtocolTraceEvent, TerminalProfile,
    effective_enabled_plugins, primary_da_for_profile, protocol_profile_name,
    scan_available_plugins, secondary_da_for_profile, supported_query_names,
};

pub(super) fn run_terminal_install_terminfo(yes: bool, check_only: bool) -> Result<u8> {
    let configured = BmuxConfig::load().map_or_else(
        |_| "bmux-256color".to_string(),
        |cfg| cfg.behavior.pane_term,
    );
    let is_installed = check_terminfo_available("bmux-256color") == Some(true);

    if check_only {
        if is_installed {
            println!("bmux-256color terminfo is installed");
            return Ok(0);
        }
        println!("bmux-256color terminfo is not installed");
        return Ok(1);
    }

    if is_installed {
        println!("bmux-256color terminfo is already installed");
        return Ok(0);
    }

    if !yes && io::stdin().is_terminal() {
        println!("bmux-256color terminfo is missing.");
        println!("Install now? [Y/n]");
        let mut answer = String::new();
        io::stdin()
            .read_line(&mut answer)
            .context("failed reading install confirmation")?;
        let trimmed = answer.trim().to_ascii_lowercase();
        if trimmed == "n" || trimmed == "no" {
            println!("skipped terminfo installation");
            return Ok(0);
        }
    }

    install_bmux_terminfo()?;
    if check_terminfo_available("bmux-256color") == Some(true) {
        println!("installed terminfo entry: bmux-256color");
        if configured != "bmux-256color" {
            println!("note: current config pane_term is '{configured}'");
        }
        Ok(0)
    } else {
        anyhow::bail!("terminfo install completed but bmux-256color is still unavailable")
    }
}

pub(super) fn run_terminal_doctor(
    as_json: bool,
    include_trace: bool,
    trace_limit: usize,
    trace_family: Option<TraceFamily>,
    trace_pane: Option<u16>,
) -> Result<u8> {
    let config = match BmuxConfig::load() {
        Ok(config) => config,
        Err(error) => {
            println!(
                "bmux terminal doctor warning: failed to load config ({error}); using defaults"
            );
            BmuxConfig::default()
        }
    };

    let configured_term = config.behavior.pane_term.clone();
    let effective = resolve_pane_term(&configured_term);
    let protocol_profile = protocol_profile_for_terminal_profile(effective.profile);
    let last_declined_prompt_epoch_secs = last_prompt_decline_epoch_secs();
    let trace_data = if include_trace {
        load_protocol_trace(10_000)?
    } else {
        ProtocolTraceData::default()
    };
    let trace_events =
        filter_trace_events(&trace_data.events, trace_family, trace_pane, trace_limit);

    if as_json {
        let payload = serde_json::json!({
            "configured_pane_term": configured_term,
            "effective_pane_term": effective.pane_term,
            "terminal_profile": terminal_profile_name(effective.profile),
            "protocol_profile": protocol_profile_name(protocol_profile),
            "primary_da_reply": String::from_utf8_lossy(primary_da_for_profile(protocol_profile)),
            "secondary_da_reply": String::from_utf8_lossy(secondary_da_for_profile(protocol_profile)),
            "supported_queries": supported_query_names(),
            "fallback_chain": effective.fallback_chain,
            "terminfo_check": {
                "attempted": effective.terminfo_checked,
                "available": effective.terminfo_available,
            },
            "terminfo_checks": effective
                .terminfo_checks
                .iter()
                .map(|(term, available)| serde_json::json!({
                    "term": term,
                    "available": available,
                }))
                .collect::<Vec<_>>(),
            "warnings": effective.warnings,
            "terminfo_auto_install": {
                "policy": terminfo_auto_install_name(config.behavior.terminfo_auto_install),
                "prompt_cooldown_days": config.behavior.terminfo_prompt_cooldown_days,
                "last_declined_prompt_epoch_secs": last_declined_prompt_epoch_secs,
            },
            "trace": if include_trace {
                serde_json::json!({
                    "events": trace_events,
                    "limit": trace_limit,
                    "dropped": trace_data.dropped,
                    "applied_filters": {
                        "family": trace_family.map(trace_family_name),
                        "pane": trace_pane,
                    },
                })
            } else {
                serde_json::Value::Null
            },
        });
        println!(
            "{}",
            serde_json::to_string_pretty(&payload)
                .context("failed to encode terminal doctor json")?
        );
        return Ok(0);
    }

    println!("bmux terminal doctor");
    println!("configured pane TERM: {configured_term}");
    println!("effective pane TERM: {}", effective.pane_term);
    println!(
        "terminal profile: {}",
        terminal_profile_name(effective.profile)
    );
    println!(
        "protocol profile: {}",
        protocol_profile_name(protocol_profile)
    );
    println!(
        "primary DA reply: {}",
        String::from_utf8_lossy(primary_da_for_profile(protocol_profile))
    );
    println!(
        "secondary DA reply: {}",
        String::from_utf8_lossy(secondary_da_for_profile(protocol_profile))
    );
    println!(
        "terminfo auto-install policy: {} (cooldown {} days)",
        terminfo_auto_install_name(config.behavior.terminfo_auto_install),
        config.behavior.terminfo_prompt_cooldown_days
    );
    if let Some(epoch) = last_declined_prompt_epoch_secs {
        println!("last declined terminfo prompt (epoch secs): {epoch}");
    }
    println!("supported queries: {}", supported_query_names().join(", "));
    println!("fallback chain: {}", effective.fallback_chain.join(" -> "));
    if effective.terminfo_checked {
        println!(
            "terminfo available: {}",
            if effective.terminfo_available {
                "yes"
            } else {
                "no"
            }
        );
        for (term, available) in &effective.terminfo_checks {
            println!(
                "terminfo check {term}: {}",
                match available {
                    Some(true) => "yes",
                    Some(false) => "no",
                    None => "unknown",
                }
            );
        }
    }
    for warning in effective.warnings {
        println!("warning: {warning}");
    }

    if include_trace {
        println!("trace events (latest {trace_limit}):");
        println!("trace dropped events: {}", trace_data.dropped);
        if trace_family.is_some() || trace_pane.is_some() {
            println!(
                "trace filters: family={} pane={}",
                trace_family.map_or("any", trace_family_name),
                trace_pane.map_or_else(|| "any".to_string(), |pane| pane.to_string())
            );
        }
        if trace_events.is_empty() {
            if trace_data.events.is_empty() {
                println!(
                    "  (no events found; enable behavior.protocol_trace_enabled and run a session)"
                );
            } else {
                println!("  (no events matched active filters)");
            }
        }
        for event in trace_events {
            let pane = event
                .pane_id
                .map_or_else(|| "-".to_string(), |id| id.to_string());
            println!(
                "  [{}] pane={} {}:{} {} {}",
                event.timestamp_ms,
                pane,
                event.family,
                event.name,
                match event.direction {
                    ProtocolDirection::Query => "query",
                    ProtocolDirection::Reply => "reply",
                },
                event.decoded.replace('\u{1b}', "<ESC>")
            );
        }
    }

    Ok(0)
}

pub(super) fn plugin_keybinding_proposals(
    config: &BmuxConfig,
) -> (
    std::collections::BTreeMap<String, String>,
    std::collections::BTreeMap<String, String>,
    std::collections::BTreeMap<String, String>,
) {
    let paths = ConfigPaths::default();
    let registry = match scan_available_plugins(config, &paths) {
        Ok(registry) => registry,
        Err(error) => {
            eprintln!(
                "bmux warning: failed loading plugin keybinding proposals ({error}); continuing without plugin keybinding defaults"
            );
            return (
                std::collections::BTreeMap::new(),
                std::collections::BTreeMap::new(),
                std::collections::BTreeMap::new(),
            );
        }
    };
    let enabled_plugins = effective_enabled_plugins(config, &registry)
        .into_iter()
        .collect::<std::collections::BTreeSet<_>>();
    let mut runtime = std::collections::BTreeMap::new();
    let mut global = std::collections::BTreeMap::new();
    let mut scroll = std::collections::BTreeMap::new();

    for plugin in registry.iter() {
        if !enabled_plugins.contains(plugin.declaration.id.as_str()) {
            continue;
        }
        for (chord, action) in &plugin.manifest.keybindings.runtime {
            runtime
                .entry(chord.clone())
                .or_insert_with(|| action.clone());
        }
        for (chord, action) in &plugin.manifest.keybindings.global {
            global
                .entry(chord.clone())
                .or_insert_with(|| action.clone());
        }
        for (chord, action) in &plugin.manifest.keybindings.scroll {
            scroll
                .entry(chord.clone())
                .or_insert_with(|| action.clone());
        }
    }

    (runtime, global, scroll)
}

/// Canonicalize the chord keys in a keybinding map so that aliases resolve to
/// the same canonical form (e.g. `"shift+left"` and `"shift+arrow_left"` both
/// become `"shift+arrow_left"`).  When two raw keys collapse to the same
/// canonical key, the last inserted value wins (matching `BTreeMap::extend`
/// semantics).
fn canonicalize_keybindings(
    bindings: std::collections::BTreeMap<String, String>,
) -> std::collections::BTreeMap<String, String> {
    bindings
        .into_iter()
        .map(|(chord_str, action)| (crate::input::canonical_chord_key(&chord_str), action))
        .collect()
}

pub(super) fn merged_runtime_keybindings(
    config: &BmuxConfig,
) -> (
    std::collections::BTreeMap<String, String>,
    std::collections::BTreeMap<String, String>,
    std::collections::BTreeMap<String, String>,
) {
    let defaults = BmuxConfig::default();
    let (plugin_runtime, plugin_global, plugin_scroll) = plugin_keybinding_proposals(config);

    let mut runtime = canonicalize_keybindings(defaults.keybindings.runtime);
    runtime.extend(canonicalize_keybindings(plugin_runtime));
    runtime.extend(canonicalize_keybindings(config.keybindings.runtime.clone()));

    let mut global = canonicalize_keybindings(defaults.keybindings.global);
    global.extend(canonicalize_keybindings(plugin_global));
    global.extend(canonicalize_keybindings(config.keybindings.global.clone()));

    let mut scroll = canonicalize_keybindings(defaults.keybindings.scroll);
    scroll.extend(canonicalize_keybindings(plugin_scroll));
    scroll.extend(canonicalize_keybindings(config.keybindings.scroll.clone()));

    (runtime, global, scroll)
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub(super) struct ProtocolTraceFile {
    pub(super) dropped: usize,
    pub(super) events: Vec<ProtocolTraceEvent>,
}

#[derive(Debug, Default)]
pub(super) struct ProtocolTraceData {
    pub(super) dropped: usize,
    pub(super) events: Vec<ProtocolTraceEvent>,
}

pub(super) fn load_protocol_trace(limit: usize) -> Result<ProtocolTraceData> {
    let path = bmux_config::ConfigPaths::default().protocol_trace_file();
    if !path.exists() {
        return Ok(ProtocolTraceData::default());
    }
    let bytes = std::fs::read(&path)
        .with_context(|| format!("failed reading protocol trace file at {}", path.display()))?;
    let file: ProtocolTraceFile = serde_json::from_slice(&bytes)
        .with_context(|| format!("failed parsing protocol trace file at {}", path.display()))?;
    if limit == 0 || file.events.len() <= limit {
        return Ok(ProtocolTraceData {
            dropped: file.dropped,
            events: file.events,
        });
    }
    let start = file.events.len().saturating_sub(limit);
    Ok(ProtocolTraceData {
        dropped: file.dropped,
        events: file.events.into_iter().skip(start).collect(),
    })
}

pub(super) fn filter_trace_events(
    events: &[ProtocolTraceEvent],
    family: Option<TraceFamily>,
    pane: Option<u16>,
    limit: usize,
) -> Vec<ProtocolTraceEvent> {
    let mut filtered: Vec<ProtocolTraceEvent> = events
        .iter()
        .filter(|event| {
            let family_matches =
                family.is_none_or(|value| event.family == trace_family_name(value));
            let pane_matches = pane.is_none_or(|value| event.pane_id == Some(value));
            family_matches && pane_matches
        })
        .cloned()
        .collect();
    if limit > 0 && filtered.len() > limit {
        let start = filtered.len().saturating_sub(limit);
        filtered = filtered.split_off(start);
    }
    filtered
}

pub(super) const fn trace_family_name(family: TraceFamily) -> &'static str {
    match family {
        TraceFamily::Csi => "csi",
        TraceFamily::Osc => "osc",
        TraceFamily::Dcs => "dcs",
    }
}

#[derive(Debug, serde::Serialize, serde::Deserialize, Default)]
pub(super) struct TerminfoPromptStateFile {
    pub(super) last_declined_epoch_secs: Option<u64>,
}

pub(super) fn install_bmux_terminfo() -> Result<()> {
    let source = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../terminfo/bmux-256color.terminfo");
    if !source.exists() {
        anyhow::bail!("terminfo source file not found at {}", source.display());
    }

    let output = ProcessCommand::new("tic")
        .arg("-x")
        .arg(&source)
        .output()
        .context("failed to execute tic")?;
    if !output.status.success() {
        anyhow::bail!(
            "tic failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

pub(super) const fn terminfo_auto_install_name(policy: TerminfoAutoInstall) -> &'static str {
    match policy {
        TerminfoAutoInstall::Ask => "ask",
        TerminfoAutoInstall::Always => "always",
        TerminfoAutoInstall::Never => "never",
    }
}

pub(super) fn last_prompt_decline_epoch_secs() -> Option<u64> {
    let path = bmux_config::ConfigPaths::default().terminfo_prompt_state_file();
    let bytes = std::fs::read(path).ok()?;
    let state: TerminfoPromptStateFile = serde_json::from_slice(&bytes).ok()?;
    state.last_declined_epoch_secs
}

pub(super) struct PaneTermResolution {
    pub(super) pane_term: String,
    pub(super) profile: TerminalProfile,
    pub(super) warnings: Vec<String>,
    pub(super) terminfo_checked: bool,
    pub(super) terminfo_available: bool,
    pub(super) fallback_chain: Vec<String>,
    pub(super) terminfo_checks: Vec<(String, Option<bool>)>,
}

pub(super) fn resolve_pane_term(configured: &str) -> PaneTermResolution {
    resolve_pane_term_with_checker(configured, check_terminfo_available)
}

pub(super) fn resolve_pane_term_with_checker<F>(
    configured: &str,
    mut checker: F,
) -> PaneTermResolution
where
    F: FnMut(&str) -> Option<bool>,
{
    let configured_trimmed = configured.trim();
    let configured_normalized = if configured_trimmed.is_empty() {
        "bmux-256color".to_string()
    } else {
        configured_trimmed.to_string()
    };

    let mut warnings = Vec::new();
    if configured_trimmed.is_empty() {
        warnings.push("behavior.pane_term is empty; falling back to bmux-256color".to_string());
    }

    let fallback_chain = vec!["xterm-256color".to_string(), "screen-256color".to_string()];
    let mut terminfo_checks = Vec::new();
    let mut pane_term = configured_normalized;

    let configured_check = checker(&pane_term);
    terminfo_checks.push((pane_term.clone(), configured_check));

    if configured_check == Some(false) {
        let mut selected_fallback = None;
        for candidate in &fallback_chain {
            if candidate == &pane_term {
                continue;
            }
            let check = checker(candidate);
            terminfo_checks.push((candidate.clone(), check));
            if check == Some(true) {
                selected_fallback = Some(candidate.clone());
                break;
            }
        }

        if let Some(fallback) = selected_fallback {
            warnings.push(format!(
                "pane TERM '{}' not installed; using '{}' (fallback chain: {})",
                pane_term,
                fallback,
                fallback_chain.join(", ")
            ));
            if pane_term == "bmux-256color" {
                warnings.push(
                    "install bmux terminfo with scripts/install-terminfo.sh to use bmux-256color"
                        .to_string(),
                );
            }
            pane_term = fallback;
        } else {
            warnings.push(format!(
                "pane TERM '{}' not installed and no fallback available (checked: {})",
                pane_term,
                fallback_chain.join(", ")
            ));
        }
    } else if configured_check.is_none() {
        warnings.push(format!(
            "could not verify terminfo for pane TERM '{pane_term}'; continuing without fallback checks"
        ));
    }

    let profile = profile_for_term(&pane_term);

    let effective_terminfo_available = terminfo_checks
        .iter()
        .find_map(|(term, available)| (term == &pane_term).then_some(*available))
        .flatten();

    if profile == TerminalProfile::Conservative {
        warnings.push(format!(
            "pane TERM '{pane_term}' uses conservative capability profile; compatibility depends on host terminfo"
        ));
    }

    PaneTermResolution {
        pane_term,
        profile,
        warnings,
        terminfo_checked: terminfo_checks
            .iter()
            .any(|(_, available)| available.is_some()),
        terminfo_available: effective_terminfo_available.unwrap_or(false),
        fallback_chain,
        terminfo_checks,
    }
}

pub(super) fn profile_for_term(term: &str) -> TerminalProfile {
    match term {
        "bmux-256color" => TerminalProfile::Bmux256Color,
        "screen-256color" | "tmux-256color" => TerminalProfile::Screen256Color,
        "xterm-256color" => TerminalProfile::Xterm256Color,
        _ => TerminalProfile::Conservative,
    }
}

pub(super) const fn terminal_profile_name(profile: TerminalProfile) -> &'static str {
    match profile {
        TerminalProfile::Bmux256Color => "bmux-256color",
        TerminalProfile::Screen256Color => "screen-256color-compatible",
        TerminalProfile::Xterm256Color => "xterm-256color-compatible",
        TerminalProfile::Conservative => "conservative",
    }
}

pub(super) const fn protocol_profile_for_terminal_profile(
    profile: TerminalProfile,
) -> ProtocolProfile {
    match profile {
        TerminalProfile::Bmux256Color => ProtocolProfile::Bmux,
        TerminalProfile::Screen256Color => ProtocolProfile::Screen,
        TerminalProfile::Xterm256Color => ProtocolProfile::Xterm,
        TerminalProfile::Conservative => ProtocolProfile::Conservative,
    }
}

pub(super) fn check_terminfo_available(term: &str) -> Option<bool> {
    let output = ProcessCommand::new("infocmp").arg(term).output().ok()?;
    Some(output.status.success())
}

pub(super) fn run_keymap_doctor(as_json: bool) -> Result<u8> {
    let config = match BmuxConfig::load() {
        Ok(config) => config,
        Err(error) => {
            println!("bmux keymap doctor warning: failed to load config ({error}); using defaults");
            BmuxConfig::default()
        }
    };
    let (runtime_bindings, global_bindings, scroll_bindings) = merged_runtime_keybindings(&config);
    let resolved_timeout = config
        .keybindings
        .resolve_timeout()
        .map_err(anyhow::Error::msg)
        .context("failed resolving keymap timeout")?;
    let keymap = crate::input::Keymap::from_parts_with_scroll(
        &config.keybindings.prefix,
        resolved_timeout.timeout_ms(),
        &runtime_bindings,
        &global_bindings,
        &scroll_bindings,
    )
    .context("failed to compile keymap")?;

    let report = keymap.doctor_report();
    let attach_effective = effective_attach_keybindings(&config);

    if as_json {
        let payload = serde_json::json!({
            "prefix": config.keybindings.prefix,
            "timeout_ms": config.keybindings.timeout_ms,
            "timeout_profile": config.keybindings.timeout_profile,
            "timeout_profiles": config.keybindings.merged_timeout_profiles(),
            "resolved_timeout": match &resolved_timeout {
                ResolvedTimeout::Indefinite => serde_json::json!({
                    "mode": "indefinite"
                }),
                ResolvedTimeout::Exact(ms) => serde_json::json!({
                    "mode": "exact",
                    "ms": ms,
                }),
                ResolvedTimeout::Profile { name, ms } => serde_json::json!({
                    "mode": "profile",
                    "name": name,
                    "ms": ms,
                }),
            },
            "global": report
                .global
                .iter()
                .map(|binding| serde_json::json!({
                    "chord": binding.chord,
                    "action": binding.action,
                }))
                .collect::<Vec<_>>(),
            "runtime": report
                .runtime
                .iter()
                .map(|binding| serde_json::json!({
                    "chord": binding.chord,
                    "action": binding.action,
                }))
                .collect::<Vec<_>>(),
            "overlaps": report.overlaps,
            "attach_effective": attach_effective
                .iter()
                .map(|entry| serde_json::json!({
                    "scope": entry.scope.as_str(),
                    "chord": entry.chord,
                    "action": entry.action_name,
                }))
                .collect::<Vec<_>>(),
        });
        println!(
            "{}",
            serde_json::to_string_pretty(&payload)
                .context("failed to encode keymap doctor json")?
        );
        return Ok(0);
    }

    println!("bmux keymap doctor");
    println!("prefix: {}", config.keybindings.prefix);
    println!("timeout: {}", describe_timeout(&resolved_timeout));
    for line in keymap.doctor_lines() {
        println!("{line}");
    }

    println!("attach_effective:");
    for entry in attach_effective {
        println!(
            "  [{}] {} -> {}",
            entry.scope.as_str(),
            entry.chord,
            entry.action_name
        );
    }

    Ok(0)
}
#[cfg(test)]
mod tests {
    #[allow(clippy::wildcard_imports)]
    use crate::runtime::*;

    #[test]
    fn pane_term_profile_mapping_is_stable() {
        assert_eq!(
            profile_for_term("bmux-256color"),
            TerminalProfile::Bmux256Color
        );
        assert_eq!(
            profile_for_term("screen-256color"),
            TerminalProfile::Screen256Color
        );
        assert_eq!(
            profile_for_term("tmux-256color"),
            TerminalProfile::Screen256Color
        );
        assert_eq!(
            profile_for_term("xterm-256color"),
            TerminalProfile::Xterm256Color
        );
        assert_eq!(
            profile_for_term("weird-term"),
            TerminalProfile::Conservative
        );
    }

    #[test]
    fn pane_term_falls_back_to_xterm_then_screen() {
        let resolved = resolve_pane_term_with_checker("bmux-256color", |term| match term {
            "bmux-256color" => Some(false),
            "xterm-256color" => Some(true),
            "screen-256color" => Some(true),
            _ => Some(false),
        });

        assert_eq!(resolved.pane_term, "xterm-256color");
        assert_eq!(resolved.profile, TerminalProfile::Xterm256Color);
    }

    #[test]
    fn pane_term_uses_screen_when_xterm_unavailable() {
        let resolved = resolve_pane_term_with_checker("bmux-256color", |term| match term {
            "bmux-256color" => Some(false),
            "xterm-256color" => Some(false),
            "screen-256color" => Some(true),
            _ => Some(false),
        });

        assert_eq!(resolved.pane_term, "screen-256color");
        assert_eq!(resolved.profile, TerminalProfile::Screen256Color);
    }

    #[test]
    fn pane_term_keeps_configured_when_no_fallback_available() {
        let resolved = resolve_pane_term_with_checker("bmux-256color", |_term| Some(false));

        assert_eq!(resolved.pane_term, "bmux-256color");
        assert!(
            resolved
                .warnings
                .iter()
                .any(|w| w.contains("no fallback available"))
        );
    }

    #[test]
    fn protocol_profile_mapping_is_stable() {
        assert_eq!(
            protocol_profile_for_terminal_profile(TerminalProfile::Bmux256Color),
            crate::runtime::ProtocolProfile::Bmux
        );
        assert_eq!(
            protocol_profile_for_terminal_profile(TerminalProfile::Xterm256Color),
            crate::runtime::ProtocolProfile::Xterm
        );
        assert_eq!(
            protocol_profile_for_terminal_profile(TerminalProfile::Screen256Color),
            crate::runtime::ProtocolProfile::Screen
        );
        assert_eq!(
            protocol_profile_for_terminal_profile(TerminalProfile::Conservative),
            crate::runtime::ProtocolProfile::Conservative
        );
    }
}
