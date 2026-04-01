use super::*;
use std::collections::HashSet;

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(super) struct DetectedTerminalProfile {
    pub(super) terminal_id: String,
    pub(super) font_families: Vec<String>,
    pub(super) font_size_px: Option<u16>,
    pub(super) background_opacity_permille: Option<u16>,
    #[serde(default)]
    pub(super) cursor_defaults: CursorDefaults,
    pub(super) source: String,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, Default)]
pub(super) struct CursorDefaults {
    pub(super) profile: Option<CursorDefaultProfile>,
    pub(super) shape: Option<CursorDefaultShape>,
    pub(super) blink: Option<CursorDefaultBlink>,
    pub(super) color: Option<String>,
    pub(super) solid_after_input_ms: Option<u32>,
    pub(super) solid_after_output_ms: Option<u32>,
    pub(super) solid_after_cursor_ms: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum CursorDefaultProfile {
    Ghostty,
    Generic,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum CursorDefaultShape {
    Block,
    Bar,
    Underline,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum CursorDefaultBlink {
    On,
    Off,
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

#[derive(Debug, Default)]
struct GhosttyConfigProfile {
    font_families: Vec<String>,
    font_size_px: Option<u16>,
    background_opacity_permille: Option<u16>,
    cursor_defaults: CursorDefaults,
}

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
            "JetBrainsMono Nerd Font".to_string(),
            "JetBrains Mono".to_string(),
            "Symbols Nerd Font Mono".to_string(),
        ]
    }

    fn resolve_from_config(path: &Path) -> Option<DetectedTerminalProfile> {
        let content = std::fs::read_to_string(path).ok()?;
        let parsed = parse_ghostty_config_profile(&content);
        if parsed.font_families.is_empty()
            && parsed.font_size_px.is_none()
            && parsed.background_opacity_permille.is_none()
            && parsed.cursor_defaults == CursorDefaults::default()
        {
            return None;
        }
        Some(DetectedTerminalProfile {
            terminal_id: "ghostty".to_string(),
            font_families: if parsed.font_families.is_empty() {
                Self::default_font_families()
            } else {
                parsed.font_families
            },
            font_size_px: parsed.font_size_px,
            background_opacity_permille: parsed.background_opacity_permille,
            cursor_defaults: CursorDefaults {
                profile: Some(CursorDefaultProfile::Ghostty),
                solid_after_input_ms: Some(500),
                solid_after_output_ms: Some(500),
                solid_after_cursor_ms: Some(500),
                ..parsed.cursor_defaults
            },
            source: format!("ghostty-config:{}", path.display()),
        })
    }

    fn default_profile() -> DetectedTerminalProfile {
        DetectedTerminalProfile {
            terminal_id: "ghostty".to_string(),
            font_families: Self::default_font_families(),
            font_size_px: None,
            background_opacity_permille: None,
            cursor_defaults: CursorDefaults {
                profile: Some(CursorDefaultProfile::Ghostty),
                solid_after_input_ms: Some(500),
                solid_after_output_ms: Some(500),
                solid_after_cursor_ms: Some(500),
                ..CursorDefaults::default()
            },
            source: "ghostty-default".to_string(),
        }
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
        for path in Self::config_paths(env) {
            if let Some(profile) = Self::resolve_from_config(&path) {
                return Some(profile);
            }
        }
        Some(Self::default_profile())
    }
}

fn parse_ghostty_config_profile(content: &str) -> GhosttyConfigProfile {
    let mut profile = GhosttyConfigProfile::default();
    for raw_line in content.lines() {
        let line = strip_inline_comment(raw_line);
        if line.is_empty() {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        let parsed = trim_config_value(value);
        match key {
            "font-family" => {
                if parsed.is_empty() {
                    profile.font_families.clear();
                    continue;
                }
                profile.font_families.push(parsed);
            }
            "font-size" => {
                if parsed.is_empty() {
                    profile.font_size_px = None;
                    continue;
                }
                if let Some(font_size) = parse_ghostty_font_size_px(&parsed) {
                    profile.font_size_px = Some(font_size);
                }
            }
            "background-opacity" => {
                if parsed.is_empty() {
                    profile.background_opacity_permille = None;
                    continue;
                }
                if let Some(opacity) = parse_ghostty_background_opacity_permille(&parsed) {
                    profile.background_opacity_permille = Some(opacity);
                }
            }
            "cursor-style" => {
                if parsed.is_empty() {
                    profile.cursor_defaults.shape = None;
                    continue;
                }
                profile.cursor_defaults.shape = parse_ghostty_cursor_style(&parsed);
            }
            "cursor-style-blink" => {
                if parsed.is_empty() {
                    profile.cursor_defaults.blink = None;
                    continue;
                }
                profile.cursor_defaults.blink = parse_ghostty_cursor_blink(&parsed);
            }
            "cursor-color" => {
                if parsed.is_empty() {
                    profile.cursor_defaults.color = None;
                    continue;
                }
                profile.cursor_defaults.color = Some(parsed);
            }
            _ => {}
        }
    }
    profile
}

fn parse_ghostty_cursor_style(value: &str) -> Option<CursorDefaultShape> {
    match value.trim().to_ascii_lowercase().as_str() {
        "block" | "block_hollow" => Some(CursorDefaultShape::Block),
        "bar" => Some(CursorDefaultShape::Bar),
        "underline" => Some(CursorDefaultShape::Underline),
        _ => None,
    }
}

fn parse_ghostty_cursor_blink(value: &str) -> Option<CursorDefaultBlink> {
    match value.trim().to_ascii_lowercase().as_str() {
        "true" => Some(CursorDefaultBlink::On),
        "false" => Some(CursorDefaultBlink::Off),
        _ => None,
    }
}

fn parse_ghostty_font_size_px(value: &str) -> Option<u16> {
    let numeric = value.parse::<f32>().ok()?;
    if numeric <= 0.0 {
        return None;
    }
    let rounded = numeric.round();
    if rounded > f32::from(u16::MAX) {
        return None;
    }
    Some(rounded as u16)
}

fn parse_ghostty_background_opacity_permille(value: &str) -> Option<u16> {
    let numeric = value.parse::<f32>().ok()?;
    let clamped = numeric.clamp(0.0, 1.0);
    let permille = (clamped * 1000.0).round();
    if permille > f32::from(u16::MAX) {
        return None;
    }
    Some(permille as u16)
}

fn strip_inline_comment(line: &str) -> String {
    let mut in_single = false;
    let mut in_double = false;
    let mut escaped = false;
    let mut out = String::new();
    for ch in line.chars() {
        if escaped {
            out.push(ch);
            escaped = false;
            continue;
        }
        if ch == '\\' {
            escaped = true;
            out.push(ch);
            continue;
        }
        if ch == '\'' && !in_double {
            in_single = !in_single;
            out.push(ch);
            continue;
        }
        if ch == '"' && !in_single {
            in_double = !in_double;
            out.push(ch);
            continue;
        }
        if ch == '#' && !in_single && !in_double {
            break;
        }
        out.push(ch);
    }
    out.trim().to_string()
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
        let parsed = parse_ghostty_config_profile(
            r#"
font-family = "JetBrains Mono"
font-family = "Symbols Nerd Font Mono"
font-family = ""
font-family = "Iosevka"
font-size = 16
background-opacity = 0.9
cursor-style = bar
cursor-style-blink = false
cursor-color = '#33aaee'
"#,
        );
        assert_eq!(parsed.font_families, vec!["Iosevka".to_string()]);
        assert_eq!(parsed.font_size_px, Some(16));
        assert_eq!(parsed.background_opacity_permille, Some(900));
        assert_eq!(parsed.cursor_defaults.shape, Some(CursorDefaultShape::Bar));
        assert_eq!(parsed.cursor_defaults.blink, Some(CursorDefaultBlink::Off));
        assert_eq!(parsed.cursor_defaults.color, Some("#33aaee".to_string()));
    }

    #[test]
    fn parse_ghostty_profile_keeps_hash_inside_quotes() {
        let parsed = parse_ghostty_config_profile(
            r#"
font-family = "Jet#Brains Mono" # inline comment
"#,
        );
        assert_eq!(parsed.font_families, vec!["Jet#Brains Mono".to_string()]);
    }

    #[test]
    fn parse_ghostty_background_opacity_clamps_to_unit_interval() {
        assert_eq!(parse_ghostty_background_opacity_permille("-1"), Some(0));
        assert_eq!(parse_ghostty_background_opacity_permille("0.5"), Some(500));
        assert_eq!(parse_ghostty_background_opacity_permille("1.5"), Some(1000));
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
        assert_eq!(
            profile.cursor_defaults.profile,
            Some(CursorDefaultProfile::Ghostty)
        );
    }

    #[test]
    fn resolve_ghostty_profile_prefers_config_when_available() {
        let suffix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let temp = std::env::temp_dir().join(format!("bmux-ghostty-test-{suffix}"));
        let config_dir = temp.join(".config/ghostty");
        std::fs::create_dir_all(&config_dir).expect("create config dir");
        std::fs::write(
            config_dir.join("config"),
            "font-family = 'Iosevka Term'\nfont-size = 15\ncursor-style = underline\n",
        )
        .expect("write config");
        let env = EnvSnapshot {
            term_program: Some("ghostty".to_string()),
            term: None,
            home_dir: Some(temp.clone()),
            xdg_config_home: None,
        };
        let profile = detect_render_profile_for_env(&env).expect("profile should be detected");
        assert_eq!(profile.font_families, vec!["Iosevka Term".to_string()]);
        assert_eq!(profile.font_size_px, Some(15));
        assert_eq!(profile.background_opacity_permille, None);
        assert_eq!(
            profile.cursor_defaults.shape,
            Some(CursorDefaultShape::Underline)
        );
        assert!(profile.source.contains("ghostty-config:"));
        let _ = std::fs::remove_dir_all(&temp);
    }
}
