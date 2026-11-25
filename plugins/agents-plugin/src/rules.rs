//! Screen rules: what an agent's screen says it is doing.
//!
//! A rule file describes one agent (its id and the other names it runs as)
//! and rules over regions of its pane: the visible rows, the title (OSC 0/2)
//! and the progress report (OSC 9;4). The highest-priority rule that matches
//! decides the state. The format and regions are those of herdr's detection
//! manifests (<https://github.com/herdrdev/herdr>, Apache-2.0), so its
//! rule files run unchanged. See `rules/NOTICE` for bundled rule provenance.

use std::collections::HashMap;

use regex::Regex;
use serde::Deserialize;

/// The newest rule format this engine reads.
const ENGINE_VERSION: u32 = 3;
/// The first format with `top_non_empty_lines(N)`.
const TOP_NON_EMPTY_LINES_VERSION: u32 = 3;
/// Bounds that keep a user's rule file from making evaluation expensive.
const MAX_RULES: usize = 128;
const MAX_GATE_DEPTH: usize = 8;
const MAX_PATTERNS: usize = 512;
const MAX_REGEX_SIZE: usize = 1 << 20;

/// What a rule says the agent is doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuleState {
    Idle,
    Working,
    Blocked,
    /// Matched, but says nothing about the state (a viewer or a menu).
    Unknown,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct RuleFile {
    id: String,
    #[serde(default)]
    #[allow(dead_code)] // Read for validation only: files carry it for their own updates.
    version: Option<String>,
    min_engine_version: Option<u32>,
    #[serde(default)]
    #[allow(dead_code)]
    updated_at: Option<String>,
    #[serde(default)]
    aliases: Vec<String>,
    #[serde(default)]
    rules: Vec<RuleSpec>,
}

// The flags are the rule file format's own (herdr's manifests).
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct RuleSpec {
    id: String,
    state: Option<RuleState>,
    #[serde(default)]
    priority: i32,
    #[serde(default = "whole_recent")]
    region: String,
    #[serde(default)]
    visible_idle: bool,
    #[serde(default)]
    visible_blocker: bool,
    #[serde(default)]
    visible_working: bool,
    #[serde(default)]
    background_working: bool,
    #[serde(default)]
    skip_state_update: bool,
    #[serde(default)]
    all: Vec<GateSpec>,
    #[serde(default)]
    any: Vec<GateSpec>,
    #[serde(default, rename = "not")]
    not: Vec<GateSpec>,
    #[serde(default)]
    contains: Vec<String>,
    #[serde(default)]
    regex: Vec<String>,
    #[serde(default)]
    line_regex: Vec<String>,
}

impl RuleSpec {
    /// A rule is a gate with a state (serde cannot flatten into a struct
    /// that refuses unknown fields).
    fn gate(&self) -> GateSpec {
        GateSpec {
            all: self.all.clone(),
            any: self.any.clone(),
            not: self.not.clone(),
            contains: self.contains.clone(),
            regex: self.regex.clone(),
            line_regex: self.line_regex.clone(),
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct GateSpec {
    #[serde(default)]
    all: Vec<Self>,
    #[serde(default)]
    any: Vec<Self>,
    #[serde(default, rename = "not")]
    not: Vec<Self>,
    #[serde(default)]
    contains: Vec<String>,
    #[serde(default)]
    regex: Vec<String>,
    #[serde(default)]
    line_regex: Vec<String>,
}

fn whole_recent() -> String {
    "whole_recent".to_string()
}

/// A gate every part of which must hold: all `contains` (case-insensitive),
/// `regex` and `line_regex` (matching some line), all `all`, one of `any`
/// when given, and none of `not`.
#[derive(Debug, Clone)]
struct Gate {
    all: Vec<Self>,
    any: Vec<Self>,
    not: Vec<Self>,
    contains: Vec<String>,
    regex: Vec<Regex>,
    line_regex: Vec<Regex>,
}

impl Gate {
    fn compile(spec: &GateSpec, depth: usize, patterns: &mut usize) -> Result<Self, String> {
        if depth > MAX_GATE_DEPTH {
            return Err(format!("gates nest deeper than {MAX_GATE_DEPTH}"));
        }
        *patterns += spec.contains.len() + spec.regex.len() + spec.line_regex.len();
        if *patterns > MAX_PATTERNS {
            return Err(format!("more than {MAX_PATTERNS} patterns"));
        }
        let regexes = |patterns: &[String]| {
            patterns
                .iter()
                .map(|pattern| {
                    regex::RegexBuilder::new(pattern)
                        .size_limit(MAX_REGEX_SIZE)
                        .build()
                        .map_err(|error| format!("regex `{pattern}`: {error}"))
                })
                .collect::<Result<Vec<_>, _>>()
        };
        let nested = |gates: &[GateSpec], patterns: &mut usize| {
            gates
                .iter()
                .map(|gate| Self::compile(gate, depth + 1, patterns))
                .collect::<Result<Vec<_>, _>>()
        };
        Ok(Self {
            all: nested(&spec.all, patterns)?,
            any: nested(&spec.any, patterns)?,
            not: nested(&spec.not, patterns)?,
            contains: spec
                .contains
                .iter()
                .map(|text| text.to_lowercase())
                .collect(),
            regex: regexes(&spec.regex)?,
            line_regex: regexes(&spec.line_regex)?,
        })
    }

    fn has_positive_matcher(&self) -> bool {
        !self.contains.is_empty()
            || !self.regex.is_empty()
            || !self.line_regex.is_empty()
            || self.all.iter().any(Self::has_positive_matcher)
            || self.any.iter().any(Self::has_positive_matcher)
    }

    fn matches(&self, text: &str, lower: &str) -> bool {
        self.contains.iter().all(|needle| lower.contains(needle))
            && self.regex.iter().all(|regex| regex.is_match(text))
            && self
                .line_regex
                .iter()
                .all(|regex| text.lines().any(|line| regex.is_match(line)))
            && self.all.iter().all(|gate| gate.matches(text, lower))
            && (self.any.is_empty() || self.any.iter().any(|gate| gate.matches(text, lower)))
            && !self.not.iter().any(|gate| gate.matches(text, lower))
    }
}

#[derive(Debug, Clone)]
struct Rule {
    id: String,
    state: RuleState,
    priority: i32,
    region: Region,
    visible: bool,
    background_working: bool,
    skip_state_update: bool,
    gate: Gate,
}

/// Where on the agent's screen (or in its OSC reports) a rule looks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Region {
    WholeRecent,
    OscTitle,
    OscProgress,
    BottomLines(usize),
    BottomNonEmptyLines(usize),
    TopNonEmptyLines(usize),
    AfterLastPromptMarker,
    BeforeCurrentPromptMarker,
    WholeRecentWithoutCurrentPromptMarker,
    CurrentPromptBlockMarker,
    AfterCurrentPromptBlockMarker,
    PromptBoxBody,
    AbovePromptBox,
    LastNonEmptyAbovePromptBox,
    AfterLastHorizontalRule,
}

/// Where a rule's evidence comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Channel {
    /// The terminal title (OSC 0/2).
    Title,
    /// The progress report (OSC 9;4).
    Progress,
    /// The visible rows.
    Screen,
}

impl Region {
    const fn channel(self) -> Channel {
        match self {
            Self::OscTitle => Channel::Title,
            Self::OscProgress => Channel::Progress,
            _ => Channel::Screen,
        }
    }

    fn parse(spec: &str, engine_version: u32) -> Result<Self, String> {
        let spec = spec.trim();
        let count = |name: &str| {
            spec.strip_prefix(name)?
                .strip_prefix('(')?
                .strip_suffix(')')
                .filter(|count| !count.starts_with('0'))?
                .parse::<u16>()
                .ok()
                .map(usize::from)
        };
        Ok(match spec {
            "whole_recent" => Self::WholeRecent,
            "osc_title" => Self::OscTitle,
            "osc_progress" => Self::OscProgress,
            "after_last_prompt_marker" => Self::AfterLastPromptMarker,
            "before_current_prompt_marker" => Self::BeforeCurrentPromptMarker,
            "whole_recent_without_current_prompt_marker" => {
                Self::WholeRecentWithoutCurrentPromptMarker
            }
            "current_prompt_block_marker" => Self::CurrentPromptBlockMarker,
            "after_current_prompt_block_marker" => Self::AfterCurrentPromptBlockMarker,
            "prompt_box_body" => Self::PromptBoxBody,
            "above_prompt_box" => Self::AbovePromptBox,
            "last_non_empty_above_prompt_box" => Self::LastNonEmptyAbovePromptBox,
            "after_last_horizontal_rule" => Self::AfterLastHorizontalRule,
            _ => {
                if let Some(count) = count("bottom_lines") {
                    Self::BottomLines(count)
                } else if let Some(count) = count("bottom_non_empty_lines") {
                    Self::BottomNonEmptyLines(count)
                } else if let Some(count) = count("top_non_empty_lines") {
                    if engine_version < TOP_NON_EMPTY_LINES_VERSION {
                        return Err(format!(
                            "`{spec}` needs min_engine_version {TOP_NON_EMPTY_LINES_VERSION}"
                        ));
                    }
                    Self::TopNonEmptyLines(count)
                } else {
                    return Err(format!("unknown region `{spec}`"));
                }
            }
        })
    }

    fn text<'a>(self, input: &Input<'a>) -> &'a str {
        let screen = input.screen;
        match self {
            Self::WholeRecent => screen,
            Self::OscTitle => input.title,
            Self::OscProgress => input.progress,
            Self::BottomLines(count) => bottom_lines(screen, count),
            Self::BottomNonEmptyLines(count) => bottom_non_empty_lines(screen, count),
            Self::TopNonEmptyLines(count) => top_non_empty_lines(screen, count),
            Self::AfterLastPromptMarker => after_last_prompt_marker(screen),
            Self::BeforeCurrentPromptMarker => before_current_prompt_marker(screen),
            Self::WholeRecentWithoutCurrentPromptMarker => {
                if current_prompt_index(&screen.lines().collect::<Vec<_>>()).is_some() {
                    ""
                } else {
                    screen
                }
            }
            Self::CurrentPromptBlockMarker => current_prompt_block_marker(screen).unwrap_or(""),
            Self::AfterCurrentPromptBlockMarker => {
                after_current_prompt_block_marker(screen).unwrap_or("")
            }
            Self::PromptBoxBody => prompt_box_body(screen).unwrap_or(""),
            Self::AbovePromptBox => above_prompt_box(screen),
            Self::LastNonEmptyAbovePromptBox => last_non_empty_line(above_prompt_box(screen)),
            Self::AfterLastHorizontalRule => after_last_horizontal_rule(screen),
        }
    }
}

/// What rules read: the pane's visible rows, its title and its progress
/// report (`4;…`, empty without one).
#[derive(Debug, Clone, Copy, Default)]
pub struct Input<'a> {
    pub screen: &'a str,
    pub title: &'a str,
    pub progress: &'a str,
}

/// The rule that decided a screen's state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verdict {
    pub rule: String,
    pub state: RuleState,
    /// The screen shows it plainly (a prompt, a spinner, a dialog), which
    /// outranks what an agent last reported.
    pub visible: bool,
    /// This is live background work and can coexist with an idle foreground.
    pub background_working: bool,
    /// Live shell count captured by a background-working screen rule.
    pub background_shell_count: Option<u32>,
    /// The screen shows something that says nothing about the state (a
    /// transcript viewer): keep the previous state.
    pub skip: bool,
}

/// One agent's rules.
#[derive(Debug, Clone)]
pub struct Agent {
    pub id: String,
    pub aliases: Vec<String>,
    rules: Vec<Rule>,
}

impl Agent {
    /// Parse and compile a rule file.
    ///
    /// # Errors
    /// Returns what makes the file unreadable: TOML, an unknown field or
    /// region, a bad regex, a newer format, or a bound exceeded.
    pub fn parse(source: &str) -> Result<Self, String> {
        let file: RuleFile = toml::from_str(source).map_err(|error| error.to_string())?;
        let engine_version = file.min_engine_version.unwrap_or(1);
        if engine_version > ENGINE_VERSION {
            return Err(format!(
                "{} needs rule engine {engine_version}; this one reads {ENGINE_VERSION}",
                file.id
            ));
        }
        if file.id.trim().is_empty() {
            return Err("rule file has no id".into());
        }
        if file.rules.len() > MAX_RULES {
            return Err(format!("{}: more than {MAX_RULES} rules", file.id));
        }
        let mut patterns = 0;
        let rules = file
            .rules
            .iter()
            .map(|spec| {
                let context = |error: String| format!("{} rule {}: {error}", file.id, spec.id);
                let gate = Gate::compile(&spec.gate(), 0, &mut patterns).map_err(context)?;
                if !gate.has_positive_matcher() {
                    return Err(context("matches everything".into()));
                }
                let state = spec.state.unwrap_or(RuleState::Unknown);
                Ok(Rule {
                    id: spec.id.clone(),
                    state,
                    priority: spec.priority,
                    region: Region::parse(&spec.region, engine_version).map_err(context)?,
                    visible: match state {
                        RuleState::Idle => spec.visible_idle,
                        RuleState::Working => spec.visible_working,
                        RuleState::Blocked => spec.visible_blocker,
                        RuleState::Unknown => false,
                    },
                    background_working: state == RuleState::Working && spec.background_working,
                    skip_state_update: spec.skip_state_update,
                    gate,
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self {
            id: file.id,
            aliases: file.aliases,
            rules,
        })
    }

    /// The state all channels together show, by the highest-priority
    /// matching rule (the earlier on a tie); `None` when none matches.
    #[cfg(test)]
    #[must_use]
    pub fn evaluate(&self, input: &Input<'_>) -> Option<Verdict> {
        self.evaluate_where(input, |_| true)
    }

    /// What one channel alone says, by its highest-priority matching rule:
    /// the title, the progress report, or the screen. Channels are read
    /// separately so one cannot hide what another shows.
    #[must_use]
    pub fn evaluate_channel(&self, input: &Input<'_>, channel: Channel) -> Option<Verdict> {
        self.evaluate_where(input, |region| region.channel() == channel)
    }

    fn evaluate_where(&self, input: &Input<'_>, admit: impl Fn(Region) -> bool) -> Option<Verdict> {
        let mut lowered = HashMap::<Region, String>::new();
        let mut best: Option<&Rule> = None;
        for rule in self.rules.iter().filter(|rule| admit(rule.region)) {
            if best.is_some_and(|best| best.priority >= rule.priority) {
                continue;
            }
            let text = rule.region.text(input);
            let lower = lowered
                .entry(rule.region)
                .or_insert_with(|| text.to_lowercase());
            if rule.gate.matches(text, lower) {
                best = Some(rule);
            }
        }
        best.map(|rule| Verdict {
            rule: rule.id.clone(),
            state: rule.state,
            visible: rule.visible,
            background_working: rule.background_working,
            background_shell_count: rule
                .background_working
                .then(|| shell_count(rule, input))
                .flatten(),
            skip: rule.skip_state_update || rule.state == RuleState::Unknown,
        })
    }
}

/// The capture group a background rule names to report its live shell count.
const SHELL_COUNT: &str = "shell_count";

/// The live shell count `rule`'s regexes capture from its region of `input`,
/// when one of them names [`SHELL_COUNT`].
fn shell_count(rule: &Rule, input: &Input<'_>) -> Option<u32> {
    let text = rule.region.text(input);
    rule.gate
        .regex
        .iter()
        .filter(|regex| {
            regex
                .capture_names()
                .flatten()
                .any(|name| name == SHELL_COUNT)
        })
        .find_map(|regex| {
            regex
                .captures(text)?
                .name(SHELL_COUNT)?
                .as_str()
                .parse()
                .ok()
        })
}

fn lines_of(content: &str) -> Vec<&str> {
    content.lines().collect()
}

fn line_start(content: &str, lines: &[&str], index: usize) -> usize {
    lines[..index.min(lines.len())]
        .iter()
        .map(|line| line.len() + 1)
        .sum::<usize>()
        .min(content.len())
}

fn from_line<'a>(content: &'a str, lines: &[&str], index: usize) -> &'a str {
    &content[line_start(content, lines, index)..]
}

fn bottom_lines(content: &str, count: usize) -> &str {
    let lines = lines_of(content);
    from_line(content, &lines, lines.len().saturating_sub(count))
}

fn bottom_non_empty_lines(content: &str, count: usize) -> &str {
    let lines = lines_of(content);
    lines
        .iter()
        .enumerate()
        .rev()
        .filter(|(_, line)| !line.trim().is_empty())
        .take(count)
        .last()
        .map_or("", |(index, _)| from_line(content, &lines, index))
}

fn top_non_empty_lines(content: &str, count: usize) -> &str {
    let lines = lines_of(content);
    lines
        .iter()
        .enumerate()
        .filter(|(_, line)| !line.trim().is_empty())
        .take(count)
        .last()
        .map_or("", |(index, _)| {
            &content[..line_start(content, &lines, index + 1)]
        })
}

/// Codex's prompt line.
fn is_prompt_line(line: &str) -> bool {
    line == "›" || line.starts_with("› ")
}

/// Codex's transcript block markers.
fn is_block_marker(line: &str) -> bool {
    line.starts_with(['•', '■', '✗', '✓'])
}

/// The prompt line with no transcript block after it.
fn current_prompt_index(lines: &[&str]) -> Option<usize> {
    let prompt = lines.iter().rposition(|line| is_prompt_line(line))?;
    (!lines[prompt + 1..].iter().any(|line| is_block_marker(line))).then_some(prompt)
}

fn after_last_prompt_marker(content: &str) -> &str {
    let lines = lines_of(content);
    lines
        .iter()
        .rposition(|line| is_prompt_line(line))
        .map_or(content, |index| from_line(content, &lines, index + 1))
}

fn before_current_prompt_marker(content: &str) -> &str {
    let lines = lines_of(content);
    current_prompt_index(&lines).map_or(content, |index| {
        &content[..line_start(content, &lines, index)]
    })
}

fn current_prompt_block_marker(content: &str) -> Option<&str> {
    let lines = lines_of(content);
    let prompt = current_prompt_index(&lines)?;
    lines[..prompt]
        .iter()
        .rev()
        .find(|line| is_block_marker(line))
        .copied()
}

fn after_current_prompt_block_marker(content: &str) -> Option<&str> {
    let lines = lines_of(content);
    let prompt = current_prompt_index(&lines)?;
    let block = lines[..prompt]
        .iter()
        .rposition(|line| is_block_marker(line))?;
    Some(from_line(content, &lines, block))
}

/// A line of `─` (three or more when followed by text, as a titled rule).
fn is_horizontal_rule(line: &str) -> bool {
    let trimmed = line.trim();
    let rule = trimmed.chars().take_while(|&ch| ch == '─').count();
    if rule == 0 {
        return false;
    }
    let rest = trimmed[trimmed
        .char_indices()
        .nth(rule)
        .map_or(trimmed.len(), |(index, _)| index)..]
        .trim_start();
    rest.is_empty() || rule >= 3
}

/// The second-to-last horizontal rule: the top border of a prompt box drawn
/// with rules above and below it.
fn prompt_box_top(lines: &[&str]) -> Option<usize> {
    lines
        .iter()
        .enumerate()
        .rev()
        .filter(|(_, line)| is_horizontal_rule(line))
        .nth(1)
        .map(|(index, _)| index)
}

fn prompt_box_body(content: &str) -> Option<&str> {
    let lines = lines_of(content);
    let top = prompt_box_top(&lines)?;
    let end = lines[top + 1..]
        .iter()
        .position(|line| is_horizontal_rule(line))
        .map_or(lines.len(), |relative| top + 1 + relative);
    Some(&content[line_start(content, &lines, top + 1)..line_start(content, &lines, end)])
}

fn above_prompt_box(content: &str) -> &str {
    let lines = lines_of(content);
    prompt_box_top(&lines).map_or(content, |top| &content[..line_start(content, &lines, top)])
}

fn after_last_horizontal_rule(content: &str) -> &str {
    let mut start = 0;
    let mut offset = 0;
    for line in content.lines() {
        offset += line.len() + 1;
        if is_horizontal_rule(line) {
            start = offset.min(content.len());
        }
    }
    &content[start..]
}

fn last_non_empty_line(content: &str) -> &str {
    content
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("")
}

/// The rule files bmux ships (herdr's; see `rules/NOTICE`).
pub const BUNDLED: &[(&str, &str)] = &[
    ("amp", include_str!("../rules/amp.toml")),
    ("antigravity", include_str!("../rules/antigravity.toml")),
    ("bcode", include_str!("../rules/bcode.toml")),
    ("claude", include_str!("../rules/claude.toml")),
    ("cline", include_str!("../rules/cline.toml")),
    ("codex", include_str!("../rules/codex.toml")),
    ("cursor", include_str!("../rules/cursor.toml")),
    ("devin", include_str!("../rules/devin.toml")),
    ("droid", include_str!("../rules/droid.toml")),
    ("gemini", include_str!("../rules/gemini.toml")),
    (
        "github-copilot",
        include_str!("../rules/github-copilot.toml"),
    ),
    ("grok", include_str!("../rules/grok.toml")),
    ("hermes", include_str!("../rules/hermes.toml")),
    ("kilo", include_str!("../rules/kilo.toml")),
    ("kimi", include_str!("../rules/kimi.toml")),
    ("kiro", include_str!("../rules/kiro.toml")),
    ("letta", include_str!("../rules/letta.toml")),
    ("maki", include_str!("../rules/maki.toml")),
    ("muse", include_str!("../rules/muse.toml")),
    ("opencode", include_str!("../rules/opencode.toml")),
    ("pi", include_str!("../rules/pi.toml")),
    ("qodercli", include_str!("../rules/qodercli.toml")),
    ("qwen", include_str!("../rules/qwen.toml")),
];

#[cfg(test)]
mod tests {
    use super::*;

    fn agent(id: &str) -> Agent {
        let source = BUNDLED
            .iter()
            .find(|(name, _)| *name == id)
            .map(|(_, source)| *source)
            .unwrap();
        Agent::parse(source).unwrap()
    }

    fn screen(agent: &Agent, screen: &str) -> Option<(RuleState, String)> {
        agent
            .evaluate(&Input {
                screen,
                ..Input::default()
            })
            .map(|verdict| (verdict.state, verdict.rule))
    }

    #[test]
    fn every_bundled_rule_file_compiles() {
        for (name, source) in BUNDLED {
            let agent = Agent::parse(source).unwrap_or_else(|error| panic!("{name}: {error}"));
            assert!(!agent.rules.is_empty(), "{name}");
        }
    }

    #[test]
    fn antigravity_counts_running_task_rows_under_the_prompt() {
        let agy = agent("antigravity");
        let footer = "? for shortcuts     auto · Gemini 4 Neon · medium · 1 task(s) · /tasks";
        let running = "  ● [16:47:02] cargo fmt && cargo test running";
        let prompt = "────────────────────\n>\n────────────────────";
        for (text, expected) in [
            (
                format!("{prompt}\n{running}\n{footer}"),
                Some(RuleState::Working),
            ),
            (
                format!("{prompt}\n  ● [16:47:02] cargo fmt && cargo test completed\n{footer}"),
                None,
            ),
            (format!("{running}\n{prompt}\n{footer}"), None),
            (
                format!("{prompt}\n{running}\n↑/↓ Navigate · tab Amend\n{footer}"),
                Some(RuleState::Blocked),
            ),
        ] {
            assert_eq!(
                screen(&agy, &text).map(|(state, _)| state),
                expected,
                "{text}"
            );
        }
    }

    #[test]
    fn bcode_reads_live_activity_and_ignores_stale_transcript_status() {
        let bcode = agent("bcode");
        let composer = "┌ Message ─────┐\n│ Ask Bcode…   │\n└──────────────┘";
        for (body, expected) in [
            (
                "⠋ waiting for astra response · 1.2s · round 1 · New draft",
                RuleState::Working,
            ),
            (
                "ready · 1 background task running · New draft",
                RuleState::Working,
            ),
            (
                "ready · spent 0 · children: 2 known active observed subtree · partial",
                RuleState::Working,
            ),
            (
                "ready · children: 0 known active · complete",
                RuleState::Idle,
            ),
            ("ready · 0 background tasks running", RuleState::Idle),
            (
                "3 background tasks running\nready · New draft",
                RuleState::Idle,
            ),
            (
                "Question\n  [ Submit ]  [ Cancel ]\nready · children: 2 known active · complete",
                RuleState::Blocked,
            ),
            ("◐ Building… (12s)", RuleState::Working),
            (
                "⠙ rate limit; retrying in 30s · cancel to exit",
                RuleState::Working,
            ),
            (
                "⠹ waiting for permission · running shell · 3.2s",
                RuleState::Blocked,
            ),
            (
                "Permission requested: shell.run\n  1. Approve once\n  2. Deny once",
                RuleState::Blocked,
            ),
            (
                "Question\nWhich file should I use?\n  [ Submit ]  [ Cancel ]",
                RuleState::Blocked,
            ),
            (
                "⠋ waiting for astra response · 1.2s\n⠹ waiting for permission · running shell · 3.2s\nready · —/272k —% · New draft",
                RuleState::Idle,
            ),
        ] {
            let rendered = format!("{body}\n{composer}");
            assert_eq!(
                screen(&bcode, &rendered).map(|(state, _)| state),
                Some(expected),
                "{body}"
            );
        }
        assert_eq!(
            screen(
                &bcode,
                &format!(
                    "Example: Permission requested: shell.run\nMentioning Question and Submit\n{composer}"
                )
            ),
            None
        );
    }

    #[test]
    fn claude_screens_read_as_working_blocked_and_idle() {
        let claude = agent("claude");
        let working = "> fix the bug\n\n✻ Pondering… (3s · ↑ 1.2k tokens · esc to interrupt)\n\n─────\n❯ \n─────\n  ⏵⏵ accept edits on (shift+tab to cycle)";
        assert_eq!(
            screen(&claude, working).map(|(state, _)| state),
            Some(RuleState::Working)
        );
        let blocked = "● Bash(rm -rf build)\n\n─────\n Bash command\n\n   rm -rf build\n\n Do you want to proceed?\n ❯ 1. Yes\n   2. No, and tell Claude what to do differently (esc)\n\n Esc to cancel · Tab to amend";
        assert_eq!(
            screen(&claude, blocked).map(|(state, _)| state),
            Some(RuleState::Blocked)
        );
        let idle = "● Done. The tests pass.\n\n─────────────\n❯ \n─────────────\n  ? for shortcuts";
        assert_eq!(
            screen(&claude, idle),
            Some((RuleState::Idle, "live_prompt_box".into()))
        );
        // The title alone can say it is working.
        let title = claude.evaluate(&Input {
            screen: "",
            title: "⠂ Fixing the bug",
            progress: "",
        });
        assert_eq!(title.map(|verdict| verdict.state), Some(RuleState::Working));
    }

    /// Cursor's background-task count is background work only as a status
    /// row of its own; the same words in the agent's prose near the prompt
    /// are not.
    #[test]
    fn cursor_background_tasks_count_only_as_a_status_row() {
        let cursor = agent("cursor");
        let background = |text: &str| {
            cursor
                .evaluate(&Input {
                    screen: text,
                    ..Input::default()
                })
                .filter(|verdict| verdict.background_working)
                .map(|verdict| verdict.rule)
        };
        for row in [
            "2 background tasks",
            "  1 background task running",
            "⬢ 3 background tasks",
            "Auto · ctrl+r to review · 2 background tasks",
            "2 background tasks · ctrl+b to view",
        ] {
            assert_eq!(
                background(&format!("→ ask anything\n{row}")).as_deref(),
                Some("background_task_status_working"),
                "{row}"
            );
        }
        for prose in [
            "I kicked off 2 background tasks to run the suite.",
            "2 background tasks are still running, so I will wait for them.",
            "The 0 background tasks finished.",
        ] {
            assert_eq!(
                background(&format!("{prose}\n→ ask anything")),
                None,
                "{prose}"
            );
        }
    }

    #[test]
    fn regions_select_what_they_name() {
        let text = "one\n\ntwo\n───\nbox\n───\nafter";
        assert_eq!(bottom_non_empty_lines(text, 2), "───\nafter");
        assert_eq!(bottom_lines(text, 1), "after");
        assert_eq!(top_non_empty_lines(text, 2), "one\n\ntwo\n");
        assert_eq!(prompt_box_body(text), Some("box\n"));
        assert_eq!(above_prompt_box(text), "one\n\ntwo\n");
        assert_eq!(after_last_horizontal_rule(text), "after");
        assert_eq!(last_non_empty_line(above_prompt_box(text)), "two");
        let codex = "• ran tests\n› next";
        assert_eq!(current_prompt_block_marker(codex), Some("• ran tests"));
        assert_eq!(before_current_prompt_marker(codex), "• ran tests\n");
        assert_eq!(after_last_prompt_marker(codex), "");
    }

    #[test]
    fn bad_rule_files_are_refused() {
        for (source, why) in [
            (
                "id = 'x'\n[[rules]]\nid='a'\nstate='idle'\nregion='nowhere'\ncontains=['x']",
                "region",
            ),
            (
                "id = 'x'\n[[rules]]\nid='a'\nstate='idle'",
                "matches everything",
            ),
            (
                "id = 'x'\n[[rules]]\nid='a'\nstate='idle'\nregex=['(']",
                "regex",
            ),
            ("id = 'x'\nmin_engine_version = 9", "rule engine"),
            ("id = 'x'\nsurprise = 1", "unknown field"),
            (
                "id = 'x'\n[[rules]]\nid='a'\nstate='idle'\nregion='top_non_empty_lines(3)'\ncontains=['x']",
                "min_engine_version",
            ),
        ] {
            let error = Agent::parse(source).unwrap_err();
            assert!(error.contains(why), "{why}: {error}");
        }
    }
}
