use anyhow::{Context, Result};
use bmux_config::{BmuxConfig, ConfigPaths};
use bmux_keybind::{RuntimeAction, parse_action};
use std::collections::{BTreeMap, BTreeSet, VecDeque};

use crate::input::{ModalModeConfig, canonical_chord_key};
use crate::runtime::attach::runtime::filtered_attach_keybindings;

fn config_file_path() -> std::path::PathBuf {
    ConfigPaths::default().config_file()
}

pub(super) fn run_config_path(as_json: bool) -> Result<u8> {
    let paths = ConfigPaths::default();
    let path = paths.config_file();
    let exists = path.exists();

    if as_json {
        let candidates: Vec<serde_json::Value> = paths
            .config_dir_candidates()
            .iter()
            .map(|dir| {
                let file = dir.join("bmux.toml");
                let file_exists = file.exists();
                serde_json::json!({
                    "path": file,
                    "exists": file_exists,
                })
            })
            .collect();

        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "path": path,
                "exists": exists,
                "candidates": candidates,
            }))
            .context("failed to encode config path json")?
        );
        return Ok(0);
    }

    if exists {
        println!("{}", path.display());
    } else {
        println!("{} (does not exist)", path.display());
    }
    Ok(0)
}

pub(super) fn run_config_show(as_json: bool) -> Result<u8> {
    let config = BmuxConfig::load().map_err(|e| anyhow::anyhow!("{e}"))?;

    if as_json {
        println!(
            "{}",
            serde_json::to_string_pretty(
                &toml::Value::try_from(&config).context("failed to serialize config")?
            )
            .context("failed to encode config json")?
        );
        return Ok(0);
    }

    let toml_str = toml::to_string_pretty(&config).context("failed to serialize config to TOML")?;
    print!("{toml_str}");
    Ok(0)
}

pub(super) fn run_config_get(key: &str, as_json: bool) -> Result<u8> {
    let config = BmuxConfig::load().map_err(|e| anyhow::anyhow!("{e}"))?;
    let table =
        toml::Value::try_from(&config).context("failed to serialize config to TOML value")?;

    let value = resolve_dotted_key(&table, key).with_context(|| format!("key not found: {key}"))?;

    if as_json {
        println!(
            "{}",
            serde_json::to_string_pretty(&toml_value_to_json(value))
                .context("failed to encode value as json")?
        );
        return Ok(0);
    }

    match value {
        toml::Value::String(s) => println!("{s}"),
        toml::Value::Integer(i) => println!("{i}"),
        toml::Value::Float(f) => println!("{f}"),
        toml::Value::Boolean(b) => println!("{b}"),
        toml::Value::Array(_) | toml::Value::Table(_) => {
            print!(
                "{}",
                toml::to_string_pretty(value).context("failed to format value as TOML")?
            );
        }
        toml::Value::Datetime(dt) => println!("{dt}"),
    }
    Ok(0)
}

pub(super) fn run_config_set(key: &str, raw_value: &str) -> Result<u8> {
    let paths = ConfigPaths::default();
    let path = paths.config_file();

    let source = if path.exists() {
        std::fs::read_to_string(&path)
            .with_context(|| format!("failed to read {}", path.display()))?
    } else {
        String::new()
    };

    let mut doc: toml_edit::DocumentMut = source
        .parse()
        .with_context(|| format!("failed to parse {}", path.display()))?;

    let parsed_value = parse_cli_value(raw_value);

    set_dotted_key(&mut doc, key, parsed_value)
        .with_context(|| format!("failed to set key: {key}"))?;

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create config directory {}", parent.display()))?;
    }
    std::fs::write(&path, doc.to_string())
        .with_context(|| format!("failed to write {}", path.display()))?;

    println!("{key} = {raw_value}");
    Ok(0)
}

pub(super) fn run_config_profiles_list(as_json: bool) -> Result<u8> {
    let (_config, resolution) =
        BmuxConfig::load_with_resolution().map_err(|e| anyhow::anyhow!("{e}"))?;
    if as_json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "profiles": resolution.available_profiles,
            }))
            .context("failed to encode profiles json")?
        );
    } else {
        for profile in resolution.available_profiles {
            println!("{profile}");
        }
    }
    Ok(0)
}

pub(super) fn run_config_profiles_show(profile: &str, as_json: bool) -> Result<u8> {
    let path = config_file_path();
    let (config, resolution) = BmuxConfig::load_from_path_with_resolution(&path, Some(profile))
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let value = toml::Value::try_from(&config).context("failed to serialize config")?;
    if as_json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "profile": profile,
                "resolution": resolution,
                "resolved_config": value,
            }))
            .context("failed to encode profile json")?
        );
    } else {
        println!("profile: {profile}");
        if let Some(source) = resolution.selected_profile_source {
            println!("source: {source}");
        }
        let rendered = toml::to_string_pretty(&value).context("failed to render profile config")?;
        print!("{rendered}");
    }
    Ok(0)
}

pub(super) fn run_config_profiles_resolve(profile: Option<&str>, as_json: bool) -> Result<u8> {
    let path = config_file_path();
    let (_config, resolution) = BmuxConfig::load_from_path_with_resolution(&path, profile)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    if as_json {
        println!(
            "{}",
            serde_json::to_string_pretty(&resolution)
                .context("failed to encode resolution json")?
        );
    } else {
        println!(
            "selected_profile: {}",
            resolution.selected_profile.as_deref().unwrap_or("<none>")
        );
        println!(
            "source: {}",
            resolution
                .selected_profile_source
                .as_deref()
                .unwrap_or("<none>")
        );
        if let Some(index) = resolution.matched_auto_select_index {
            println!("matched_auto_select_index: {index}");
        }
        println!("layer_order: {}", resolution.layer_order.join(" -> "));
    }
    Ok(0)
}

pub(super) fn run_config_profiles_explain(profile: Option<&str>, as_json: bool) -> Result<u8> {
    let path = config_file_path();
    let (_config, explain) = BmuxConfig::load_from_path_with_explain(&path, profile)
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    if as_json {
        println!(
            "{}",
            serde_json::to_string_pretty(&explain).context("failed to encode explain json")?
        );
        return Ok(0);
    }

    println!(
        "selected_profile: {}",
        explain
            .resolution
            .selected_profile
            .as_deref()
            .unwrap_or("<none>")
    );
    println!(
        "source: {}",
        explain
            .resolution
            .selected_profile_source
            .as_deref()
            .unwrap_or("<none>")
    );
    println!(
        "layer_order: {}",
        explain.resolution.layer_order.join(" -> ")
    );
    println!("applied layers:");
    for layer in &explain.applied_layers {
        if layer.changed_paths.is_empty() {
            println!("  - {}: no changes", layer.layer);
            continue;
        }
        println!(
            "  - {}: {} changed paths",
            layer.layer,
            layer.changed_paths.len()
        );
        for path in layer.changed_paths.iter().take(12) {
            println!("      {path}");
        }
        if layer.changed_paths.len() > 12 {
            println!("      ... {} more", layer.changed_paths.len() - 12);
        }
    }

    Ok(0)
}

pub(super) fn run_config_profiles_diff(from: &str, to: &str, as_json: bool) -> Result<u8> {
    let path = config_file_path();
    let (from_config, _) = BmuxConfig::load_from_path_with_resolution(&path, Some(from))
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let (to_config, _) = BmuxConfig::load_from_path_with_resolution(&path, Some(to))
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let from_value =
        toml::Value::try_from(&from_config).context("failed to serialize from config")?;
    let to_value = toml::Value::try_from(&to_config).context("failed to serialize to config")?;
    let changed_paths = diff_toml_paths(&from_value, &to_value);
    let top_level_changes = summarize_top_level_changes(&changed_paths);
    if as_json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "from": from,
                "to": to,
                "changed_paths": changed_paths,
                "top_level_changes": top_level_changes,
                "from_config": from_value,
                "to_config": to_value,
            }))
            .context("failed to encode diff json")?
        );
    } else {
        println!("from profile: {from}");
        println!("to profile: {to}");
        if changed_paths.is_empty() {
            println!("no differences");
        } else {
            println!("resolved configurations differ");
            println!("changed paths: {}", changed_paths.len());
            if !top_level_changes.is_empty() {
                println!("top-level sections changed:");
                for (section, count) in top_level_changes {
                    println!("  - {section}: {count}");
                }
            }
            for path in changed_paths.iter().take(20) {
                println!("  - {path}");
            }
            if changed_paths.len() > 20 {
                println!("  ... {} more", changed_paths.len() - 20);
            }
        }
    }
    Ok(0)
}

pub(super) fn run_config_profiles_lint(as_json: bool) -> Result<u8> {
    let path = config_file_path();
    let (config, resolution) = BmuxConfig::load_from_path_with_resolution(&path, None)
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    let mut warnings = Vec::new();
    warnings.extend(unreachable_mode_warnings(&config));
    warnings.extend(global_vs_mode_conflict_warnings(&config));

    let timeout_ms = config
        .keybindings
        .resolve_timeout()
        .map_err(anyhow::Error::msg)
        .context("failed resolving keymap timeout")?
        .timeout_ms();
    let (_runtime_bindings, global_bindings, scroll_bindings) =
        filtered_attach_keybindings(&config);
    let modal_modes = config
        .keybindings
        .modes
        .iter()
        .map(|(mode_id, mode)| {
            (
                mode_id.clone(),
                ModalModeConfig {
                    label: mode.label.clone(),
                    passthrough: mode.passthrough,
                    bindings: mode.bindings.clone(),
                },
            )
        })
        .collect::<BTreeMap<_, _>>();

    let keymap = crate::input::Keymap::from_modal_parts_with_scroll(
        timeout_ms,
        &config.keybindings.initial_mode,
        &modal_modes,
        &global_bindings,
        &scroll_bindings,
    )
    .context("failed compiling modal keymap for lint")?;
    warnings.extend(keymap.overlap_warnings());

    if as_json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "ok": true,
                "available_profiles": resolution.available_profiles,
                "warning_count": warnings.len(),
                "warnings": warnings,
            }))
            .context("failed to encode lint json")?
        );
    } else {
        println!(
            "ok: {} profiles validated",
            resolution.available_profiles.len()
        );
        if warnings.is_empty() {
            println!("lint warnings: none");
        } else {
            println!("lint warnings: {}", warnings.len());
            for warning in warnings {
                println!("  - {warning}");
            }
        }
    }
    Ok(0)
}

pub(super) fn run_config_profiles_evaluate(as_json: bool) -> Result<u8> {
    let path = config_file_path();
    let (_config, resolution) = BmuxConfig::load_from_path_with_resolution(&path, None)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    if as_json {
        println!(
            "{}",
            serde_json::to_string_pretty(&resolution).context("failed to encode evaluate json")?
        );
    } else {
        println!(
            "selected profile: {}",
            resolution.selected_profile.as_deref().unwrap_or("<none>")
        );
        if let Some(index) = resolution.matched_auto_select_index {
            println!("matched auto_select rule: {index}");
        }
    }
    Ok(0)
}

pub(super) fn run_config_profiles_switch(
    profile: &str,
    dry_run: bool,
    as_json: bool,
) -> Result<u8> {
    let path = config_file_path();
    let (before_config, before_resolution) =
        BmuxConfig::load_from_path_with_resolution(&path, None)
            .map_err(|e| anyhow::anyhow!("{e}"))?;
    let (after_config, after_resolution) =
        BmuxConfig::load_from_path_with_resolution(&path, Some(profile))
            .map_err(|e| anyhow::anyhow!("{e}"))?;

    let before_value =
        toml::Value::try_from(&before_config).context("failed to serialize previous config")?;
    let after_value =
        toml::Value::try_from(&after_config).context("failed to serialize switched config")?;
    let changed_paths = diff_toml_paths(&before_value, &after_value);

    if !dry_run {
        run_config_profiles_set_active(profile)?;
    }

    if as_json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "dry_run": dry_run,
                "requested_profile": profile,
                "previous_profile": before_resolution.selected_profile,
                "next_profile": after_resolution.selected_profile,
                "changed_paths": changed_paths,
                "changed_path_count": changed_paths.len(),
                "wrote_config": !dry_run,
            }))
            .context("failed to encode switch json")?
        );
        return Ok(0);
    }

    if dry_run {
        println!("dry-run: switch_profile {profile}");
    } else {
        println!("switched active profile to {profile}");
    }
    println!("changed paths: {}", changed_paths.len());
    for path in changed_paths.iter().take(16) {
        println!("  - {path}");
    }
    if changed_paths.len() > 16 {
        println!("  ... {} more", changed_paths.len() - 16);
    }

    Ok(0)
}

pub(super) fn run_config_profiles_set_active(profile: &str) -> Result<()> {
    let path = ConfigPaths::default().config_file();
    run_config_profiles_set_active_at_path(profile, &path)
}

pub(super) fn run_config_profiles_set_active_at_path(
    profile: &str,
    path: &std::path::Path,
) -> Result<()> {
    let source = if path.exists() {
        std::fs::read_to_string(path)
            .with_context(|| format!("failed to read {}", path.display()))?
    } else {
        String::new()
    };

    let mut doc: toml_edit::DocumentMut = source
        .parse()
        .with_context(|| format!("failed to parse {}", path.display()))?;
    set_dotted_key(
        &mut doc,
        "composition.active_profile",
        toml_edit::value(profile)
            .into_value()
            .expect("string value"),
    )
    .context("failed setting composition.active_profile")?;

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create config directory {}", parent.display()))?;
    }
    std::fs::write(path, doc.to_string())
        .with_context(|| format!("failed to write {}", path.display()))?;
    Ok(())
}

fn resolve_dotted_key<'a>(table: &'a toml::Value, key: &str) -> Option<&'a toml::Value> {
    let mut current = table;
    for segment in key.split('.') {
        current = current.as_table()?.get(segment)?;
    }
    Some(current)
}

fn toml_value_to_json(value: &toml::Value) -> serde_json::Value {
    match value {
        toml::Value::String(s) => serde_json::Value::String(s.clone()),
        toml::Value::Integer(i) => serde_json::json!(i),
        toml::Value::Float(f) => serde_json::json!(f),
        toml::Value::Boolean(b) => serde_json::json!(b),
        toml::Value::Datetime(dt) => serde_json::Value::String(dt.to_string()),
        toml::Value::Array(arr) => {
            serde_json::Value::Array(arr.iter().map(toml_value_to_json).collect())
        }
        toml::Value::Table(tbl) => {
            let map = tbl
                .iter()
                .map(|(k, v)| (k.clone(), toml_value_to_json(v)))
                .collect();
            serde_json::Value::Object(map)
        }
    }
}

fn collect_toml_diff_paths(
    before: Option<&toml::Value>,
    after: Option<&toml::Value>,
    prefix: &str,
    out: &mut BTreeSet<String>,
) {
    match (before, after) {
        (None, None) => {}
        (Some(left), Some(right)) if left == right => {}
        (Some(toml::Value::Table(left)), Some(toml::Value::Table(right))) => {
            let keys = left
                .keys()
                .chain(right.keys())
                .cloned()
                .collect::<BTreeSet<_>>();
            for key in keys {
                let next_prefix = if prefix.is_empty() {
                    key.clone()
                } else {
                    format!("{prefix}.{key}")
                };
                collect_toml_diff_paths(left.get(&key), right.get(&key), &next_prefix, out);
            }
        }
        _ => {
            if prefix.is_empty() {
                out.insert("<root>".to_string());
            } else {
                out.insert(prefix.to_string());
            }
        }
    }
}

fn diff_toml_paths(before: &toml::Value, after: &toml::Value) -> Vec<String> {
    let mut out = BTreeSet::new();
    collect_toml_diff_paths(Some(before), Some(after), "", &mut out);
    out.into_iter().collect()
}

fn summarize_top_level_changes(changed_paths: &[String]) -> Vec<(String, usize)> {
    let mut counts = BTreeMap::<String, usize>::new();
    for path in changed_paths {
        let section = path
            .split('.')
            .next()
            .map_or_else(|| path.clone(), std::string::ToString::to_string);
        *counts.entry(section).or_default() += 1;
    }
    counts.into_iter().collect()
}

fn unreachable_mode_warnings(config: &BmuxConfig) -> Vec<String> {
    let known_modes = config
        .keybindings
        .modes
        .keys()
        .map(|id| id.to_ascii_lowercase())
        .collect::<BTreeSet<_>>();
    let mode_lookup = config
        .keybindings
        .modes
        .iter()
        .map(|(id, mode)| (id.to_ascii_lowercase(), mode))
        .collect::<BTreeMap<_, _>>();

    let mut visited = BTreeSet::new();
    let mut queue = VecDeque::new();
    let initial = config.keybindings.initial_mode.to_ascii_lowercase();
    queue.push_back(initial.clone());
    visited.insert(initial);

    while let Some(mode_id) = queue.pop_front() {
        let Some(mode) = mode_lookup.get(&mode_id) else {
            continue;
        };
        for action_name in mode.bindings.values() {
            if let Ok(RuntimeAction::EnterMode(target_mode)) = parse_action(action_name) {
                let canonical = target_mode.to_ascii_lowercase();
                if known_modes.contains(&canonical) && !visited.contains(&canonical) {
                    visited.insert(canonical.clone());
                    queue.push_back(canonical);
                }
            }
        }
    }

    known_modes
        .iter()
        .filter(|mode_id| !visited.contains(*mode_id))
        .map(|mode_id| {
            format!(
                "mode '{mode_id}' is unreachable from initial_mode '{}'",
                config.keybindings.initial_mode
            )
        })
        .collect()
}

fn global_vs_mode_conflict_warnings(config: &BmuxConfig) -> Vec<String> {
    let global = config
        .keybindings
        .global
        .keys()
        .map(|key| canonical_chord_key(key))
        .collect::<BTreeSet<_>>();
    let mut warnings = Vec::new();
    for (mode_id, mode) in &config.keybindings.modes {
        for key in mode.bindings.keys() {
            let canonical = canonical_chord_key(key);
            if global.contains(&canonical) {
                warnings.push(format!(
                    "global chord '{canonical}' overrides mode '{mode_id}' chord '{key}'"
                ));
            }
        }
    }
    warnings
}

fn parse_cli_value(raw: &str) -> toml_edit::Value {
    if raw.eq_ignore_ascii_case("true") {
        return toml_edit::value(true).into_value().unwrap();
    }
    if raw.eq_ignore_ascii_case("false") {
        return toml_edit::value(false).into_value().unwrap();
    }
    if let Ok(i) = raw.parse::<i64>() {
        return toml_edit::value(i).into_value().unwrap();
    }
    if let Ok(f) = raw.parse::<f64>()
        && raw.contains('.')
    {
        return toml_edit::value(f).into_value().unwrap();
    }
    toml_edit::value(raw).into_value().unwrap()
}

fn set_dotted_key(
    doc: &mut toml_edit::DocumentMut,
    key: &str,
    value: toml_edit::Value,
) -> Result<()> {
    let segments: Vec<&str> = key.split('.').collect();
    if segments.is_empty() {
        anyhow::bail!("key must not be empty");
    }

    let mut table = doc.as_table_mut();
    for &segment in &segments[..segments.len() - 1] {
        if !table.contains_key(segment) {
            table.insert(segment, toml_edit::Item::Table(toml_edit::Table::new()));
        }
        match table.get_mut(segment) {
            Some(toml_edit::Item::Table(t)) => table = t,
            Some(_) => anyhow::bail!(
                "cannot traverse into non-table key '{segment}' while setting '{key}'"
            ),
            None => unreachable!(),
        }
    }

    let leaf = segments.last().unwrap();
    table[*leaf] = toml_edit::Item::Value(value);
    Ok(())
}

#[cfg(test)]
mod tests {
    #[allow(clippy::wildcard_imports)]
    use super::*;
    #[test]
    fn resolve_dotted_key_traverses_nested_tables() {
        let input: toml::Value = toml::from_str(
            r#"
            [appearance]
            theme = "night"
            [behavior]
            [behavior.mouse]
            enabled = true
            "#,
        )
        .unwrap();

        assert_eq!(
            resolve_dotted_key(&input, "appearance.theme"),
            Some(&toml::Value::String("night".to_string()))
        );
        assert_eq!(
            resolve_dotted_key(&input, "behavior.mouse.enabled"),
            Some(&toml::Value::Boolean(true))
        );
        assert!(resolve_dotted_key(&input, "nonexistent.key").is_none());
    }

    #[test]
    #[allow(clippy::approx_constant)] // 3.14 is a test value, not an approximation of PI
    fn parse_cli_value_detects_types() {
        assert!(parse_cli_value("true").as_bool().unwrap());
        assert!(!parse_cli_value("false").as_bool().unwrap());
        assert!(!parse_cli_value("FALSE").as_bool().unwrap());
        assert_eq!(parse_cli_value("42").as_integer().unwrap(), 42);
        assert!((parse_cli_value("3.14").as_float().unwrap() - 3.14).abs() < f64::EPSILON);
        assert_eq!(parse_cli_value("hello").as_str().unwrap(), "hello");
    }

    #[test]
    fn set_dotted_key_creates_intermediate_tables() {
        let mut doc: toml_edit::DocumentMut = "".parse().unwrap();
        set_dotted_key(
            &mut doc,
            "appearance.theme",
            toml_edit::value("dark").into_value().unwrap(),
        )
        .unwrap();
        let output = doc.to_string();
        assert!(output.contains("[appearance]"));
        assert!(output.contains("theme = \"dark\""));
    }

    #[test]
    fn set_dotted_key_preserves_existing_content() {
        let mut doc: toml_edit::DocumentMut = "[general]\nshell = \"bash\"\n".parse().unwrap();
        set_dotted_key(
            &mut doc,
            "general.scrollback_limit",
            toml_edit::value(5000).into_value().unwrap(),
        )
        .unwrap();
        let output = doc.to_string();
        assert!(output.contains("shell = \"bash\""));
        assert!(output.contains("scrollback_limit = 5000"));
    }
}
