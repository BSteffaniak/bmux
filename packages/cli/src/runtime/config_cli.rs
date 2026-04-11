use anyhow::{Context, Result};
use bmux_config::{BmuxConfig, ConfigPaths};

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

pub(super) fn run_config_profiles_diff(from: &str, to: &str, as_json: bool) -> Result<u8> {
    let path = config_file_path();
    let (from_config, _) = BmuxConfig::load_from_path_with_resolution(&path, Some(from))
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let (to_config, _) = BmuxConfig::load_from_path_with_resolution(&path, Some(to))
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let from_value =
        toml::Value::try_from(&from_config).context("failed to serialize from config")?;
    let to_value = toml::Value::try_from(&to_config).context("failed to serialize to config")?;
    if as_json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "from": from,
                "to": to,
                "from_config": from_value,
                "to_config": to_value,
            }))
            .context("failed to encode diff json")?
        );
    } else {
        println!("from profile: {from}");
        println!("to profile: {to}");
        if from_value == to_value {
            println!("no differences");
        } else {
            println!("resolved configurations differ");
        }
    }
    Ok(0)
}

pub(super) fn run_config_profiles_lint(as_json: bool) -> Result<u8> {
    let path = config_file_path();
    let (_config, resolution) = BmuxConfig::load_from_path_with_resolution(&path, None)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    if as_json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "ok": true,
                "available_profiles": resolution.available_profiles,
            }))
            .context("failed to encode lint json")?
        );
    } else {
        println!(
            "ok: {} profiles validated",
            resolution.available_profiles.len()
        );
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

pub(super) fn run_config_profiles_set_active(profile: &str) -> Result<()> {
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
    std::fs::write(&path, doc.to_string())
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
