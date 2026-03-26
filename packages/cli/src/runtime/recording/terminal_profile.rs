use super::*;
use std::collections::HashSet;

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(super) struct DetectedTerminalProfile {
    pub(super) terminal_id: String,
    pub(super) font_families: Vec<String>,
    pub(super) source: String,
}

trait TerminalProfileProvider {
    fn detect_score(&self, env: &EnvSnapshot) -> u8;
    fn resolve(&self, env: &EnvSnapshot) -> Option<DetectedTerminalProfile>;
}

pub(super) fn detect_render_profile() -> Option<DetectedTerminalProfile> {
    detect_render_profile_for_env(&EnvSnapshot::from_process())
}

fn detect_render_profile_for_env(env: &EnvSnapshot) -> Option<DetectedTerminalProfile> {
    let providers: [&dyn TerminalProfileProvider; 1] = [&GhosttyProvider];
    let mut best = None::<(u8, &dyn TerminalProfileProvider)>;
    for provider in providers {
        let score = provider.detect_score(env);
        if score == 0 {
            continue;
        }
        if best.is_none_or(|(best_score, _)| score > best_score) {
            best = Some((score, provider));
        }
    }
    best.and_then(|(_, provider)| provider.resolve(env))
}

#[derive(Debug, Clone)]
struct EnvSnapshot {
    term_program: Option<String>,
    term: Option<String>,
    home_dir: Option<PathBuf>,
    xdg_config_home: Option<PathBuf>,
}

impl EnvSnapshot {
    fn from_process() -> Self {
        Self {
            term_program: std::env::var("TERM_PROGRAM").ok(),
            term: std::env::var("TERM").ok(),
            home_dir: std::env::var("HOME").ok().map(PathBuf::from),
            xdg_config_home: std::env::var("XDG_CONFIG_HOME").ok().map(PathBuf::from),
        }
    }
}

struct GhosttyProvider;

impl GhosttyProvider {
    fn config_paths(env: &EnvSnapshot) -> Vec<PathBuf> {
        let mut candidates = Vec::new();
        if let Some(xdg) = &env.xdg_config_home {
            candidates.push(xdg.join("ghostty/config"));
        }
        if let Some(home) = &env.home_dir {
            candidates.push(home.join(".config/ghostty/config"));
            candidates.push(home.join(".config/nix/configs/ghostty/config"));
        }
        let mut dedupe = HashSet::new();
        candidates
            .into_iter()
            .filter(|path| dedupe.insert(path.clone()))
            .collect()
    }

    fn default_font_families() -> Vec<String> {
        vec![
            "JetBrains Mono".to_string(),
            "JetBrainsMono Nerd Font".to_string(),
            "Symbols Nerd Font Mono".to_string(),
        ]
    }
}

impl TerminalProfileProvider for GhosttyProvider {
    fn detect_score(&self, env: &EnvSnapshot) -> u8 {
        if env
            .term_program
            .as_deref()
            .is_some_and(|value| value.eq_ignore_ascii_case("ghostty"))
        {
            return 100;
        }
        if env
            .term
            .as_deref()
            .is_some_and(|value| value.contains("ghostty"))
        {
            return 90;
        }
        if Self::config_paths(env).iter().any(|path| path.exists()) {
            return 30;
        }
        0
    }

    fn resolve(&self, env: &EnvSnapshot) -> Option<DetectedTerminalProfile> {
        let mut font_families = Vec::new();
        let mut source = "ghostty-default".to_string();

        for path in Self::config_paths(env) {
            let Ok(content) = std::fs::read_to_string(&path) else {
                continue;
            };
            let parsed = parse_ghostty_font_families(&content);
            if !parsed.is_empty() {
                font_families = parsed;
                source = format!("ghostty-config:{}", path.display());
                break;
            }
        }

        if font_families.is_empty() {
            font_families = Self::default_font_families();
        }

        Some(DetectedTerminalProfile {
            terminal_id: "ghostty".to_string(),
            font_families,
            source,
        })
    }
}

fn parse_ghostty_font_families(content: &str) -> Vec<String> {
    let mut families = Vec::new();
    for raw_line in content.lines() {
        let line = raw_line.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if key.trim() != "font-family" {
            continue;
        }
        let parsed = trim_config_value(value);
        if parsed.is_empty() {
            families.clear();
            continue;
        }
        families.push(parsed);
    }
    families
}

fn trim_config_value(value: &str) -> String {
    let trimmed = value.trim();
    if trimmed.len() >= 2 {
        let quoted = (trimmed.starts_with('"') && trimmed.ends_with('"'))
            || (trimmed.starts_with('\'') && trimmed.ends_with('\''));
        if quoted {
            return trimmed[1..trimmed.len() - 1].to_string();
        }
    }
    trimmed.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_ghostty_font_families_handles_repeat_and_reset() {
        let parsed = parse_ghostty_font_families(
            r#"
font-family = "JetBrains Mono"
font-family = "Symbols Nerd Font Mono"
font-family = ""
font-family = "Iosevka"
"#,
        );
        assert_eq!(parsed, vec!["Iosevka".to_string()]);
    }

    #[test]
    fn detect_ghostty_profile_from_term_program() {
        let env = EnvSnapshot {
            term_program: Some("ghostty".to_string()),
            term: None,
            home_dir: None,
            xdg_config_home: None,
        };
        let profile = detect_render_profile_for_env(&env).expect("profile should be detected");
        assert_eq!(profile.terminal_id, "ghostty");
        assert!(!profile.font_families.is_empty());
    }
}
