//! Renders the sidebar for review: realistic tabs in several workspaces, at
//! several widths, in a dark and a light theme.
//!
//! `cargo test -p bmux_sidebar_plugin preview -- --ignored` with
//! `BMUX_SIDEBAR_PREVIEW_DIR` set writes one JSON file per picture there:
//! the theme's colors and every text run the sidebar painted, for a renderer
//! to turn into an image.

use bmux_plugin::RenderOp;
use bmux_tabs_plugin_api::tabs_list::{TabListEntry, TabListSnapshot};
use bmux_tabs_plugin_api::tabs_status::{
    AgentActivity, GitStatus, TabAgent, TabStatus, TabStatusSnapshot,
};
use uuid::Uuid;

use crate::{CompanionState, ExtensionRect, Grouping, Settings, VisibilitySettings, build_surface};

/// A theme: the runtime appearance and the terminal's sixteen colors.
struct Theme {
    name: &'static str,
    appearance: bmux_appearance::RuntimeAppearance,
    ansi: [&'static str; 16],
}

fn themes() -> Vec<Theme> {
    let appearance =
        |fg: &str, bg: &str, cursor: &str, selection: &str, active: &str, inactive: &str| {
            let mut appearance = bmux_appearance::RuntimeAppearance {
                foreground: fg.into(),
                background: bg.into(),
                cursor: cursor.into(),
                selection_background: selection.into(),
                ..bmux_appearance::RuntimeAppearance::default()
            };
            appearance.border.active = active.into();
            appearance.border.inactive = inactive.into();
            appearance
        };
    vec![
        // Tokyo Night.
        Theme {
            name: "dark",
            appearance: appearance(
                "#c0caf5", "#1a1b26", "#7aa2f7", "#33467c", "#7aa2f7", "#3b4261",
            ),
            ansi: [
                "#15161e", "#f7768e", "#9ece6a", "#e0af68", "#7aa2f7", "#bb9af7", "#7dcfff",
                "#a9b1d6", "#414868", "#f7768e", "#9ece6a", "#e0af68", "#7aa2f7", "#bb9af7",
                "#7dcfff", "#c0caf5",
            ],
        },
        // GitHub Light.
        Theme {
            name: "light",
            appearance: appearance(
                "#1f2328", "#ffffff", "#0969da", "#b6e3ff", "#0969da", "#d0d7de",
            ),
            ansi: [
                "#24292f", "#cf222e", "#116329", "#4d2d00", "#0969da", "#8250df", "#1b7c83",
                "#6e7781", "#57606a", "#a40e26", "#1a7f37", "#633c01", "#218bff", "#a475f9",
                "#3192aa", "#8c959f",
            ],
        },
    ]
}

fn shown(grouping: Grouping) -> Settings {
    Settings {
        grouping,
        visibility: VisibilitySettings {
            visible: true,
            ..VisibilitySettings::default()
        },
        ..Settings::default()
    }
}

/// Eight tabs in three workspaces, doing everything a tab can be doing.
fn fixture(settings: Settings, width: u16, height: u16) -> CompanionState {
    let mut state = CompanionState::new(settings, None);
    state.allocation = Some(ExtensionRect::new(0, 0, width, height));
    state.activity.replace(statuses());
    let tab = |index: u128, name: &str, workspace: &str, workspace_id: u128| TabListEntry {
        id: Uuid::from_u128(index),
        name: name.into(),
        active: index == 1,
        workspace: workspace.into(),
        workspace_id: Uuid::from_u128(workspace_id),
    };
    state.replace_tabs(TabListSnapshot {
        tabs: vec![
            tab(1, "sidebar", "bmux", 100),
            tab(2, "review", "bmux", 100),
            tab(3, "tests", "bmux", 100),
            tab(4, "docs", "bmux", 100),
            tab(5, "triage", "bcode", 200),
            tab(6, "feedback 日本語", "bcode", 200),
            tab(7, "todo", "notes", 300),
            tab(8, "top", "notes", 300),
        ],
        revision: 1,
    });
    state
}

/// What the fixture's tabs are doing.
fn statuses() -> TabStatusSnapshot {
    let now = 3_600_000;
    let agent = |activity, message: Option<&str>, ago_ms: u64| TabAgent {
        name: "claude".into(),
        activity,
        message: message.map(Into::into),
        changed_at_ms: now - ago_ms,
        finished_at_ms: None,
    };
    let git = |branch: &str, dirty, ahead, behind| GitStatus {
        branch: branch.into(),
        dirty,
        ahead,
        behind,
    };
    let status = |index: u128,
                  command: Option<&str>,
                  cwd: &str,
                  git: Option<GitStatus>,
                  exit: Option<u32>,
                  agent: Option<TabAgent>| TabStatus {
        tab_id: Uuid::from_u128(index),
        state: bmux_tabs_plugin_api::tabs_status::TabState::Idle,
        state_since_ms: None,
        panes: 1,
        running_command: command.map(Into::into),
        cwd: Some(cwd.into()),
        title: None,
        git,
        last_exit: exit,
        output_at_ms: None,
        attention_at_ms: None,
        attention: None,
        agent,
    };
    TabStatusSnapshot {
        tabs: vec![
            status(
                1,
                Some("claude"),
                "/work/bmux/plugins/sidebar-plugin",
                Some(git("sidebar-polish", true, 2, 0)),
                None,
                Some(agent(AgentActivity::Working, None, 194_000)),
            ),
            status(
                2,
                Some("claude"),
                "/work/bmux",
                Some(git("main", false, 0, 0)),
                None,
                Some(agent(
                    AgentActivity::Blocked,
                    Some("approve edit to lib.rs?"),
                    41_000,
                )),
            ),
            status(
                3,
                None,
                "/work/bmux",
                Some(git("main", false, 0, 3)),
                Some(101),
                None,
            ),
            status(
                4,
                Some("nvim"),
                "/work/bmux/docs",
                Some(git("main", true, 0, 0)),
                None,
                None,
            ),
            status(
                5,
                Some("codex"),
                "/work/bcode",
                Some(git("feedback-triage", false, 1, 0)),
                None,
                Some(agent(AgentActivity::Idle, None, 720_000)),
            ),
            status(
                6,
                Some("claude"),
                "/work/bcode/packages/feedback",
                Some(git("feedback-triage", true, 0, 0)),
                None,
                Some(agent(AgentActivity::Working, None, 12_000)),
            ),
            status(7, None, "/notes", None, None, None),
            status(8, Some("htop"), "/", None, None, None),
        ],
        now_ms: now,
        revision: 1,
    }
}

fn color_json(color: Option<bmux_plugin::RenderColor>) -> serde_json::Value {
    serde_json::to_value(color).unwrap_or(serde_json::Value::Null)
}

#[test]
#[ignore = "writes pictures for review; run with BMUX_SIDEBAR_PREVIEW_DIR"]
fn preview() {
    let Some(dir) = std::env::var_os("BMUX_SIDEBAR_PREVIEW_DIR") else {
        return;
    };
    let dir = std::path::PathBuf::from(dir);
    std::fs::create_dir_all(&dir).unwrap();
    let pictures: Vec<(&str, Settings, u16, u16, i64, bool)> = vec![
        ("grouped-24", shown(Grouping::Workspace), 24, 24, 0, false),
        ("grouped-32", shown(Grouping::Workspace), 32, 24, 0, false),
        ("grouped-44", shown(Grouping::Workspace), 44, 24, 0, false),
        ("flat-32", shown(Grouping::None), 32, 24, 0, false),
        ("scrolled-32", shown(Grouping::Workspace), 32, 12, 5, false),
        ("collapsed-32", shown(Grouping::Workspace), 32, 24, 0, true),
        (
            "compact-32",
            Settings {
                density: crate::Density::Compact,
                ..shown(Grouping::Workspace)
            },
            32,
            24,
            0,
            false,
        ),
        ("narrow-16", shown(Grouping::Workspace), 16, 24, 0, false),
    ];
    for theme in themes() {
        for (label, settings, width, height, scroll, collapse) in &pictures {
            let mut state = fixture(settings.clone(), *width, *height);
            state.apply_appearance(&theme.appearance);
            if *collapse {
                state.toggle_group(Uuid::from_u128(200));
            }
            if *scroll > 0 {
                state.scroll_rows(*scroll);
            }
            let surface = build_surface(&state, 1);
            let runs: Vec<_> = surface
                .ops
                .iter()
                .filter_map(|op| match op {
                    RenderOp::TextRun { x, y, text, style } => Some(serde_json::json!({
                        "x": x, "y": y, "text": text,
                        "fg": color_json(style.fg), "bg": color_json(style.bg),
                        "bold": style.bold, "dim": style.dim,
                        "italic": style.italic, "underline": style.underline,
                        "reverse": style.reverse,
                    })),
                    _ => None,
                })
                .collect();
            let picture = serde_json::json!({
                "width": width,
                "height": height,
                "foreground": theme.appearance.foreground,
                "background": theme.appearance.background,
                "ansi": theme.ansi,
                "runs": runs,
            });
            let name = format!("{}-{label}.json", theme.name);
            std::fs::write(dir.join(name), serde_json::to_vec(&picture).unwrap()).unwrap();
        }
    }
}
