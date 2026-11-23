#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Which panes run a coding agent, and what each agent is doing.
//!
//! On the server, an observer follows every pane's foreground job to
//! identify agents ([`identify`]), reads agents' screens with their rules
//! ([`rules`]), listens for the reports agents send through their hooks, and
//! publishes each pane's state on `bmux.agents/agent-states` ([`tracker`]).
//! `bmux agents status` lists it; `bmux agents report` reports from a pane;
//! `bmux agents install claude` adds Claude Code hooks that report
//! ([`integration`]).

/// Plugin declaration generated from the BPDL world and `plugin.toml`.
pub mod declaration {
    bmux_plugin_schema_macros::plugin_manifest! {
        package: {
            schema: ::bmux_agents_plugin_api::export_schema,
            crate_path: ::bmux_agents_plugin_api,
        },
        settings: "plugin.toml",
    }
}

mod identify;
mod integration;
mod rules;
mod tracker;

use std::collections::HashMap;
use std::io::Write as _;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bmux_agents_plugin_api::agent_states::{self, AgentSnapshot, AgentSource, AgentState};
use bmux_pane_runtime_plugin_api::pane_runtime_state as api_pane_state;
use bmux_plugin::{ServiceCaller, TypedServiceCaller};
use bmux_plugin_sdk::author::{Declarations, Plugin};
use bmux_plugin_sdk::prelude::*;

/// How often the observer reads changed screens and settles states.
const TICK: Duration = Duration::from_millis(400);
/// Every how many ticks it lists the panes' foreground jobs.
const JOBS_EVERY: u32 = 3;

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
        })
}

/// `[plugins.settings."bmux.agents"]`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Settings {
    /// More programs to treat as agents (with no screen rules).
    agents: Vec<String>,
    /// More names or exact paths each agent runs as, by agent id.
    aliases: std::collections::BTreeMap<String, Vec<String>>,
    /// Patterns of command lines each agent runs as: `(agent id, regex)`.
    patterns: Vec<(String, String)>,
    /// Read agents' screens with their rules (on by default).
    screen_rules: bool,
}

impl Settings {
    fn parse(value: Option<&toml::Value>) -> Result<Self, String> {
        let mut settings = Self {
            screen_rules: true,
            ..Self::default()
        };
        let Some(value) = value else {
            return Ok(settings);
        };
        let table = value
            .as_table()
            .ok_or("bmux.agents settings must be a table")?;
        for (key, value) in table {
            match key.as_str() {
                "agents" => {
                    settings.agents = value
                        .as_array()
                        .ok_or("agents must be a list of program names")?
                        .iter()
                        .map(|name| {
                            name.as_str()
                                .map(str::to_string)
                                .ok_or("agents must be a list of program names")
                        })
                        .collect::<Result<_, _>>()?;
                }
                "aliases" => {
                    (settings.aliases, settings.patterns) = parse_aliases(value)?;
                }
                "screen_rules" => {
                    settings.screen_rules = value
                        .as_bool()
                        .ok_or("screen_rules must be true or false")?;
                }
                other => return Err(format!("unknown bmux.agents setting `{other}`")),
            }
        }
        Ok(settings)
    }
}

/// `aliases`: each agent id's list of program names, exact paths, and
/// `{ regex = "…" }` patterns of whole command lines.
type Aliases = (
    std::collections::BTreeMap<String, Vec<String>>,
    Vec<(String, String)>,
);

fn parse_aliases(value: &toml::Value) -> Result<Aliases, String> {
    const SHAPE: &str =
        "aliases must map agent ids to lists of program names, paths or { regex = \"…\" } patterns";
    let mut names = std::collections::BTreeMap::new();
    let mut patterns = Vec::new();
    for (agent, entries) in value.as_table().ok_or(SHAPE)? {
        let mut agent_names = Vec::new();
        for entry in entries.as_array().ok_or(SHAPE)? {
            if let Some(name) = entry.as_str() {
                agent_names.push(name.to_string());
                continue;
            }
            let pattern = entry
                .as_table()
                .filter(|table| table.len() == 1)
                .and_then(|table| table.get("regex"))
                .and_then(toml::Value::as_str)
                .ok_or(SHAPE)?;
            regex::Regex::new(pattern)
                .map_err(|error| format!("aliases.{agent}: invalid regex `{pattern}`: {error}"))?;
            patterns.push((agent.clone(), pattern.to_string()));
        }
        names.insert(agent.clone(), agent_names);
    }
    Ok((names, patterns))
}

/// Every agent's rules and names.
struct Knowledge {
    rules: HashMap<String, rules::Agent>,
    catalog: identify::Catalog,
    screen_rules: bool,
}

impl Knowledge {
    /// The bundled rule files, then each config directory's `agents/*.toml`
    /// (the first directory wins), which replace bundled files by id.
    fn load(settings: &Settings, config_dirs: &[PathBuf]) -> Self {
        let mut rules = HashMap::new();
        for (name, source) in rules::BUNDLED {
            match rules::Agent::parse(source) {
                Ok(agent) => {
                    rules.insert(agent.id.clone(), agent);
                }
                Err(error) => tracing::warn!(%name, %error, "bundled agent rules unreadable"),
            }
        }
        for dir in config_dirs.iter().rev() {
            let Ok(entries) = std::fs::read_dir(dir.join("agents")) else {
                continue;
            };
            let mut paths = entries
                .filter_map(|entry| entry.ok().map(|entry| entry.path()))
                .filter(|path| {
                    path.extension()
                        .is_some_and(|extension| extension == "toml")
                })
                .collect::<Vec<_>>();
            paths.sort();
            for path in paths {
                match std::fs::read_to_string(&path)
                    .map_err(|error| error.to_string())
                    .and_then(|source| rules::Agent::parse(&source))
                {
                    Ok(agent) => {
                        rules.insert(agent.id.clone(), agent);
                    }
                    Err(error) => {
                        tracing::warn!(path = %path.display(), %error, "agent rules unreadable");
                    }
                }
            }
        }
        let mut extra = identify::DEFAULT_RULELESS_AGENTS
            .iter()
            .map(|name| (*name).to_string())
            .collect::<Vec<_>>();
        extra.extend(settings.agents.iter().cloned());
        // Configured aliases come last, so they win over the rule files'.
        let catalog = identify::Catalog::new(
            rules
                .values()
                .map(|agent| (agent.id.as_str(), agent.aliases.as_slice()))
                .chain(
                    settings
                        .aliases
                        .iter()
                        .map(|(agent, names)| (agent.as_str(), names.as_slice())),
                ),
            &extra,
        )
        .with_patterns(settings.patterns.iter().filter_map(|(agent, pattern)| {
            regex::Regex::new(pattern)
                .ok()
                .map(|pattern| (pattern, agent.clone()))
        }));
        Self {
            rules,
            catalog,
            screen_rules: settings.screen_rules,
        }
    }

    fn rules_for(&self, agent: &str) -> Option<&rules::Agent> {
        self.screen_rules.then(|| self.rules.get(agent)).flatten()
    }
}

static REVISION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

#[derive(Default)]
pub struct AgentsPlugin {
    tracker: Arc<Mutex<tracker::Tracker>>,
}

impl Plugin for AgentsPlugin {
    fn declare(d: &mut Declarations<Self>) {
        d.blocking_activate(Self::activate)
            .blocking_command("status", |_, context| Self::run_command(context))
            .blocking_command("report", |_, context| Self::run_command(context))
            .blocking_command("install", |_, context| Self::run_command(context))
            .blocking_command("uninstall", |_, context| Self::run_command(context))
            .blocking_services(Self::invoke_service, None);
    }
}

impl AgentsPlugin {
    fn activate(
        &self,
        context: &NativeLifecycleContext,
        async_handle: Option<bmux_plugin_sdk::HostAsyncHandle>,
    ) -> Result<(), PluginCommandError> {
        let settings = Settings::parse(context.settings.as_ref())
            .map_err(PluginCommandError::invalid_arguments)?;
        let Some(events) = bmux_plugin::activation_event_bus(context) else {
            return Ok(());
        };
        events.register_state_channel::<AgentSnapshot>(
            agent_states::STATE_KIND,
            AgentSnapshot {
                panes: Vec::new(),
                revision: 0,
            },
        );
        let Some(async_handle) = async_handle else {
            return Ok(());
        };
        let mut config_dirs = context
            .connection
            .config_dir_candidates
            .iter()
            .map(PathBuf::from)
            .collect::<Vec<_>>();
        if config_dirs.is_empty() {
            config_dirs.push(PathBuf::from(&context.connection.config_dir));
        }
        let observer = Observer {
            caller: Arc::new(TypedServiceCaller::from_lifecycle_context(context)),
            tracker: Arc::clone(&self.tracker),
            knowledge: Arc::new(Knowledge::load(&settings, &config_dirs)),
            events,
        };
        observer.spawn(&async_handle)
    }

    fn invoke_service(&self, context: &NativeServiceContext) -> ServiceResponse {
        let tracker = Arc::clone(&self.tracker);
        bmux_plugin_sdk::route_service!(context, {
            "agents-state", "list-agents" => |_req: (), _ctx| {
                let panes = tracker.lock().map(|tracker| tracker.snapshot()).unwrap_or_default();
                Ok::<_, ServiceResponse>(AgentSnapshot {
                    panes,
                    revision: REVISION.load(std::sync::atomic::Ordering::Relaxed),
                })
            },
        })
    }

    fn run_command(context: &NativeCommandContext) -> Result<i32, PluginCommandError> {
        match context.command.as_str() {
            "status" => status(context),
            "report" => report(&context.arguments),
            "install" | "uninstall" => {
                let agent = positional(&context.arguments, 0)
                    .ok_or_else(|| PluginCommandError::invalid_arguments("name an agent"))?;
                if !integration::SUPPORTED.contains(&agent.as_str()) {
                    return Err(PluginCommandError::invalid_arguments(format!(
                        "no integration for `{agent}`; available: {}",
                        integration::SUPPORTED.join(", ")
                    )));
                }
                let home = std::env::var_os("HOME")
                    .map(PathBuf::from)
                    .ok_or_else(|| PluginCommandError::unavailable("HOME is not set"))?;
                let dir = integration::claude_dir(
                    &home,
                    std::env::var("CLAUDE_CONFIG_DIR").ok().as_deref(),
                );
                let outcome = if context.command == "install" {
                    integration::install_claude(&dir)
                } else {
                    integration::uninstall_claude(&dir)
                }
                .map_err(PluginCommandError::failed)?;
                println!("{outcome}");
                Ok(EXIT_OK)
            }
            other => Err(PluginCommandError::invalid_arguments(format!(
                "unknown agents command `{other}`"
            ))),
        }
    }
}

/// Follows panes and publishes their agents' states.
struct Observer {
    caller: Arc<TypedServiceCaller>,
    tracker: Arc<Mutex<tracker::Tracker>>,
    knowledge: Arc<Knowledge>,
    events: Arc<bmux_plugin::EventBus>,
}

impl Observer {
    fn spawn(
        self,
        async_handle: &bmux_plugin_sdk::HostAsyncHandle,
    ) -> Result<(), PluginCommandError> {
        use bmux_pane_runtime_plugin_api::pane_runtime_events::{self, PaneEvent};
        use bmux_pane_runtime_plugin_api::pane_runtime_terminal_events::{self, TerminalEvent};
        let worker = async_handle.clone();
        let observer = Arc::new(self);
        async_handle
            .spawn_background("agents-observer", move |mut cancellation| async move {
                let terminal_kind = pane_runtime_terminal_events::EVENT_KIND;
                let pane_kind = pane_runtime_events::EVENT_KIND;
                let mut terminal = None;
                let mut panes = None;
                let mut tick = tokio::time::interval(TICK);
                tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                let mut ticks = 0_u32;
                loop {
                    tokio::select! {
                        () = cancellation.cancelled() => return Ok(()),
                        ready = observer.events.subscribe_when_registered::<TerminalEvent>(&terminal_kind), if terminal.is_none() => {
                            terminal = ready.ok();
                        }
                        ready = observer.events.subscribe_when_registered::<PaneEvent>(&pane_kind), if panes.is_none() => {
                            panes = ready.ok();
                        }
                        event = receive(&mut terminal) => match event {
                            Ok(event) => {
                                if let Ok(mut tracker) = observer.tracker.lock() {
                                    tracker.apply_terminal(&event);
                                }
                            }
                            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                            Err(tokio::sync::broadcast::error::RecvError::Closed) => terminal = None,
                        },
                        event = receive(&mut panes) => match event {
                            Ok(event) => {
                                if let Ok(mut tracker) = observer.tracker.lock() {
                                    tracker.apply_pane(&event, now_ms());
                                }
                            }
                            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                            Err(tokio::sync::broadcast::error::RecvError::Closed) => panes = None,
                        },
                        _ = tick.tick() => {
                            let list_jobs = ticks.is_multiple_of(JOBS_EVERY);
                            ticks = ticks.wrapping_add(1);
                            let observer = Arc::clone(&observer);
                            worker
                                .spawn_blocking(move || observer.pass(list_jobs))
                                .await
                                .map_err(|error| error.to_string())?;
                        }
                    }
                }
            })
            .map_err(PluginCommandError::failed)
    }

    /// One pass: list jobs (when due), read changed screens, settle, publish.
    fn pass(&self, list_jobs: bool) {
        let caller = self.caller.as_ref();
        if list_jobs
            && let Some(jobs) = foreground_jobs(caller)
            && let Ok(mut tracker) = self.tracker.lock()
        {
            tracker.set_jobs(&jobs, &self.knowledge.catalog);
        }
        let has_rules = |agent: &str| self.knowledge.rules_for(agent).is_some();
        let screens = self
            .tracker
            .lock()
            .map(|tracker| tracker.screens_to_read(has_rules))
            .unwrap_or_default();
        for (session_id, pane_id) in screens {
            let Some(text) = screen_text(caller, session_id, pane_id) else {
                continue;
            };
            let Ok(mut tracker) = self.tracker.lock() else {
                return;
            };
            if let Some(rules) = tracker
                .agent(pane_id)
                .and_then(|agent| self.knowledge.rules_for(agent))
            {
                tracker.apply_screen(pane_id, &text, rules);
            }
        }
        let Ok(mut tracker) = self.tracker.lock() else {
            return;
        };
        tracker.settle(now_ms(), has_rules);
        let panes = tracker.snapshot();
        if !tracker.changed(&panes) {
            return;
        }
        drop(tracker);
        let snapshot = AgentSnapshot {
            panes,
            revision: REVISION.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1,
        };
        if let Err(error) = self
            .events
            .publish_state(&agent_states::STATE_KIND, snapshot)
        {
            tracing::warn!(%error, "cannot publish agent states");
        }
    }
}

async fn receive<E: Send + Sync>(
    receiver: &mut Option<tokio::sync::broadcast::Receiver<Arc<E>>>,
) -> Result<Arc<E>, tokio::sync::broadcast::error::RecvError> {
    match receiver {
        Some(receiver) => receiver.recv().await,
        None => std::future::pending().await,
    }
}

fn foreground_jobs(caller: &(impl ServiceCaller + Sync)) -> Option<Vec<tracker::Job>> {
    let mut client = bmux_plugin::ServiceCallerDispatchClient::new(caller);
    let jobs = bmux_plugin::block_on_typed_dispatch(api_pane_state::client::list_foreground_jobs(
        &mut client,
    ))
    .map_err(|error| tracing::debug!(%error, "cannot list foreground jobs"))
    .ok()?
    .ok()?;
    Some(
        jobs.into_iter()
            .map(|job| tracker::Job {
                session_id: job.session_id,
                pane_id: job.pane_id,
                process_group_id: job.process_group_id,
                commands: job.commands,
            })
            .collect(),
    )
}

fn screen_text(
    caller: &(impl ServiceCaller + Sync),
    session_id: uuid::Uuid,
    pane_id: uuid::Uuid,
) -> Option<String> {
    let mut client = bmux_plugin::ServiceCallerDispatchClient::new(caller);
    bmux_plugin::block_on_typed_dispatch(api_pane_state::client::pane_screen_text(
        &mut client,
        session_id,
        pane_id,
    ))
    .ok()?
    .ok()
    .map(|screen| screen.text)
}

fn positional(arguments: &[String], index: usize) -> Option<String> {
    let mut skip_value = false;
    arguments
        .iter()
        .filter(|argument| {
            if std::mem::take(&mut skip_value) {
                return false;
            }
            if argument.starts_with("--") {
                skip_value = !argument.contains('=');
                return false;
            }
            true
        })
        .nth(index)
        .cloned()
}

fn option(arguments: &[String], name: &str) -> Option<String> {
    let flag = format!("--{name}");
    let prefix = format!("--{name}=");
    arguments.iter().enumerate().find_map(|(index, argument)| {
        if *argument == flag {
            arguments.get(index + 1).cloned()
        } else {
            argument.strip_prefix(&prefix).map(str::to_string)
        }
    })
}

/// `bmux agents report <state> [--agent NAME] [--message TEXT]`: write the
/// pane's authenticated report to the terminal this runs in.
fn report(arguments: &[String]) -> Result<i32, PluginCommandError> {
    let state = positional(arguments, 0)
        .ok_or_else(|| PluginCommandError::invalid_arguments("name a state"))?;
    if !matches!(state.as_str(), "working" | "blocked" | "idle" | "release") {
        return Err(PluginCommandError::invalid_arguments(format!(
            "unknown state `{state}`: use working, blocked, idle or release"
        )));
    }
    let marker = std::env::var("BMUX_SHELL_MARKER").map_err(|_| {
        PluginCommandError::unavailable("not inside a bmux pane (BMUX_SHELL_MARKER is not set)")
    })?;
    let sequence = report_sequence(
        &marker,
        &state,
        option(arguments, "agent").as_deref(),
        option(arguments, "message").as_deref(),
    );
    let mut terminal = std::fs::OpenOptions::new()
        .write(true)
        .open(if cfg!(windows) { "CONOUT$" } else { "/dev/tty" })
        .map_err(|error| {
            PluginCommandError::unavailable(format!("no terminal to report to: {error}"))
        })?;
    terminal
        .write_all(sequence.as_bytes())
        .and_then(|()| terminal.flush())
        .map_err(|error| PluginCommandError::failed(error.to_string()))?;
    Ok(EXIT_OK)
}

/// `OSC 633;bmux;<marker>;agent;<state>;<base64 agent>;<base64 message>`.
fn report_sequence(
    marker: &str,
    state: &str,
    agent: Option<&str>,
    message: Option<&str>,
) -> String {
    format!(
        "\x1b]633;bmux;{marker};agent;{state};{};{}\x07",
        agent.map(base64).unwrap_or_default(),
        message.map(base64).unwrap_or_default()
    )
}

fn base64(text: &str) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let triple = chunk.iter().enumerate().fold(0_u32, |acc, (index, byte)| {
            acc | (u32::from(*byte) << (16 - 8 * index))
        });
        for index in 0..4 {
            if index <= chunk.len() {
                out.push(char::from(
                    ALPHABET[((triple >> (18 - 6 * index)) & 0x3f) as usize],
                ));
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// `bmux agents status [--json]`.
fn status(context: &NativeCommandContext) -> Result<i32, PluginCommandError> {
    let mut client = bmux_plugin::ServiceCallerDispatchClient::new(context);
    let snapshot = bmux_plugin::block_on_typed_dispatch(
        bmux_agents_plugin_api::agents_state::client::list_agents(&mut client),
    )
    .map_err(|error| PluginCommandError::unavailable(format!("agents unavailable: {error}")))?;
    if context
        .arguments
        .iter()
        .any(|argument| argument == "--json")
    {
        let json = serde_json::to_string_pretty(&snapshot)
            .map_err(|error| PluginCommandError::failed(error.to_string()))?;
        println!("{json}");
        return Ok(EXIT_OK);
    }
    if snapshot.panes.is_empty() {
        println!("no agents running");
        return Ok(EXIT_OK);
    }
    let now = now_ms();
    for pane in snapshot.panes {
        let state = match pane.state {
            AgentState::Idle => "idle",
            AgentState::Working => "working",
            AgentState::Blocked => "blocked",
        };
        let source = match (pane.source, pane.rule.as_deref()) {
            (AgentSource::Report, _) => "reported".to_string(),
            (AgentSource::Screen, Some(rule)) => format!("screen rule {rule}"),
            (AgentSource::Screen, None) => "screen".to_string(),
            (AgentSource::Activity, _) => "output activity".to_string(),
        };
        let age = now.saturating_sub(pane.changed_at_ms) / 1000;
        print!(
            "{}\t{}\t{state} for {age}s\t({source})",
            pane.pane_id, pane.agent
        );
        if let Some(message) = pane.message {
            print!("\t{message}");
        }
        println!();
    }
    Ok(EXIT_OK)
}

bmux_plugin_sdk::plugin!(AgentsPlugin);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_name_extra_agents_and_refuse_unknown_keys() {
        assert_eq!(
            Settings::parse(None).unwrap(),
            Settings {
                screen_rules: true,
                ..Settings::default()
            }
        );
        let value =
            toml::from_str::<toml::Value>("agents = ['mybot']\nscreen_rules = false").unwrap();
        assert_eq!(
            Settings::parse(Some(&value)).unwrap(),
            Settings {
                agents: vec!["mybot".into()],
                screen_rules: false,
                ..Settings::default()
            }
        );
        let value = toml::from_str::<toml::Value>("aliases = { claude = 3 }").unwrap();
        assert!(
            Settings::parse(Some(&value))
                .unwrap_err()
                .contains("aliases")
        );
        let value = toml::from_str::<toml::Value>("agent = ['typo']").unwrap();
        assert!(Settings::parse(Some(&value)).unwrap_err().contains("agent"));
    }

    #[test]
    fn user_rules_replace_bundled_ones_and_add_agents() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("agents")).unwrap();
        std::fs::write(
            dir.path().join("agents/mine.toml"),
            "id = 'mine'\naliases = ['my-agent']\n[[rules]]\nid = 'busy'\nstate = 'working'\ncontains = ['busy']",
        )
        .unwrap();
        std::fs::write(dir.path().join("agents/broken.toml"), "nonsense").unwrap();
        let knowledge = Knowledge::load(&Settings::parse(None).unwrap(), &[dir.path().into()]);
        assert!(knowledge.rules_for("claude").is_some());
        assert!(knowledge.rules_for("mine").is_some());
        assert_eq!(
            knowledge
                .catalog
                .identify(&["my-agent --go".into()])
                .as_deref(),
            Some("mine")
        );
        assert_eq!(
            knowledge.catalog.identify(&["aider".into()]).as_deref(),
            Some("aider")
        );
        let off = Settings {
            screen_rules: false,
            ..Settings::default()
        };
        assert!(Knowledge::load(&off, &[]).rules_for("claude").is_none());
    }

    #[test]
    fn configured_aliases_name_agents_with_or_without_rules() {
        let value = toml::from_str::<toml::Value>(
            "[aliases]\nclaude = ['work-claude', '/opt/ai/bin/run-agent']\nmybot = ['mb']",
        )
        .unwrap();
        let settings = Settings::parse(Some(&value)).unwrap();
        let knowledge = Knowledge::load(&settings, &[]);
        let identify = |command: &str| knowledge.catalog.identify(&[command.to_string()]);
        // A rule-backed agent keeps its screen rules under its other names.
        assert_eq!(identify("work-claude --resume").as_deref(), Some("claude"));
        assert_eq!(identify("/opt/ai/bin/run-agent").as_deref(), Some("claude"));
        assert!(knowledge.rules_for("claude").is_some());
        // Any other id is an agent without rules.
        assert_eq!(identify("mb").as_deref(), Some("mybot"));
        assert!(knowledge.rules_for("mybot").is_none());
        assert_eq!(identify("/usr/bin/run-agent"), None);
    }

    #[test]
    fn alias_patterns_match_whole_command_lines() {
        let value = toml::from_str::<toml::Value>(
            r"aliases = { claude = [{ regex = '^/nix/store/[^/]+-claude-code[^/]*/bin/' }, 'cc'], helper = [{ regex = '\bagent-runner\b.*--model' }] }",
        )
        .unwrap();
        let settings = Settings::parse(Some(&value)).unwrap();
        let knowledge = Knowledge::load(&settings, &[]);
        let identify = |command: &str| knowledge.catalog.identify(&[command.to_string()]);
        assert_eq!(
            identify("/nix/store/abc-claude-code-2.1/bin/launcher --resume").as_deref(),
            Some("claude")
        );
        assert_eq!(identify("cc").as_deref(), Some("claude"));
        assert_eq!(
            identify("python3 /srv/agent-runner.py --model big").as_deref(),
            Some("helper")
        );
        assert_eq!(identify("python3 /srv/agent-runner.py"), None);
        // A later process of the job can match too.
        assert_eq!(
            knowledge
                .catalog
                .identify(&["tee log".into(), "/srv/agent-runner --model x".into()])
                .as_deref(),
            Some("helper")
        );

        for (source, error) in [
            ("aliases = { claude = [{ regex = '(' }] }", "invalid regex"),
            ("aliases = { claude = [{ pattern = 'x' }] }", "aliases"),
            (
                "aliases = { claude = [{ regex = 'x', other = 1 }] }",
                "aliases",
            ),
        ] {
            let value = toml::from_str::<toml::Value>(source).unwrap();
            let message = Settings::parse(Some(&value)).unwrap_err();
            assert!(message.contains(error), "{source}: {message}");
        }
    }

    #[test]
    fn reports_are_marked_osc_sequences() {
        assert_eq!(
            report_sequence("tok", "blocked", Some("claude"), Some("approve?")),
            "\x1b]633;bmux;tok;agent;blocked;Y2xhdWRl;YXBwcm92ZT8=\x07"
        );
        assert_eq!(
            report_sequence("tok", "idle", None, None),
            "\x1b]633;bmux;tok;agent;idle;;\x07"
        );
        assert_eq!(base64("ab"), "YWI=");
        assert_eq!(base64("a"), "YQ==");
        assert_eq!(base64(""), "");
    }

    #[test]
    fn arguments_separate_positionals_from_options() {
        let arguments = [
            "--agent".to_string(),
            "x".into(),
            "working".into(),
            "--message=hi".into(),
        ];
        assert_eq!(positional(&arguments, 0).as_deref(), Some("working"));
        assert_eq!(option(&arguments, "agent").as_deref(), Some("x"));
        assert_eq!(option(&arguments, "message").as_deref(), Some("hi"));
    }
}
