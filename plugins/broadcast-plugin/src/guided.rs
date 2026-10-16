//! Guided caller workflows; no implicit publication and no credential text arguments.
use crate::{profiles, relay};
use bmux_broadcast_plugin_api::{broadcast_control_v1 as control, broadcast_turnkey_v1 as turnkey};
use bmux_plugin_sdk::{NativeCommandContext, NativeCommandInvocationSource, PluginCommandError};
use std::{collections::BTreeMap, path::Path};
fn failed(code: &'static str) -> PluginCommandError {
    PluginCommandError::failed(code)
}
pub(crate) fn viewer_url(endpoint: &str, channel: &str) -> Result<String, &'static str> {
    let mut url = relay::validate_endpoint(endpoint, channel)?;
    url.set_path("/live");
    url.set_query(Some(&format!("channel={channel}")));
    Ok(url.to_string())
}
fn options(args: &[String]) -> Result<BTreeMap<String, String>, PluginCommandError> {
    let mut map = BTreeMap::new();
    let mut index = 0;
    while index < args.len() {
        let key = args[index].as_str();
        if ["--default", "--advanced"].contains(&key) {
            if map.insert(key.into(), "true".into()).is_some() {
                return Err(failed("duplicate flag"));
            }
            index += 1;
            continue;
        }
        if ![
            "--profile",
            "--endpoint",
            "--channel",
            "--credential-file",
            "--source",
            "--session",
            "--executable",
            "--sha256",
            "--version",
            "--cols",
            "--rows",
        ]
        .contains(&key)
        {
            return Err(failed("unknown broadcast flag"));
        }
        let value = args
            .get(index + 1)
            .filter(|v| !v.starts_with("--"))
            .ok_or_else(|| failed("flag requires value"))?;
        if map.insert(key.into(), value.clone()).is_some() {
            return Err(failed("duplicate flag"));
        }
        index += 2;
    }
    if map.get("--profile").is_some_and(|value| value.len() > 48) {
        return Err(failed("profile name exceeds limit"));
    }
    if map.values().any(|value| value.len() > 4096) {
        return Err(failed("broadcast argument exceeds limit"));
    }
    Ok(map)
}
async fn structured_text(label: &str) -> Result<String, PluginCommandError> {
    use bmux_plugin::prompt::{PromptRequest, PromptResponse, PromptValue};
    match bmux_plugin::prompt::request(PromptRequest::text_input(label))
        .await
        .map_err(|_| failed("broadcast prompt unavailable"))?
    {
        PromptResponse::Submitted(PromptValue::Text(text)) if text.len() <= 4096 => Ok(text),
        _ => Err(failed("broadcast cancelled")),
    }
}
async fn prompt(context: &NativeCommandContext, label: &str) -> Result<String, PluginCommandError> {
    if context.invocation_source == NativeCommandInvocationSource::AttachKeybinding {
        return structured_text(label).await;
    }
    if context.invocation_source == NativeCommandInvocationSource::Cli {
        return crate::setup_ui::text(label)
            .await
            .map_err(|error| PluginCommandError::failed(error.to_string()))?
            .ok_or_else(|| failed("broadcast cancelled"));
    }
    Err(failed(
        "interactive selection unavailable for this invocation; pass explicit --source or --session",
    ))
}
async fn value(
    context: &NativeCommandContext,
    map: &BTreeMap<String, String>,
    key: &str,
    label: &str,
) -> Result<String, PluginCommandError> {
    match map.get(key) {
        Some(v) => Ok(v.clone()),
        None => prompt(context, label).await,
    }
}
async fn connectivity(endpoint: &str, channel: &str) -> Result<(), PluginCommandError> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let mut url = relay::validate_endpoint(endpoint, channel).map_err(failed)?;
    url.set_path(&format!("/api/live/{channel}/health"));
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(std::time::Duration::from_secs(3))
        .connect_timeout(std::time::Duration::from_secs(2))
        .build()
        .map_err(|_| failed("doctor transport unavailable"))?;
    let mut response = client
        .get(url)
        .send()
        .await
        .map_err(|_| failed("relay unreachable: check endpoint, DNS, TLS and deployment"))?;
    if !response.status().is_success() {
        return Err(failed("relay health rejected: check deployment routes"));
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| failed("relay health unavailable"))?
    {
        if bytes.len() + chunk.len() > 4096 {
            return Err(failed("relay health exceeds bound"));
        }
        bytes.extend_from_slice(&chunk);
    }
    let health: serde_json::Value =
        serde_json::from_slice(&bytes).map_err(|_| failed("relay health invalid JSON"))?;
    if health.get("renderer").and_then(serde_json::Value::as_str) != Some("bmwp-3")
        || health.get("protocol").and_then(serde_json::Value::as_u64) != Some(1)
    {
        return Err(failed("relay renderer mismatch: BMWP3 required"));
    }
    Ok(())
}
macro_rules! emit { ($context:expr, $($arg:tt)*) => { output($context, &format!($($arg)*)).await; }; }
fn terminal_defaults(
    map: &BTreeMap<String, String>,
) -> Result<profiles::TerminalDefaults, PluginCommandError> {
    Ok(profiles::TerminalDefaults {
        cols: map
            .get("--cols")
            .map_or(Ok(80), |v| v.parse())
            .map_err(|_| failed("invalid columns"))?,
        rows: map
            .get("--rows")
            .map_or(Ok(24), |v| v.parse())
            .map_err(|_| failed("invalid rows"))?,
        executable: map.get("--executable").map(Into::into),
    })
}
async fn output(context: &NativeCommandContext, text: &str) {
    if context.invocation_source == NativeCommandInvocationSource::Cli {
        println!("{text}");
    } else if context.invocation_source == NativeCommandInvocationSource::AttachKeybinding {
        let _ = bmux_plugin::prompt::request(
            bmux_plugin::prompt::PromptRequest::confirm(text).confirm_labels("OK", "Close"),
        )
        .await;
    }
}
pub(crate) async fn local(context: &NativeCommandContext) -> Result<bool, PluginCommandError> {
    let command = context.arguments.first().map(String::as_str);
    let root = Path::new(&context.connection.config_dir);
    match command {
        Some("setup") => {
            let map = options(&context.arguments[1..])?;
            let key = value(context, &map, "--profile", "Profile name").await?;
            profiles::name(&key).map_err(failed)?;
            let endpoint = value(
                context,
                &map,
                "--endpoint",
                "HTTPS relay endpoint (loopback HTTP allowed)",
            )
            .await?;
            let channel = map
                .get("--channel")
                .cloned()
                .unwrap_or_else(|| "main".into());
            relay::validate_endpoint(&endpoint, &channel).map_err(failed)?;
            let source = value(
                context,
                &map,
                "--credential-file",
                "Absolute private credential FILE path (not key text)",
            )
            .await?;
            if !Path::new(&source).is_absolute() {
                return Err(failed("absolute credential file required"));
            }
            let mut terminal = terminal_defaults(&map)?;
            if map.contains_key("--advanced")
                && !map.contains_key("--cols")
                && !map.contains_key("--rows")
                && !map.contains_key("--executable")
            {
                terminal.cols = prompt(context, "Terminal columns")
                    .await?
                    .parse()
                    .map_err(|_| failed("invalid columns"))?;
                terminal.rows = prompt(context, "Terminal rows")
                    .await?
                    .parse()
                    .map_err(|_| failed("invalid rows"))?;
                let executable = prompt(
                    context,
                    "Absolute executable path (empty uses this installed bmux)",
                )
                .await?;
                if !executable.is_empty() {
                    terminal.executable = Some(executable.into());
                }
            }
            profiles::Profile {
                endpoint: endpoint.clone(),
                channel: channel.clone(),
                credential_file: source.clone().into(),
                terminal: terminal.clone(),
            }
            .validate()
            .map_err(failed)?;
            // Reject corrupt configuration before importing anything.
            profiles::load(root).map_err(failed)?;
            relay::credential_key(Path::new(&source)).map_err(failed)?;
            connectivity(&endpoint, &channel).await?;
            let credential_file =
                profiles::import(root, &key, Path::new(&source)).map_err(failed)?;
            profiles::update(
                root,
                &key,
                profiles::Profile {
                    endpoint,
                    channel,
                    credential_file,
                    terminal,
                },
                map.contains_key("--default"),
            )
            .map_err(failed)?;
            output(
                context,
                &format!(
                    "Broadcast profile saved. Start with: bmux broadcast start --profile {key}"
                ),
            )
            .await;
            Ok(true)
        }
        Some("profiles") => {
            let p = profiles::load(root).map_err(failed)?;
            for key in p.profiles.keys() {
                emit!(
                    context,
                    "{key}{}",
                    if p.default.as_ref() == Some(key) {
                        " (default)"
                    } else {
                        ""
                    }
                );
            }
            Ok(true)
        }
        Some("doctor") => {
            let map = options(&context.arguments[1..])?;
            let p = profiles::selected(root, map.get("--profile").map(String::as_str))
                .map_err(failed)?;
            relay::Relay::new(&p.endpoint, &p.channel, &p.credential_file).map_err(failed)?;
            connectivity(&p.endpoint, &p.channel).await?;
            output(
                context,
                "Profile, private credential permissions and relay BMWP3 health: OK. No configuration changed.",
            ).await;
            Ok(true)
        }
        _ => Ok(false),
    }
}
pub(super) fn display_hint(value: &str) -> String {
    value
        .chars()
        .filter(|ch| {
            !ch.is_control() && !matches!(ch, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
        })
        .take(96)
        .collect()
}

struct TerminalChoice {
    id: uuid::Uuid,
    label: String,
}

// Resolve only against this displayed snapshot. Names are hints, never identity;
// duplicate names cannot silently select the wrong terminal.
fn terminal_choice(input: &str, choices: &[TerminalChoice]) -> Result<usize, &'static str> {
    let input = input.trim();
    let input = input
        .strip_prefix("\x1b[200~")
        .and_then(|value| value.strip_suffix("\x1b[201~"))
        .unwrap_or(input)
        .trim();
    if input.is_empty() || input.eq_ignore_ascii_case("q") {
        return Err("broadcast cancelled");
    }
    if let Ok(number) = input.parse::<usize>() {
        return number
            .checked_sub(1)
            .filter(|index| *index < choices.len())
            .ok_or("choose a listed number or full session UUID (q cancels)");
    }
    let id = uuid::Uuid::parse_str(input)
        .map_err(|_| "choose a listed number or full session UUID (q cancels)")?;
    choices
        .iter()
        .position(|choice| choice.id == id)
        .ok_or("terminal is not in the displayed list; cancel and retry to refresh")
}

async fn terminal_choices(
    client: &mut bmux_client::BmuxClient,
    enrich: bool,
) -> Result<Vec<TerminalChoice>, PluginCommandError> {
    use bmux_pane_runtime_plugin_api::pane_runtime_state;
    let sessions = bmux_sessions_plugin_api::sessions_state::client::list_sessions(client)
        .await
        .map_err(|_| failed("session list unavailable; specify --session UUID or --source UUID"))?;
    let mut choices = Vec::with_capacity(sessions.len());
    if sessions.len() > 256 {
        return Err(failed(
            "too many terminals for interactive selection; use explicit --session UUID",
        ));
    }
    for session in sessions {
        let name = session
            .name
            .as_deref()
            .map(display_hint)
            .filter(|name| !name.trim().is_empty())
            .unwrap_or_else(|| "Unnamed terminal".into());
        let mut label = format!("{name} — {} attached", session.client_count);
        if !enrich {
            label.push_str(&format!(" [{}]", session.id));
            choices.push(TerminalChoice {
                id: session.id,
                label,
            });
            continue;
        }
        // Optional enrichment only: missing/denied pane metadata must not
        // prevent selecting a session through the sessions contract.
        match pane_runtime_state::client::list_panes(client, Some(session.id)).await {
            Ok(Ok(list)) => {
                let hints: Vec<_> = list
                    .panes
                    .iter()
                    .take(3)
                    .map(|pane| {
                        let name = pane.name.as_deref().unwrap_or("pane");
                        let command = pane.active_command.as_deref().unwrap_or(&pane.shell);
                        let command = if pane.active_command.is_none() {
                            Path::new(command)
                                .file_name()
                                .and_then(|name| name.to_str())
                                .unwrap_or(command)
                        } else {
                            command
                        };
                        format!("{}: {}", display_hint(name), display_hint(command))
                    })
                    .collect();
                label.push_str(&format!(
                    " — {} pane(s): {}",
                    list.panes.len(),
                    hints.join("; ")
                ));
            }
            _ => label.push_str(" — pane details unavailable"),
        }
        label.push_str(&format!(" [{}]", session.id));
        choices.push(TerminalChoice {
            id: session.id,
            label,
        });
    }
    Ok(choices)
}

pub(crate) async fn connected(
    context: &NativeCommandContext,
    client: &mut bmux_client::BmuxClient,
) -> Result<bool, PluginCommandError> {
    let args = &context.arguments;
    if args.first().is_some_and(|arg| arg == "sources") {
        let sources = turnkey::client::sources(client)
            .await
            .map_err(|_| failed("source list unavailable"))?;
        for source in sources {
            emit!(context, "{}  {}", source.source_id, source.label);
        }
        return Ok(true);
    }
    if args.first().is_none_or(|arg| arg == "start")
        && !(args.len() == 5 && args.get(1).is_some_and(|arg| !arg.starts_with("--")))
    {
        let mut map = options(args.get(1..).unwrap_or_default())?;
        if !map.contains_key("--source") && !map.contains_key("--session") {
            let choices = terminal_choices(
                client,
                context.invocation_source != NativeCommandInvocationSource::Cli,
            )
            .await?;
            if choices.is_empty() {
                return Err(failed(
                    "no terminal sessions available; register a presentation source first",
                ));
            }
            let inline = context.invocation_source == NativeCommandInvocationSource::Cli;
            let session = if inline {
                let destination = profiles::selected(
                    Path::new(&context.connection.config_dir),
                    map.get("--profile").map(String::as_str),
                )
                .ok()
                .and_then(|profile| viewer_url(&profile.endpoint, &profile.channel).ok())
                .unwrap_or_else(|| "configured broadcast profile".into());
                let items = choices
                    .iter()
                    .map(|choice| {
                        let summary = choice
                            .label
                            .rsplit_once(" [")
                            .map_or(choice.label.as_str(), |(label, _)| label);
                        crate::inline_ui::Choice {
                            id: choice.id.to_string(),
                            title: summary.split(" — ").take(2).collect::<Vec<_>>().join(" · "),
                            details: format!(
                                "{}\nSession {}",
                                summary.replace(" — ", "\n"),
                                choice.id
                            ),
                        }
                    })
                    .collect();
                crate::inline_ui::select(
                    items,
                    destination,
                    bmux_config::ConfigPaths::new(
                        context.connection.config_dir.clone().into(),
                        context.connection.runtime_dir.clone().into(),
                        context.connection.data_dir.clone().into(),
                        context.connection.state_dir.clone().into(),
                    ),
                )
                .await
                .map_err(|_| {
                    failed("inline picker unavailable; use --session UUID or --source UUID")
                })?
                .ok_or_else(|| failed("broadcast cancelled"))?
            } else if context.invocation_source == NativeCommandInvocationSource::AttachKeybinding {
                use bmux_plugin::prompt::{
                    PromptOption, PromptRequest, PromptResponse, PromptValue,
                };
                let options = choices
                    .iter()
                    .map(|choice| PromptOption::new(choice.id.to_string(), choice.label.clone()))
                    .collect();
                match bmux_plugin::prompt::request(PromptRequest::single_select(
                    "Select ONE terminal to share",
                    options,
                ))
                .await
                .map_err(|_| failed("source prompt unavailable"))?
                {
                    PromptResponse::Submitted(PromptValue::Single(id)) => id,
                    _ => return Err(failed("broadcast cancelled")),
                }
            } else {
                output(
                    context,
                    "Choose one terminal to share publicly. Nothing is selected automatically.",
                )
                .await;
                for (index, choice) in choices.iter().enumerate() {
                    output(context, &format!("  {}. {}", index + 1, choice.label)).await;
                }
                loop {
                    let input =
                        prompt(context, "Terminal number (or full UUID; q cancels)").await?;
                    match terminal_choice(&input, &choices) {
                        Ok(index) => break choices[index].id.to_string(),
                        Err("broadcast cancelled") => return Err(failed("broadcast cancelled")),
                        Err(message) => output(context, message).await,
                    }
                }
            };
            let selected_index = terminal_choice(&session, &choices).map_err(failed)?;
            let selected = &choices[selected_index];
            let confirmed = inline
                || if context.invocation_source == NativeCommandInvocationSource::AttachKeybinding {
                    use bmux_plugin::prompt::{PromptRequest, PromptResponse, PromptValue};
                    matches!(
                        bmux_plugin::prompt::request(PromptRequest::confirm(format!(
                            "Share ALL output of {} publicly?",
                            selected.label
                        )))
                        .await
                        .map_err(|_| failed("confirmation unavailable"))?,
                        PromptResponse::Submitted(PromptValue::Confirm(true))
                    )
                } else {
                    prompt(
                        context,
                        &format!("Share ALL output of {} publicly? Type yes", selected.label),
                    )
                    .await?
                        == "yes"
                };
            if !confirmed {
                return Err(failed("broadcast cancelled"));
            }
            map.insert("--session".into(), session);
        }
        let selected_profile = match profiles::selected(
            Path::new(&context.connection.config_dir),
            map.get("--profile").map(String::as_str),
        ) {
            Ok(profile) => Some(profile),
            Err("run_broadcast_setup")
                if ["--endpoint", "--channel", "--credential-file"]
                    .iter()
                    .all(|key| map.contains_key(*key)) =>
            {
                None
            }
            Err(code) => return Err(failed(code)),
        };
        if let Some(profile) = selected_profile {
            map.entry("--cols".into())
                .or_insert_with(|| profile.terminal.cols.to_string());
            map.entry("--rows".into())
                .or_insert_with(|| profile.terminal.rows.to_string());
            if let Some(executable) = profile.terminal.executable {
                map.entry("--executable".into())
                    .or_insert_with(|| executable.to_string_lossy().into_owned());
            }
        }
        let mut id = match map.get("--source") {
            Some(id) => id.clone(),
            None if map.contains_key("--session") => uuid::Uuid::new_v4().to_string(),
            None => {
                let sources = turnkey::client::sources(client)
                    .await
                    .map_err(|_| failed("source list unavailable"))?;
                if context.invocation_source == NativeCommandInvocationSource::Cli {
                    for source in sources {
                        emit!(context, "{}  {}", source.source_id, source.label);
                    }
                }
                prompt(
                    context,
                    "Explicit source UUID to share (nothing is selected automatically)",
                )
                .await?
            }
        };
        if let Some(session) = map.get("--session") {
            let session =
                uuid::Uuid::parse_str(session).map_err(|_| failed("invalid session UUID"))?;
            if !map.contains_key("--source") {
                id = uuid::Uuid::new_v4().to_string();
            }
            let (executable, derived_hash, derived_version) =
                crate::identity::installed(map.get("--executable").map(String::as_str))
                    .await
                    .map_err(failed)?;
            let hash = map.get("--sha256").cloned().unwrap_or(derived_hash);
            let version = map.get("--version").cloned().unwrap_or(derived_version);
            let cols = map
                .get("--cols")
                .map_or(Ok(80), |v| v.parse::<u32>())
                .map_err(|_| failed("invalid columns"))?;
            let rows = map
                .get("--rows")
                .map_or(Ok(24), |v| v.parse::<u32>())
                .map_err(|_| failed("invalid rows"))?;
            control::client::terminal(
                client,
                id.clone(),
                session.to_string(),
                executable.to_string_lossy().into_owned(),
                hash,
                version,
                cols,
                rows,
            )
            .await
            .map_err(|_| failed("terminal source verification failed"))?;
        }
        uuid::Uuid::parse_str(&id).map_err(|_| failed("invalid source UUID"))?;
        if ["--endpoint", "--channel", "--credential-file"]
            .iter()
            .any(|key| map.contains_key(*key))
        {
            let mut p = match profiles::selected(
                Path::new(&context.connection.config_dir),
                map.get("--profile").map(String::as_str),
            ) {
                Ok(profile) => profile,
                Err("run_broadcast_setup")
                    if ["--endpoint", "--channel", "--credential-file"]
                        .iter()
                        .all(|key| map.contains_key(*key)) =>
                {
                    profiles::Profile::default()
                }
                Err(code) => return Err(failed(code)),
            };
            if let Some(v) = map.get("--endpoint") {
                p.endpoint.clone_from(v);
            }
            if let Some(v) = map.get("--channel") {
                p.channel.clone_from(v);
            }
            if let Some(v) = map.get("--credential-file") {
                p.credential_file = v.into();
            }
            let status = control::client::start(
                client,
                p.endpoint.clone(),
                p.channel.clone(),
                p.credential_file.to_string_lossy().into_owned(),
                id.clone(),
            )
            .await
            .map_err(|_| failed("broadcast start rejected"))?;
            let live = crate::startup_ui::wait(
                client,
                &id,
                context.invocation_source == NativeCommandInvocationSource::Cli,
            )
            .await
            .map_err(PluginCommandError::failed)?;
            emit!(
                context,
                "{} / {} source={} {}",
                if live {
                    "running"
                } else {
                    status.local.as_str()
                },
                if live { "live" } else { status.remote.as_str() },
                status.source_id.as_deref().unwrap_or("none"),
                viewer_url(&p.endpoint, &p.channel).map_err(failed)?
            );
        } else {
            let result = turnkey::client::start_profile(
                client,
                map.get("--profile").cloned(),
                id.clone(),
            )
            .await
            .map_err(|_| {
                failed(
                    "broadcast start rejected; run broadcast setup and register an explicit source",
                )
            })?;
            let live = crate::startup_ui::wait(
                client,
                &id,
                context.invocation_source == NativeCommandInvocationSource::Cli,
            )
            .await
            .map_err(PluginCommandError::failed)?;
            // Ownership token is private teardown authority: do not print/log it.
            emit!(
                context,
                "{} / {} source={} {}",
                if live {
                    "running"
                } else {
                    result.local.as_str()
                },
                if live { "live" } else { result.remote.as_str() },
                result.source_id.as_deref().unwrap_or("none"),
                result.viewer_url.as_deref().unwrap_or("")
            );
        }
        return Ok(true);
    }
    if args.first().is_some_and(|arg| arg == "status") && args.len() == 1 {
        let status = turnkey::client::status(client)
            .await
            .map_err(|_| failed("broadcast status unavailable"))?;
        if context.invocation_source == NativeCommandInvocationSource::Cli {
            println!(
                "{}",
                serde_json::to_string(&status)
                    .map_err(|_| failed("broadcast status unavailable"))?
            );
        } else {
            emit!(
                context,
                "{} / {} source={} {}",
                status.local,
                status.remote,
                status.source_id.as_deref().unwrap_or("none"),
                status.viewer_url.as_deref().unwrap_or("")
            );
        }
        return Ok(true);
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn picker_resolves_numbers_and_pasted_uuids_only_in_displayed_snapshot() {
        let id = uuid::Uuid::parse_str("17af57cf-c912-4809-8b0d-5a10fba78569").unwrap();
        let choices = vec![
            TerminalChoice {
                id,
                label: "editor".into(),
            },
            TerminalChoice {
                id: uuid::Uuid::new_v4(),
                label: "editor".into(),
            },
        ];
        assert_eq!(terminal_choice("1", &choices), Ok(0));
        assert_eq!(terminal_choice(" 2\r\n", &choices), Ok(1));
        assert_eq!(terminal_choice(&format!(" {id}\r\n"), &choices), Ok(0));
        assert_eq!(
            terminal_choice(&format!("\x1b[200~{id}\x1b[201~"), &choices),
            Ok(0)
        );
        for invalid in ["0", "3", "editor", "99999999999999999999999999", "-1"] {
            assert!(terminal_choice(invalid, &choices).is_err());
        }
        assert!(terminal_choice(&uuid::Uuid::new_v4().to_string(), &choices).is_err());
        assert!(terminal_choice("1", &[]).is_err());
        assert_eq!(terminal_choice("q", &choices), Err("broadcast cancelled"));
        assert_eq!(terminal_choice("", &choices), Err("broadcast cancelled"));
    }

    #[test]
    fn terminal_display_hints_are_bounded_single_line_text() {
        assert_eq!(display_hint("editor\n\r\x1b\u{202e}"), "editor");
        assert_eq!(display_hint(&"猫".repeat(200)).chars().count(), 96);
    }

    #[tokio::test]
    async fn structured_palette_prompt_is_host_owned() {
        use bmux_plugin::prompt::{PromptResponse, PromptValue};
        let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
        let guard = bmux_plugin::prompt::register_host(sender);
        let task = tokio::spawn(async { structured_text("Profile name").await });
        let request = receiver.recv().await.unwrap();
        request
            .response_tx
            .send(PromptResponse::Submitted(PromptValue::Text(
                "public".into(),
            )))
            .unwrap();
        assert_eq!(task.await.unwrap().unwrap(), "public");
        drop(guard);
    }
    #[test]
    fn explicit_flags_are_bounded_and_duplicates_rejected() {
        assert!(
            options(&[
                "--profile".into(),
                "a".into(),
                "--profile".into(),
                "b".into()
            ])
            .is_err()
        );
        assert!(options(&["--credential".into(), "plaintext".into()]).is_err());
        assert!(options(&["--source".into()]).is_err());
    }
    #[test]
    fn advanced_defaults_preserve_explicit_values() {
        let map = options(&[
            "--advanced".into(),
            "--cols".into(),
            "120".into(),
            "--rows".into(),
            "40".into(),
            "--executable".into(),
            "/installed/bmux".into(),
        ])
        .unwrap();
        let defaults = terminal_defaults(&map).unwrap();
        assert_eq!(defaults.cols, 120);
        assert_eq!(defaults.rows, 40);
        assert_eq!(defaults.executable.unwrap(), Path::new("/installed/bmux"));
    }
    #[test]
    fn viewer_url_never_contains_credentials() {
        assert!(viewer_url("https://secret@example.com", "main").is_err());
        assert_eq!(
            viewer_url("https://example.com", "main").unwrap(),
            "https://example.com/live?channel=main"
        );
    }
    #[tokio::test]
    async fn doctor_checks_real_bounded_public_health() {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut bytes = [0; 4096];
            let count = stream.read(&mut bytes).unwrap();
            let request = String::from_utf8_lossy(&bytes[..count]);
            assert!(request.starts_with("GET /api/live/main/health"));
            assert!(!request.to_lowercase().contains("authorization:"));
            let body = r#"{"protocol":1,"renderer":"bmwp-3"}"#;
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .unwrap();
        });
        connectivity(&format!("http://{address}"), "main")
            .await
            .unwrap();
        server.join().unwrap();
    }
}
