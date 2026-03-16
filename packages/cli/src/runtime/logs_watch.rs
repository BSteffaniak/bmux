use anyhow::{Context, Result};
use bmux_config::ConfigPaths;
use crossterm::cursor::{Hide, Show};
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal;
use crossterm::terminal::{EnterAlternateScreen, LeaveAlternateScreen};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, Paragraph};
use regex::{Regex, RegexBuilder};
use std::collections::{BTreeMap, VecDeque};
use std::io::{self, Read, Seek, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;
use time::OffsetDateTime;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum LogFilterKind {
    Include,
    Exclude,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum LogFilterCaseMode {
    Sensitive,
    Insensitive,
}

#[derive(Debug, Clone)]
pub(crate) struct LogFilterRule {
    pub(crate) kind: LogFilterKind,
    pub(crate) pattern: String,
    pub(crate) case_mode: LogFilterCaseMode,
    pub(crate) enabled: bool,
    regex: std::result::Result<Regex, String>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default)]
struct LogsWatchStateFile {
    version: u32,
    active_profile: Option<String>,
    #[serde(default)]
    profiles: BTreeMap<String, LogsWatchProfileState>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default)]
struct LogsWatchProfileState {
    #[serde(default)]
    filters: Vec<LogsWatchFilterState>,
    quick_filter: Option<String>,
    since: Option<String>,
    lines: Option<usize>,
    selected_filter_index: Option<usize>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct LogsWatchFilterState {
    pub(crate) kind: LogFilterKind,
    pub(crate) pattern: String,
    pub(crate) case_mode: LogFilterCaseMode,
    pub(crate) enabled: bool,
}

impl LogFilterRule {
    pub(crate) fn new(kind: LogFilterKind, pattern: String, case_mode: LogFilterCaseMode) -> Self {
        let regex = compile_filter_regex(&pattern, case_mode);
        Self {
            kind,
            pattern,
            case_mode,
            enabled: true,
            regex,
        }
    }

    fn matches(&self, line: &str) -> bool {
        if !self.enabled {
            return false;
        }
        self.regex.as_ref().is_ok_and(|regex| regex.is_match(line))
    }

    fn has_error(&self) -> bool {
        self.regex.is_err()
    }

    fn toggle_case_mode(&mut self) {
        self.case_mode = match self.case_mode {
            LogFilterCaseMode::Sensitive => LogFilterCaseMode::Insensitive,
            LogFilterCaseMode::Insensitive => LogFilterCaseMode::Sensitive,
        };
        self.regex = compile_filter_regex(&self.pattern, self.case_mode);
    }
}

struct LogWatchUiGuard;

impl LogWatchUiGuard {
    fn activate() -> Result<Self> {
        enable_raw_mode().context("failed enabling raw mode for logs watch")?;
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen, Hide)
            .context("failed entering alternate screen for logs watch")?;
        Ok(Self)
    }
}

impl Drop for LogWatchUiGuard {
    fn drop(&mut self) {
        let mut stdout = io::stdout();
        let _ = execute!(stdout, Show, LeaveAlternateScreen);
        let _ = disable_raw_mode();
    }
}

pub(crate) fn run_logs_watch(
    lines: Option<usize>,
    since: Option<&str>,
    profile: Option<&str>,
    include: &[String],
    include_i: &[String],
    exclude: &[String],
    exclude_i: &[String],
) -> Result<u8> {
    const WATCH_BUFFER_LIMIT: usize = 20_000;

    let profile_name = normalize_logs_watch_profile(profile)?;
    let mut persisted_profile = load_logs_watch_profile_state(&profile_name).unwrap_or_default();
    let effective_lines = lines.or(persisted_profile.lines).unwrap_or(200);
    let effective_since = since
        .map(ToString::to_string)
        .or_else(|| persisted_profile.since.clone());
    let since_cutoff = match effective_since.as_deref() {
        Some(value) => Some(super::parse_since_cutoff(value)?),
        None => None,
    };

    let mut filters = persisted_profile
        .filters
        .iter()
        .cloned()
        .map(logs_watch_filter_state_to_rule)
        .collect::<Vec<_>>();
    filters.extend(seed_watch_filters(include, include_i, exclude, exclude_i));

    let mut selected_filter = persisted_profile.selected_filter_index.unwrap_or_default();
    if selected_filter >= filters.len() {
        selected_filter = filters.len().saturating_sub(1);
    }
    let mut status_message = String::new();
    let mut quick_filter: Option<String> = persisted_profile.quick_filter.take();
    let mut paused = false;
    let mut auto_follow = true;
    let mut log_cursor = usize::MAX;

    let mut active_path = active_log_file_path();
    let mut entries = VecDeque::new();
    let mut pending_fragment = String::new();
    let mut read_offset = 0_u64;

    if active_path.exists() {
        preload_watch_entries(
            &active_path,
            &mut entries,
            effective_lines,
            since_cutoff,
            WATCH_BUFFER_LIMIT,
        )?;
    }

    let mut log_file = open_log_watch_file(&active_path)?;
    if let Some(file) = log_file.as_mut() {
        read_offset = file
            .metadata()
            .with_context(|| format!("failed reading metadata for {}", active_path.display()))?
            .len();
    }

    let _ui_guard = LogWatchUiGuard::activate()?;
    let mut terminal =
        Terminal::new(CrosstermBackend::new(io::stdout())).context("failed initializing TUI")?;

    let save_state = |filters: &[LogFilterRule],
                      quick_filter: Option<&str>,
                      selected_filter: usize|
     -> Result<()> {
        persist_logs_watch_profile_state(
            &profile_name,
            filters,
            quick_filter,
            effective_since.as_deref(),
            effective_lines,
            selected_filter,
        )
    };

    loop {
        let newest_path = active_log_file_path();
        if newest_path != active_path {
            active_path = newest_path;
            log_file = open_log_watch_file(&active_path)?;
            read_offset = 0;
            pending_fragment.clear();
            status_message = format!("switched to {}", active_path.display());
        }

        if !paused {
            if log_file.is_none() {
                log_file = open_log_watch_file(&active_path)?;
                if log_file.is_some() {
                    status_message = format!("opened {}", active_path.display());
                }
            }

            if let Some(file) = log_file.as_mut()
                && let Some(new_lines) = read_watch_log_delta(
                    file,
                    &active_path,
                    &mut read_offset,
                    &mut pending_fragment,
                )?
            {
                for line in new_lines {
                    if super::line_matches_since(&line, since_cutoff) {
                        entries.push_back(line);
                    }
                }
                while entries.len() > WATCH_BUFFER_LIMIT {
                    let _ = entries.pop_front();
                }
                if auto_follow {
                    log_cursor = usize::MAX;
                }
            }
        }

        render_logs_watch(
            &mut terminal,
            &active_path,
            &entries,
            &filters,
            selected_filter,
            quick_filter.as_deref(),
            paused,
            &profile_name,
            log_cursor,
            auto_follow,
            &status_message,
        )
        .context("failed rendering logs watch ui")?;

        let visible_count = count_visible_watch_lines(&entries, &filters, quick_filter.as_deref());
        if visible_count == 0 {
            log_cursor = 0;
        } else {
            if auto_follow || log_cursor == usize::MAX {
                log_cursor = visible_count.saturating_sub(1);
            }
            log_cursor = log_cursor.min(visible_count.saturating_sub(1));
        }

        if !event::poll(Duration::from_millis(120)).context("failed polling logs watch input")? {
            continue;
        }
        let Event::Key(key) = event::read().context("failed reading logs watch input")? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }

        let viewport = watch_viewport_height();
        match key.code {
            KeyCode::Char('q') => break,
            KeyCode::Char('p') => {
                paused = !paused;
                status_message = if paused {
                    "paused ingest".to_string()
                } else {
                    "resumed ingest".to_string()
                };
            }
            KeyCode::Char('a') => {
                if let Some(pattern) = prompt_logs_watch_line("Add include regex: ")? {
                    filters.push(LogFilterRule::new(
                        LogFilterKind::Include,
                        pattern,
                        LogFilterCaseMode::Sensitive,
                    ));
                    selected_filter = filters.len().saturating_sub(1);
                    if let Err(error) =
                        save_state(&filters, quick_filter.as_deref(), selected_filter)
                    {
                        status_message = format!("failed saving watch state: {error:#}");
                    }
                }
            }
            KeyCode::Char('x') => {
                if let Some(pattern) = prompt_logs_watch_line("Add exclude regex: ")? {
                    filters.push(LogFilterRule::new(
                        LogFilterKind::Exclude,
                        pattern,
                        LogFilterCaseMode::Sensitive,
                    ));
                    selected_filter = filters.len().saturating_sub(1);
                    if let Err(error) =
                        save_state(&filters, quick_filter.as_deref(), selected_filter)
                    {
                        status_message = format!("failed saving watch state: {error:#}");
                    }
                }
            }
            KeyCode::Char('/') => {
                quick_filter = prompt_logs_watch_line("Quick substring filter (empty clears): ")?;
                if let Err(error) = save_state(&filters, quick_filter.as_deref(), selected_filter) {
                    status_message = format!("failed saving watch state: {error:#}");
                }
            }
            KeyCode::Char('c') => {
                filters.clear();
                selected_filter = 0;
                quick_filter = None;
                status_message = "cleared filters".to_string();
                if let Err(error) = save_state(&filters, quick_filter.as_deref(), selected_filter) {
                    status_message = format!("failed saving watch state: {error:#}");
                }
            }
            KeyCode::Char('t') => {
                if let Some(filter) = filters.get_mut(selected_filter) {
                    filter.enabled = !filter.enabled;
                    if let Err(error) =
                        save_state(&filters, quick_filter.as_deref(), selected_filter)
                    {
                        status_message = format!("failed saving watch state: {error:#}");
                    }
                }
            }
            KeyCode::Char('i') => {
                if let Some(filter) = filters.get_mut(selected_filter) {
                    filter.toggle_case_mode();
                    if let Err(error) =
                        save_state(&filters, quick_filter.as_deref(), selected_filter)
                    {
                        status_message = format!("failed saving watch state: {error:#}");
                    }
                }
            }
            KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if visible_count > 0 {
                    auto_follow = false;
                    log_cursor = log_cursor
                        .saturating_add((viewport / 2).max(1))
                        .min(visible_count.saturating_sub(1));
                }
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                auto_follow = false;
                log_cursor = log_cursor.saturating_sub((viewport / 2).max(1));
            }
            KeyCode::Char('d') => {
                if selected_filter < filters.len() {
                    let _ = filters.remove(selected_filter);
                    if selected_filter >= filters.len() {
                        selected_filter = filters.len().saturating_sub(1);
                    }
                    if let Err(error) =
                        save_state(&filters, quick_filter.as_deref(), selected_filter)
                    {
                        status_message = format!("failed saving watch state: {error:#}");
                    }
                }
            }
            KeyCode::Char('j') => {
                if visible_count > 0 {
                    auto_follow = false;
                    log_cursor = log_cursor
                        .saturating_add(1)
                        .min(visible_count.saturating_sub(1));
                }
            }
            KeyCode::Char('k') => {
                auto_follow = false;
                log_cursor = log_cursor.saturating_sub(1);
            }
            KeyCode::Char('g') => {
                auto_follow = false;
                log_cursor = 0;
            }
            KeyCode::Char('G') => {
                auto_follow = true;
                if visible_count > 0 {
                    log_cursor = visible_count.saturating_sub(1);
                }
            }
            KeyCode::PageDown => {
                if visible_count > 0 {
                    auto_follow = false;
                    log_cursor = log_cursor
                        .saturating_add(viewport.max(1))
                        .min(visible_count.saturating_sub(1));
                }
            }
            KeyCode::PageUp => {
                auto_follow = false;
                log_cursor = log_cursor.saturating_sub(viewport.max(1));
            }
            KeyCode::Up => {
                selected_filter = selected_filter.saturating_sub(1);
            }
            KeyCode::Down => {
                if selected_filter + 1 < filters.len() {
                    selected_filter += 1;
                }
            }
            _ => {}
        }
    }

    save_state(&filters, quick_filter.as_deref(), selected_filter)?;

    Ok(0)
}

fn seed_watch_filters(
    include: &[String],
    include_i: &[String],
    exclude: &[String],
    exclude_i: &[String],
) -> Vec<LogFilterRule> {
    let mut filters = Vec::new();
    filters.extend(include.iter().cloned().map(|pattern| {
        LogFilterRule::new(
            LogFilterKind::Include,
            pattern,
            LogFilterCaseMode::Sensitive,
        )
    }));
    filters.extend(include_i.iter().cloned().map(|pattern| {
        LogFilterRule::new(
            LogFilterKind::Include,
            pattern,
            LogFilterCaseMode::Insensitive,
        )
    }));
    filters.extend(exclude.iter().cloned().map(|pattern| {
        LogFilterRule::new(
            LogFilterKind::Exclude,
            pattern,
            LogFilterCaseMode::Sensitive,
        )
    }));
    filters.extend(exclude_i.iter().cloned().map(|pattern| {
        LogFilterRule::new(
            LogFilterKind::Exclude,
            pattern,
            LogFilterCaseMode::Insensitive,
        )
    }));
    filters
}

pub(crate) fn normalize_logs_watch_profile(profile: Option<&str>) -> Result<String> {
    let value = profile.unwrap_or("default").trim();
    if value.is_empty() {
        anyhow::bail!("--profile cannot be empty");
    }
    if value.len() > 64 {
        anyhow::bail!("--profile must be 64 characters or fewer");
    }
    if !value
        .chars()
        .all(|entry| entry.is_ascii_alphanumeric() || entry == '-' || entry == '_')
    {
        anyhow::bail!("--profile may contain only ASCII letters, numbers, '-', and '_'");
    }
    Ok(value.to_string())
}

fn logs_watch_state_file_path() -> PathBuf {
    ConfigPaths::default()
        .state_dir()
        .join("runtime")
        .join("logs-watch-state.json")
}

fn load_logs_watch_profile_state(profile_name: &str) -> Result<LogsWatchProfileState> {
    let state = read_logs_watch_state_file()?;
    Ok(state
        .profiles
        .get(profile_name)
        .cloned()
        .unwrap_or_default())
}

fn persist_logs_watch_profile_state(
    profile_name: &str,
    filters: &[LogFilterRule],
    quick_filter: Option<&str>,
    since: Option<&str>,
    lines: usize,
    selected_filter: usize,
) -> Result<()> {
    let mut state = read_logs_watch_state_file()?;
    state.version = 1;
    state.active_profile = Some(profile_name.to_string());
    state.profiles.insert(
        profile_name.to_string(),
        LogsWatchProfileState {
            filters: filters
                .iter()
                .map(logs_watch_filter_rule_to_state)
                .collect(),
            quick_filter: quick_filter
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(ToString::to_string),
            since: since
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(ToString::to_string),
            lines: Some(lines.max(1)),
            selected_filter_index: Some(selected_filter),
        },
    );
    write_logs_watch_state_file(&state)
}

pub(crate) fn run_logs_profiles_list(as_json: bool) -> Result<u8> {
    let state = read_logs_watch_state_file()?;
    let active_profile = state.active_profile.as_deref().unwrap_or("default");
    let mut profile_names = state.profiles.keys().cloned().collect::<Vec<_>>();
    if !profile_names.iter().any(|name| name == "default") {
        profile_names.push("default".to_string());
    }
    profile_names.sort();

    if as_json {
        let payload = profile_names
            .into_iter()
            .map(|name| {
                let filter_count = state
                    .profiles
                    .get(&name)
                    .map_or(0, |profile| profile.filters.len());
                let is_active = name == active_profile;
                serde_json::json!({
                    "name": name,
                    "active": is_active,
                    "filter_count": filter_count,
                })
            })
            .collect::<Vec<_>>();
        println!(
            "{}",
            serde_json::to_string_pretty(&payload)
                .context("failed encoding logs profiles list json")?
        );
        return Ok(0);
    }

    for name in profile_names {
        let marker = if name == active_profile { "*" } else { " " };
        let filter_count = state
            .profiles
            .get(&name)
            .map_or(0, |profile| profile.filters.len());
        println!("{marker} {name} ({filter_count} filters)");
    }
    Ok(0)
}

pub(crate) fn run_logs_profiles_show(profile: Option<&str>, as_json: bool) -> Result<u8> {
    let profile_name = normalize_logs_watch_profile(profile)?;
    let state = read_logs_watch_state_file()?;
    let profile_state = state
        .profiles
        .get(&profile_name)
        .cloned()
        .unwrap_or_default();

    if as_json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "name": profile_name,
                "active": state.active_profile.as_deref() == Some(profile_name.as_str()),
                "quick_filter": profile_state.quick_filter,
                "since": profile_state.since,
                "lines": profile_state.lines,
                "selected_filter_index": profile_state.selected_filter_index,
                "filters": profile_state.filters,
            }))
            .context("failed encoding logs profile json")?
        );
        return Ok(0);
    }

    println!("profile: {profile_name}");
    println!(
        "active: {}",
        if state.active_profile.as_deref() == Some(profile_name.as_str()) {
            "yes"
        } else {
            "no"
        }
    );
    println!(
        "lines: {}",
        profile_state
            .lines
            .map_or_else(|| "(default)".to_string(), |value| value.to_string())
    );
    println!(
        "since: {}",
        profile_state.since.as_deref().unwrap_or("(none)")
    );
    println!(
        "quick filter: {}",
        profile_state.quick_filter.as_deref().unwrap_or("(none)")
    );
    println!("filters:");
    if profile_state.filters.is_empty() {
        println!("  (none)");
    } else {
        for filter in &profile_state.filters {
            let kind = match filter.kind {
                LogFilterKind::Include => "include",
                LogFilterKind::Exclude => "exclude",
            };
            let case = match filter.case_mode {
                LogFilterCaseMode::Sensitive => "case-sensitive",
                LogFilterCaseMode::Insensitive => "case-insensitive",
            };
            let enabled = if filter.enabled {
                "enabled"
            } else {
                "disabled"
            };
            println!("  - {kind} /{}/ ({case}, {enabled})", filter.pattern);
        }
    }
    Ok(0)
}

pub(crate) fn run_logs_profiles_delete(profile: &str) -> Result<u8> {
    let profile_name = normalize_logs_watch_profile(Some(profile))?;
    if profile_name == "default" {
        anyhow::bail!("cannot delete reserved profile 'default'");
    }

    let mut state = read_logs_watch_state_file()?;
    if state.profiles.remove(&profile_name).is_none() {
        anyhow::bail!("profile '{profile_name}' not found");
    }
    if state.active_profile.as_deref() == Some(profile_name.as_str()) {
        state.active_profile = Some("default".to_string());
    }
    write_logs_watch_state_file(&state)?;
    println!("deleted profile '{profile_name}'");
    Ok(0)
}

pub(crate) fn run_logs_profiles_rename(from: &str, to: &str) -> Result<u8> {
    let from_name = normalize_logs_watch_profile(Some(from))?;
    let to_name = normalize_logs_watch_profile(Some(to))?;
    if from_name == to_name {
        anyhow::bail!("source and destination profile names are the same");
    }

    let mut state = read_logs_watch_state_file()?;
    if state.profiles.contains_key(&to_name) {
        anyhow::bail!("profile '{to_name}' already exists");
    }
    let profile = state
        .profiles
        .remove(&from_name)
        .ok_or_else(|| anyhow::anyhow!("profile '{from_name}' not found"))?;
    state.profiles.insert(to_name.clone(), profile);
    if state.active_profile.as_deref() == Some(from_name.as_str()) {
        state.active_profile = Some(to_name.clone());
    }
    write_logs_watch_state_file(&state)?;
    println!("renamed profile '{from_name}' -> '{to_name}'");
    Ok(0)
}

fn read_logs_watch_state_file() -> Result<LogsWatchStateFile> {
    let path = logs_watch_state_file_path();
    if !path.exists() {
        return Ok(LogsWatchStateFile {
            version: 1,
            ..LogsWatchStateFile::default()
        });
    }
    let content = std::fs::read_to_string(&path)
        .with_context(|| format!("failed reading logs watch state file {}", path.display()))?;
    let mut state: LogsWatchStateFile = serde_json::from_str(&content)
        .with_context(|| format!("failed parsing logs watch state file {}", path.display()))?;
    if state.version == 0 {
        state.version = 1;
    }
    Ok(state)
}

fn write_logs_watch_state_file(state: &LogsWatchStateFile) -> Result<()> {
    let path = logs_watch_state_file_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| {
            format!(
                "failed creating logs watch state directory {}",
                parent.display()
            )
        })?;
    }
    let bytes = serde_json::to_vec_pretty(state).context("failed encoding logs watch state")?;
    let temp_path = path.with_extension(format!("tmp-{}", std::process::id()));
    std::fs::write(&temp_path, bytes).with_context(|| {
        format!(
            "failed writing temporary logs watch state file {}",
            temp_path.display()
        )
    })?;
    std::fs::rename(&temp_path, &path)
        .with_context(|| format!("failed finalizing logs watch state file {}", path.display()))?;
    Ok(())
}

pub(crate) fn logs_watch_filter_rule_to_state(rule: &LogFilterRule) -> LogsWatchFilterState {
    LogsWatchFilterState {
        kind: rule.kind,
        pattern: rule.pattern.clone(),
        case_mode: rule.case_mode,
        enabled: rule.enabled,
    }
}

pub(crate) fn logs_watch_filter_state_to_rule(state: LogsWatchFilterState) -> LogFilterRule {
    let mut rule = LogFilterRule::new(state.kind, state.pattern, state.case_mode);
    rule.enabled = state.enabled;
    rule
}

pub(crate) fn compile_filter_regex(
    pattern: &str,
    case_mode: LogFilterCaseMode,
) -> std::result::Result<Regex, String> {
    let mut builder = RegexBuilder::new(pattern);
    builder.unicode(false);
    if matches!(case_mode, LogFilterCaseMode::Insensitive) {
        builder.case_insensitive(true);
    }
    builder.build().map_err(|error| error.to_string())
}

fn open_log_watch_file(path: &Path) -> Result<Option<std::fs::File>> {
    if !path.exists() {
        return Ok(None);
    }
    let file = std::fs::OpenOptions::new()
        .read(true)
        .open(path)
        .with_context(|| format!("failed opening log file {}", path.display()))?;
    Ok(Some(file))
}

fn preload_watch_entries(
    path: &Path,
    entries: &mut VecDeque<String>,
    lines: usize,
    since_cutoff: Option<OffsetDateTime>,
    max_entries: usize,
) -> Result<()> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("failed reading log file {}", path.display()))?;
    for line in content.lines() {
        if super::line_matches_since(line, since_cutoff) {
            entries.push_back(line.to_string());
        }
    }
    while entries.len() > max_entries {
        let _ = entries.pop_front();
    }
    if entries.len() > lines {
        let drop = entries.len().saturating_sub(lines);
        for _ in 0..drop {
            let _ = entries.pop_front();
        }
    }
    Ok(())
}

fn read_watch_log_delta(
    file: &mut std::fs::File,
    path: &Path,
    read_offset: &mut u64,
    pending_fragment: &mut String,
) -> Result<Option<Vec<String>>> {
    let metadata = file
        .metadata()
        .with_context(|| format!("failed reading metadata for {}", path.display()))?;
    let file_len = metadata.len();
    if file_len < *read_offset {
        *read_offset = 0;
    }
    if file_len == *read_offset {
        return Ok(None);
    }

    file.seek(std::io::SeekFrom::Start(*read_offset))
        .with_context(|| format!("failed seeking {}", path.display()))?;
    let mut chunk = String::new();
    file.read_to_string(&mut chunk)
        .with_context(|| format!("failed reading appended logs from {}", path.display()))?;
    *read_offset = file_len;

    pending_fragment.push_str(&chunk);
    let mut complete = Vec::new();
    let ends_with_newline = pending_fragment.ends_with('\n');
    for segment in pending_fragment.split('\n') {
        complete.push(segment.to_string());
    }
    if !ends_with_newline {
        let last = complete.pop().unwrap_or_default();
        *pending_fragment = last;
    } else {
        pending_fragment.clear();
    }
    if let Some(last) = complete.last()
        && last.is_empty()
    {
        let _ = complete.pop();
    }
    Ok(Some(complete))
}

pub(crate) fn line_visible_in_watch(
    line: &str,
    filters: &[LogFilterRule],
    quick_filter: Option<&str>,
) -> bool {
    if let Some(quick) = quick_filter
        && !quick.is_empty()
        && !line.contains(quick)
    {
        return false;
    }

    let include_filters = filters
        .iter()
        .filter(|rule| rule.enabled && matches!(rule.kind, LogFilterKind::Include))
        .collect::<Vec<_>>();
    if !include_filters.is_empty() && !include_filters.iter().any(|rule| rule.matches(line)) {
        return false;
    }

    !filters.iter().any(|rule| {
        rule.enabled && matches!(rule.kind, LogFilterKind::Exclude) && rule.matches(line)
    })
}

fn count_visible_watch_lines(
    entries: &VecDeque<String>,
    filters: &[LogFilterRule],
    quick_filter: Option<&str>,
) -> usize {
    entries
        .iter()
        .filter(|line| line_visible_in_watch(line, filters, quick_filter))
        .count()
}

const WATCH_STATUS_HEIGHT: u16 = 4;
const WATCH_FILTER_HEIGHT: u16 = 5;
const WATCH_INFO_HEIGHT: u16 = 3;

fn watch_viewport_height() -> usize {
    let (_, rows) = terminal::size().unwrap_or((120, 40));
    rows.saturating_sub(WATCH_STATUS_HEIGHT + WATCH_FILTER_HEIGHT + WATCH_INFO_HEIGHT) as usize
}

fn render_logs_watch(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    active_path: &Path,
    entries: &VecDeque<String>,
    filters: &[LogFilterRule],
    selected_filter: usize,
    quick_filter: Option<&str>,
    paused: bool,
    profile_name: &str,
    log_cursor: usize,
    auto_follow: bool,
    status_message: &str,
) -> Result<()> {
    let mut visible_lines = entries
        .iter()
        .filter(|line| line_visible_in_watch(line, filters, quick_filter))
        .cloned()
        .collect::<Vec<_>>();
    if visible_lines.is_empty() {
        visible_lines.push("(no visible log lines)".to_string());
    }

    let cursor = log_cursor.min(visible_lines.len().saturating_sub(1));
    let viewport_height = watch_viewport_height().max(1);
    let start = cursor.saturating_sub(viewport_height.saturating_sub(1));
    let end = (start + viewport_height).min(visible_lines.len());
    let visible_slice = &visible_lines[start..end];

    terminal
        .draw(|frame| {
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(WATCH_STATUS_HEIGHT),
                    Constraint::Min(5),
                    Constraint::Length(WATCH_FILTER_HEIGHT),
                    Constraint::Length(WATCH_INFO_HEIGHT),
                ])
                .split(frame.area());

            let mode = if paused { "PAUSED" } else { "LIVE" };
            let follow = if auto_follow { "follow" } else { "manual" };
            let header_text = vec![
                Line::from(vec![
                    Span::styled("bmux logs watch", Style::default().fg(Color::Cyan)),
                    Span::raw(format!(
                        "  [{mode}] [{follow}] profile={} total={} visible={} file={}",
                        profile_name,
                        entries.len(),
                        visible_lines.len(),
                        active_path.display()
                    )),
                ]),
                Line::from(
                    "keys: q quit | p pause | j/k move | g/G top/bottom | ctrl-u/d half-page | pgup/pgdn page | a/x add | t toggle | i case | d delete | c clear | / search",
                ),
            ];
            let header = Paragraph::new(header_text)
                .block(Block::default().borders(Borders::ALL).title("Status"));
            frame.render_widget(header, chunks[0]);

            let log_items = visible_slice
                .iter()
                .map(|line| ListItem::new(line.clone()))
                .collect::<Vec<_>>();
            let logs_block =
                List::new(log_items).block(Block::default().borders(Borders::ALL).title("Logs"));
            frame.render_widget(logs_block, chunks[1]);

            let filter_lines = if filters.is_empty() {
                vec![Line::from("(none)")]
            } else {
                filters
                    .iter()
                    .enumerate()
                    .map(|(index, filter)| {
                        let marker = if index == selected_filter { '>' } else { ' ' };
                        let kind = match filter.kind {
                            LogFilterKind::Include => "+",
                            LogFilterKind::Exclude => "-",
                        };
                        let enabled = if filter.enabled { "on" } else { "off" };
                        let case = match filter.case_mode {
                            LogFilterCaseMode::Sensitive => "CS",
                            LogFilterCaseMode::Insensitive => "CI",
                        };
                        let error = if filter.has_error() {
                            " (regex error)"
                        } else {
                            ""
                        };
                        Line::from(format!(
                            "{marker} {kind} [{enabled}|{case}] /{}{error}",
                            filter.pattern
                        ))
                    })
                    .collect()
            };
            let filter_panel = Paragraph::new(filter_lines)
                .block(Block::default().borders(Borders::ALL).title("Filters"));
            frame.render_widget(filter_panel, chunks[2]);

            let footer_text = if status_message.is_empty() {
                match quick_filter {
                    Some(value) if !value.is_empty() => {
                        format!("quick filter: {value} | cursor: {}/{}", cursor + 1, visible_lines.len())
                    }
                    _ => format!("cursor: {}/{}", cursor + 1, visible_lines.len()),
                }
            } else {
                status_message.to_string()
            };
            let footer =
                Paragraph::new(footer_text).block(Block::default().borders(Borders::ALL).title("Info"));
            frame.render_widget(footer, chunks[3]);
        })
        .context("failed drawing logs watch terminal")?;

    Ok(())
}

fn prompt_logs_watch_line(prompt: &str) -> Result<Option<String>> {
    disable_raw_mode().context("failed disabling raw mode for prompt")?;
    {
        let mut stdout = io::stdout();
        execute!(stdout, Show).context("failed showing cursor for prompt")?;
        print!("\r\n{prompt}");
        stdout.flush().context("failed flushing prompt")?;
    }

    let mut input = String::new();
    let read_result = std::io::stdin().read_line(&mut input);

    {
        let mut stdout = io::stdout();
        execute!(stdout, Hide).context("failed hiding cursor after prompt")?;
    }
    enable_raw_mode().context("failed re-enabling raw mode after prompt")?;
    read_result.context("failed reading prompt input")?;

    let trimmed = input.trim().to_string();
    if trimmed.is_empty() {
        return Ok(None);
    }
    Ok(Some(trimmed))
}

pub(crate) fn active_log_file_path() -> PathBuf {
    let logs_dir = ConfigPaths::default().logs_dir();
    let mut newest: Option<(std::time::SystemTime, PathBuf)> = None;

    if let Ok(entries) = std::fs::read_dir(&logs_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(file_name) = path.file_name().and_then(|value| value.to_str()) else {
                continue;
            };
            if !file_name.starts_with("bmux.log") {
                continue;
            }
            let Ok(metadata) = entry.metadata() else {
                continue;
            };
            let Ok(modified) = metadata.modified() else {
                continue;
            };
            match &newest {
                Some((latest_modified, _)) if modified <= *latest_modified => {}
                _ => newest = Some((modified, path)),
            }
        }
    }

    newest
        .map(|(_, path)| path)
        .unwrap_or_else(|| logs_dir.join("bmux.log"))
}
