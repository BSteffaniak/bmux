//! Remove from view versus close for everyone, through real attachments of
//! one owned daemon: removing a tab moves every attachment of that view off
//! it (never terminating it), undo restores it, closing a tab for everyone
//! removes it from another principal's view and moves its attachments within
//! their own views, and an attachment whose view lost its last tab stays
//! connected with no input target: typing never reaches the hidden tab.
#![cfg(unix)]

#[allow(dead_code)] // Shared process/path fixture supports other integration targets too.
mod support;

use std::{
    os::unix::fs::PermissionsExt as _,
    process::{Child, Command, Stdio},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use anyhow::{Context as _, Result, ensure};
use bmux_cli::attach::{
    NativeAttachEvent, NativeAttachHandle, NativeAttachHost, initialize_native_runtime_state,
    run_native_with_client,
};
use bmux_client::BmuxClient;
use bmux_config::{BmuxConfig, ConfigLoadOverrides, ConfigPaths};
use bmux_frontend_runtime::input::{
    InputEvent, KeyEvent, KeyPhase, LogicalKey, Modifiers, PhysicalKey,
};
use uuid::Uuid;

const LIMIT: Duration = Duration::from_secs(15);
/// Daemon startup and attach convergence deadline. Generous because a loaded
/// host can take far longer to boot a plugin-loaded debug server.
const STARTUP: Duration = Duration::from_secs(120);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

struct OwnedDaemon {
    env: support::ServerEnv,
    child: Child,
}

impl OwnedDaemon {
    fn start() -> Result<Self> {
        let env = support::ServerEnv::new("personal-view-remove-close");
        std::fs::set_permissions(env.root(), std::fs::Permissions::from_mode(0o700))?;
        for name in ["home", "tmp", "cache"] {
            std::fs::create_dir(env.root().join(name))?;
        }
        // A quiet shell that reports every line it receives, so input that
        // reaches a tab shows in any attachment presenting it.
        let shell = env.root().join("quiet-shell");
        std::fs::write(
            &shell,
            "#!/bin/sh\nstty -echo\nwhile IFS= read -r line; do [ \"$line\" = quit ] && exit 0; printf 'got:%s\\n' \"$line\"; done\n",
        )?;
        std::fs::set_permissions(&shell, std::fs::Permissions::from_mode(0o700))?;
        env.write_config(&format!(
            "[general]\ndefault_shell = {}\n[behavior]\nrestore_last_layout = false\npane_shell_integration = false\n[multi_client]\ndefault_follow_mode = false\n[keybindings.global]\n\"alt+x\" = \"plugin:bmux.tabs:remove-from-view\"\n\"alt+u\" = \"plugin:bmux.tabs:undo-remove-from-view\"\n",
            serde_json::to_string(&shell.to_string_lossy())?,
        ));
        let child = isolated_command(
            &env,
            &[
                "--runtime",
                "default",
                "--target",
                "local",
                "server",
                "start",
                "--foreground-internal",
            ],
        )
        .stdout(std::fs::File::create(env.root().join("server.stdout"))?)
        .stderr(std::fs::File::create(env.root().join("server.stderr"))?)
        .spawn()?;
        Ok(Self { env, child })
    }

    fn paths(&self) -> ConfigPaths {
        ConfigPaths::new(
            self.env.config_dir.clone(),
            self.env.runtime_dir.clone(),
            self.env.data_dir.clone(),
            self.env.state_dir.clone(),
        )
    }

    fn logs(&self) -> String {
        format!(
            "stdout={:?} stderr={:?}",
            std::fs::read_to_string(self.env.root().join("server.stdout")),
            std::fs::read_to_string(self.env.root().join("server.stderr"))
        )
    }
}

impl Drop for OwnedDaemon {
    fn drop(&mut self) {
        if let Ok(mut stop) = isolated_command(
            &self.env,
            &[
                "--runtime",
                "default",
                "--target",
                "local",
                "server",
                "stop",
            ],
        )
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        {
            let until = Instant::now() + LIMIT;
            while Instant::now() < until {
                if stop.try_wait().ok().flatten().is_some() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            let _ = stop.kill();
            let _ = stop.wait();
        }
        let until = Instant::now() + LIMIT;
        while Instant::now() < until {
            if self.child.try_wait().ok().flatten().is_some() {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn isolated_command(env: &support::ServerEnv, args: &[&str]) -> Command {
    let mut command = env.command(args);
    command
        .env_clear()
        .current_dir(env.root())
        .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
        .env("HOME", env.root().join("home"))
        .env("TMPDIR", env.root().join("tmp"))
        .env("XDG_CONFIG_HOME", &env.config_dir)
        .env("XDG_RUNTIME_DIR", &env.runtime_dir)
        .env("XDG_DATA_HOME", &env.data_dir)
        .env("XDG_STATE_HOME", &env.state_dir)
        .env("XDG_CACHE_HOME", env.root().join("cache"))
        .env("BMUX_CONFIG_DIR", &env.config_dir)
        .env("BMUX_RUNTIME_DIR", &env.runtime_dir)
        .env("BMUX_DATA_DIR", &env.data_dir)
        .env("BMUX_STATE_DIR", &env.state_dir)
        .env("BMUX_LOG_DIR", &env.log_dir)
        .env("BMUX_CONFIG", env.config_dir.join("bmux.toml"))
        .env("BMUX_NO_BASE_CONFIG", "1")
        .env("BMUX_RUNTIME_NAME", "default")
        .env("BMUX_TARGET", "local")
        .env("TERM", "xterm-256color")
        .env("LC_ALL", "C")
        .stdin(Stdio::null());
    command
}

/// A connection whose handshake binds it to `principal`, as a client running
/// with another data directory (`<data>/runtime/principal-id`) would.
async fn connect_as(paths: &ConfigPaths, principal: Uuid) -> Result<BmuxClient> {
    let endpoint = bmux_ipc::IpcEndpoint::unix_socket(paths.server_socket());
    let until = Instant::now() + STARTUP;
    loop {
        match BmuxClient::connect_with_principal(
            &endpoint,
            REQUEST_TIMEOUT,
            "personal-view-remove-close",
            principal,
        )
        .await
        {
            Ok(mut client) => {
                client.require_service_contracts()?;
                return Ok(client);
            }
            Err(_) if Instant::now() < until => tokio::time::sleep(Duration::from_millis(20)).await,
            Err(error) => return Err(error.into()),
        }
    }
}

/// The rows of the most recent frame a native view delivered.
type Frame = Arc<Mutex<Vec<String>>>;

fn host(paths: &ConfigPaths) -> Result<(NativeAttachHandle, NativeAttachHost, Frame)> {
    let frame = Frame::default();
    let sink = frame.clone();
    let (handle, mut host) = NativeAttachHost::channel(100, 24, 1, move |delivery| {
        let buffer = delivery.prepared.buffer();
        let rows = (buffer.area().y..buffer.area().bottom())
            .filter_map(|y| buffer.row_symbols(y))
            .collect();
        *sink
            .lock()
            .map_err(|_| anyhow::anyhow!("frame capture poisoned"))? = rows;
        Ok(())
    })?;
    host.set_config_paths(paths.clone());
    Ok((handle, host, frame))
}

/// The tab bar row: the one row naming every `names` tab.
fn bar_row(frame: &Frame, names: &[&str]) -> Option<String> {
    frame
        .lock()
        .ok()?
        .iter()
        .find(|row| names.iter().all(|name| row.contains(name)))
        .cloned()
}

/// Wait until the bar shows `names` in that order and none of `hidden`.
async fn wait_bar(frame: &Frame, label: &str, names: &[&str], hidden: &[&str]) -> Result<()> {
    let until = Instant::now() + STARTUP;
    loop {
        if let Some(row) = bar_row(frame, names) {
            let positions: Vec<_> = names.iter().filter_map(|name| row.find(name)).collect();
            if positions.windows(2).all(|pair| pair[0] < pair[1])
                && !hidden.iter().any(|name| row.contains(name))
            {
                return Ok(());
            }
        }
        ensure!(
            Instant::now() < until,
            "view {label} never showed tabs {names:?} without {hidden:?}; frame: {:#?}",
            frame.lock().map(|rows| rows.clone()).unwrap_or_default()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn create_tab(control: &mut BmuxClient, name: &str) -> Result<(Uuid, Uuid)> {
    let ack = bmux_contexts_plugin_api::contexts_commands::client::create_context(
        control,
        Some(name.to_string()),
        std::collections::BTreeMap::new(),
    )
    .await?
    .map_err(|error| anyhow::anyhow!("create tab {name}: {error:?}"))?;
    let session = ack.session_id.context("created tab has no session")?;
    Ok((ack.id, session))
}

/// The session this view currently presents, from its committed compositor.
fn attached_session(handle: &NativeAttachHandle) -> Option<Uuid> {
    handle
        .pane_zoom_target(None)
        .map(|target| target.identity.attached_id)
}

async fn wait_attached(handle: &NativeAttachHandle, session: Uuid, label: &str) -> Result<()> {
    let until = Instant::now() + STARTUP;
    loop {
        if attached_session(handle) == Some(session) {
            return Ok(());
        }
        ensure!(
            Instant::now() < until,
            "view {label} never presented session {session}; presenting {:?}",
            attached_session(handle)
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// A key press with modifiers, as the native host delivers it.
fn key(character: char, alt: bool, control: bool) -> NativeAttachEvent {
    NativeAttachEvent::Input(InputEvent::Key(KeyEvent {
        physical: PhysicalKey::Unidentified,
        logical: LogicalKey::Character(character.to_string()),
        phase: KeyPhase::Press,
        modifiers: Modifiers {
            alt,
            control,
            ..Modifiers::default()
        },
        text: None,
    }))
}

/// A plain key press, as the native host delivers it.
fn character(character: char) -> NativeAttachEvent {
    key(character, false, false)
}

fn enter() -> NativeAttachEvent {
    NativeAttachEvent::Input(InputEvent::Key(KeyEvent {
        physical: PhysicalKey::Unidentified,
        logical: LogicalKey::Named(bmux_frontend_runtime::input::NamedKey::Enter),
        phase: KeyPhase::Press,
        modifiers: Modifiers::default(),
        text: None,
    }))
}

fn type_line(handle: &NativeAttachHandle, line: &str) -> Result<()> {
    for character_key in line.chars() {
        handle.try_send(character(character_key))?;
    }
    handle.try_send(enter())?;
    Ok(())
}

/// `alt+x`: `remove-from-view` (bound by this test's configuration).
fn remove() -> NativeAttachEvent {
    key('x', true, false)
}

/// `alt+u`: `undo-remove-from-view`.
fn undo() -> NativeAttachEvent {
    key('u', true, false)
}

/// `ctrl+s`: the default `next-tab` binding.
fn next() -> NativeAttachEvent {
    key('s', false, true)
}

fn frame_contains(frame: &Frame, text: &str) -> bool {
    frame
        .lock()
        .map(|rows| rows.iter().any(|row| row.contains(text)))
        .unwrap_or(false)
}

async fn wait_text(frame: &Frame, label: &str, text: &str) -> Result<()> {
    let until = Instant::now() + STARTUP;
    while !frame_contains(frame, text) {
        ensure!(
            Instant::now() < until,
            "view {label} never showed {text:?}; frame: {:#?}",
            frame.lock().map(|rows| rows.clone()).unwrap_or_default()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    Ok(())
}

#[allow(clippy::too_many_lines)] // One ordered multi-attachment scenario.
async fn exercise() -> Result<()> {
    let daemon = OwnedDaemon::start()?;
    let paths = daemon.paths();
    let config = BmuxConfig::load_isolated_with_paths_and_overrides(
        &paths,
        &ConfigLoadOverrides::default(),
    )?;
    let (one, two) = (Uuid::new_v4(), Uuid::new_v4());
    let mut control = connect_as(&paths, Uuid::new_v4())
        .await
        .with_context(|| format!("isolated daemon readiness failed: {}", daemon.logs()))?;
    let (alpha, alpha_session) = create_tab(&mut control, "alpha").await?;
    let (beta, beta_session) = create_tab(&mut control, "beta").await?;
    let (gamma, gamma_session) = create_tab(&mut control, "gamma").await?;
    // Two attachments of principal one share its default view; principal
    // two has its own.
    let client_a = connect_as(&paths, one).await?;
    let client_c = connect_as(&paths, one).await?;
    let client_b = connect_as(&paths, two).await?;
    initialize_native_runtime_state(&config, &paths)?;
    let (a, host_a, frame_a) = host(&paths)?;
    let (c, host_c, frame_c) = host(&paths)?;
    let (b, host_b, frame_b) = host(&paths)?;
    let controller = async {
        let result: Result<()> = async {
            for (frame, label) in [(&frame_a, "a"), (&frame_c, "c"), (&frame_b, "b")] {
                wait_bar(frame, label, &["alpha", "beta", "gamma"], &[]).await?;
            }
            for (handle, label) in [(&a, "a"), (&c, "c"), (&b, "b")] {
                wait_attached(handle, alpha_session, label).await?;
            }

            // Removing alpha from principal one's view moves both of its
            // attachments to the next tab; principal two keeps alpha.
            a.try_send(remove())?;
            wait_attached(&a, beta_session, "a").await?;
            wait_attached(&c, beta_session, "c").await?;
            wait_bar(&frame_a, "a", &["beta", "gamma"], &["alpha"]).await?;
            wait_bar(&frame_c, "c", &["beta", "gamma"], &["alpha"]).await?;
            // The transient undo hint is timed by wall clock (a few seconds),
            // so a loaded host may miss it here; the real-attach playbook
            // `personal_view_remove_real_attach` checks it.
            ensure!(
                attached_session(&b) == Some(alpha_session),
                "another principal's attachment moved"
            );
            wait_bar(&frame_b, "b", &["alpha", "beta", "gamma"], &[]).await?;

            // Undo puts it back in place and selects it again, for the
            // attachment that removed it only.
            a.try_send(undo())?;
            wait_bar(&frame_a, "a", &["alpha", "beta", "gamma"], &[]).await?;
            wait_attached(&a, alpha_session, "a").await?;
            ensure!(
                attached_session(&c) == Some(beta_session),
                "undo moved another attachment"
            );

            // Closing beta for everyone removes it from both principals'
            // views; the attachment on it moves within its own view.
            bmux_tabs_plugin_api::tabs_commands::client::kill_tab(
                &mut control,
                beta.to_string(),
                false,
            )
            .await?
            .map_err(|error| anyhow::anyhow!("kill beta: {error:?}"))?;
            wait_bar(&frame_b, "b", &["alpha", "gamma"], &["beta"]).await?;
            wait_bar(&frame_a, "a", &["alpha", "gamma"], &["beta"]).await?;
            wait_attached(&c, gamma_session, "c").await?;

            // Principal two moves to gamma, the tab principal one is about to
            // hide from its own view.
            b.try_send(next())?;
            wait_attached(&b, gamma_session, "b").await?;
            // Principal one removes alpha, then gamma: its view of the
            // workspace is empty, and both of its attachments are left with
            // no input target, still connected.
            a.try_send(remove())?;
            wait_bar(&frame_a, "a", &["gamma"], &["alpha"]).await?;
            wait_attached(&a, gamma_session, "a").await?;
            a.try_send(remove())?;
            wait_text(&frame_a, "a", "View empty").await?;
            wait_text(&frame_c, "c", "View empty").await?;

            // Typing into either emptied attachment never reaches the hidden
            // tab, which principal two still shows and which still runs.
            type_line(&a, "secret-from-a")?;
            type_line(&c, "secret-from-c")?;
            let settle = Instant::now() + Duration::from_secs(2);
            while Instant::now() < settle {
                ensure!(
                    !frame_contains(&frame_b, "secret-from"),
                    "a hidden tab received input from an emptied view"
                );
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            // The tab is alive and does receive its own attachment's input.
            type_line(&b, "visible-from-b")?;
            wait_text(&frame_b, "b", "got:visible-from-b").await?;
            ensure!(
                !frame_contains(&frame_b, "secret-from"),
                "a hidden tab received input from an emptied view"
            );
            let live =
                bmux_contexts_plugin_api::contexts_state::client::list_contexts(&mut control)
                    .await?;
            ensure!(
                [alpha, gamma]
                    .iter()
                    .all(|tab| live.iter().any(|context| context.id == *tab)),
                "removing tabs from a view terminated one: {live:?}"
            );

            // Undo resumes: gamma is back in the view and selected.
            a.try_send(undo())?;
            wait_bar(&frame_a, "a", &["gamma"], &["alpha", "View empty"]).await?;
            wait_attached(&a, gamma_session, "a").await?;
            type_line(&a, "resumed-from-a")?;
            wait_text(&frame_b, "b", "got:resumed-from-a").await?;
            Ok(())
        }
        .await;
        let closes = [&a, &c, &b].map(|handle| handle.try_send(NativeAttachEvent::Close));
        result?;
        for close in closes {
            close?;
        }
        Ok::<(), anyhow::Error>(())
    };
    let target = alpha_session.to_string();
    let (outcome_a, outcome_c, outcome_b, checks) =
        tokio::time::timeout(Duration::from_secs(400), async {
            tokio::join!(
                run_native_with_client(client_a, &target, config.clone(), host_a),
                run_native_with_client(client_c, &target, config.clone(), host_c),
                run_native_with_client(client_b, &target, config.clone(), host_b),
                controller,
            )
        })
        .await
        .with_context(|| format!("personal remove/close attach deadline: {}", daemon.logs()))?;
    checks?;
    ensure!(
        outcome_a?.status_code == 0 && outcome_c?.status_code == 0 && outcome_b?.status_code == 0,
        "attach failed"
    );
    Ok(())
}

#[test]
fn removal_moves_or_suspends_attachments_and_close_reaches_every_view() -> Result<()> {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("warn")
        .with_test_writer()
        .try_init();
    // Native plugin configuration is process-global; keep this test alone in
    // its own integration binary.
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?
        .block_on(exercise())
}
