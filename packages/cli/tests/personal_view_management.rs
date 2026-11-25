//! View management, labels and follow through real attachments of one owned
//! daemon: one principal with two views on two attachments keeps independent
//! arrangements, a personal label renames a tab for one view while a shared
//! rename reaches everyone, following another client shows its tab without
//! adding it to the follower's view, also as the leader switches tabs (ending
//! the follow restores the follower's own tab), and deleting a view rebinds its attachment to the
//! default view, which itself cannot be deleted.
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
        let env = support::ServerEnv::new("personal-view-management");
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
        env.write_config(&config_text(&shell, None)?);
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
            "personal-view-management",
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
    let (handle, mut host) = NativeAttachHost::channel(120, 24, 1, move |delivery| {
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

/// `alt+x`: `remove-from-view` (bound by this test's configuration).
fn remove() -> NativeAttachEvent {
    key('x', true, false)
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

/// The daemon and attach configuration: a quiet shell, no follow by default,
/// and key bindings for the view commands (with `follow` bound to `leader`
/// once its client id is known).
fn config_text(shell: &std::path::Path, leader: Option<Uuid>) -> Result<String> {
    let mut text = format!(
        "[general]\ndefault_shell = {}\n[behavior]\nrestore_last_layout = false\npane_shell_integration = false\n[multi_client]\ndefault_follow_mode = false\n[keybindings.global]\n\"alt+c\" = \"plugin:bmux.tabs:create-view work --copy\"\n\"alt+x\" = \"plugin:bmux.tabs:remove-from-view\"\n\"alt+m\" = \"plugin:bmux.tabs:set-tab-label mine\"\n\"alt+r\" = \"plugin:bmux.tabs:clear-tab-label\"\n\"alt+g\" = \"plugin:bmux.clients:unfollow\"\n\"alt+d\" = \"plugin:bmux.tabs:delete-view work\"\n\"alt+e\" = \"plugin:bmux.tabs:delete-view default\"\n",
        serde_json::to_string(&shell.to_string_lossy())?,
    );
    if let Some(leader) = leader {
        text.push_str(&format!(
            "\"alt+f\" = \"plugin:bmux.clients:follow {leader} --global\"\n"
        ));
    }
    Ok(text)
}

/// Every view of the principal `owner` reads, by name, with the context ids
/// of each one's arrangements in order.
async fn views(owner: &mut BmuxClient) -> Result<Vec<(String, bool, Vec<Uuid>)>> {
    let summaries = bmux_tabs_plugin_api::tabs_personal_views::client::list_views(owner)
        .await?
        .map_err(|error| anyhow::anyhow!("list views: {error:?}"))?;
    let mut views = Vec::new();
    for summary in summaries {
        let view =
            bmux_tabs_plugin_api::tabs_personal_views::client::get_view(owner, summary.view_id)
                .await?
                .map_err(|error| anyhow::anyhow!("get view: {error:?}"))?;
        views.push((
            view.name,
            view.is_default,
            view.workspaces
                .iter()
                .flat_map(|arrangement| &arrangement.members)
                .map(|member| member.context_id)
                .collect(),
        ));
    }
    views.sort();
    Ok(views)
}

async fn wait_without(frame: &Frame, label: &str, text: &str) -> Result<()> {
    let until = Instant::now() + STARTUP;
    while frame_contains(frame, text) {
        ensure!(
            Instant::now() < until,
            "view {label} kept showing {text:?}; frame: {:#?}",
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
    let (one, two) = (Uuid::new_v4(), Uuid::new_v4());
    let mut control = connect_as(&paths, Uuid::new_v4())
        .await
        .with_context(|| format!("isolated daemon readiness failed: {}", daemon.logs()))?;
    let (alpha, alpha_session) = create_tab(&mut control, "alpha").await?;
    let (_, beta_session) = create_tab(&mut control, "beta").await?;
    let (gamma, gamma_session) = create_tab(&mut control, "gamma").await?;
    // Principal one: attachments A and C, plus an unattached connection that
    // reads its views. Principal two: attachment B, which A follows.
    let client_a = connect_as(&paths, one).await?;
    let client_c = connect_as(&paths, one).await?;
    let mut client_b = connect_as(&paths, two).await?;
    let mut owner_one = connect_as(&paths, one).await?;
    let leader = bmux_clients_plugin_api::clients_state::client::current_client(&mut client_b)
        .await?
        .map_err(|error| anyhow::anyhow!("current client: {error:?}"))?
        .id;
    daemon.env.write_config(&config_text(
        &daemon.env.root().join("quiet-shell"),
        Some(leader),
    )?);
    let config = BmuxConfig::load_isolated_with_paths_and_overrides(
        &paths,
        &ConfigLoadOverrides::default(),
    )?;
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

            // A copies the shared default view into "work" and shows it; C
            // stays on the default view.
            a.try_send(key('c', true, false))?;
            wait_text(&frame_a, "a", "View: work").await?;
            ensure!(
                !frame_contains(&frame_c, "View:"),
                "C left the default view"
            );

            // Independent arrangements: A drops beta from work only.
            a.try_send(next())?;
            wait_attached(&a, beta_session, "a").await?;
            a.try_send(remove())?;
            wait_bar(&frame_a, "a", &["alpha", "gamma"], &["beta"]).await?;
            wait_attached(&a, gamma_session, "a").await?;
            wait_bar(&frame_c, "c", &["alpha", "beta", "gamma"], &[]).await?;
            ensure!(
                attached_session(&c) == Some(alpha_session),
                "another attachment of the owner moved"
            );

            // A personal label renames gamma in work only; a shared rename
            // reaches everyone else, and never overrides the label.
            a.try_send(key('m', true, false))?;
            wait_bar(&frame_a, "a", &["alpha", "mine"], &["gamma"]).await?;
            wait_bar(&frame_c, "c", &["alpha", "beta", "gamma"], &["mine"]).await?;
            bmux_tabs_plugin_api::tabs_commands::client::rename_tab_by_id(
                &mut control,
                gamma,
                "gamma2".to_string(),
            )
            .await?
            .map_err(|error| anyhow::anyhow!("rename gamma: {error:?}"))?;
            wait_bar(&frame_c, "c", &["alpha", "beta", "gamma2"], &[]).await?;
            wait_bar(&frame_b, "b", &["alpha", "beta", "gamma2"], &[]).await?;
            ensure!(
                bar_row(&frame_a, &["alpha", "mine"]).is_some_and(|row| !row.contains("gamma2")),
                "a shared rename replaced the personal label"
            );
            // Clearing the label shows the shared name again.
            a.try_send(key('r', true, false))?;
            wait_bar(&frame_a, "a", &["alpha", "gamma2"], &["mine"]).await?;

            // Follow: B moves to beta; A follows B and shows beta, which its
            // work view does not hold, without adding it there.
            b.try_send(next())?;
            wait_attached(&b, beta_session, "b").await?;
            let before = views(&mut owner_one).await?;
            a.try_send(key('f', true, false))?;
            wait_attached(&a, beta_session, "a").await?;
            // The hint is "Following CLIENT on beta (not in your view)",
            // truncated on a narrow bar.
            wait_text(&frame_a, "a", "on beta (not").await?;
            ensure!(
                views(&mut owner_one).await? == before,
                "following changed the follower's views"
            );
            ensure!(
                before
                    .iter()
                    .any(|(name, _, members)| name == "work" && *members == [alpha, gamma]),
                "work view: {before:?}"
            );
            // The leader's later tab switches carry the follower along, still
            // without touching its views: B goes to alpha (`alt+1`).
            b.try_send(key('1', true, false))?;
            wait_attached(&b, alpha_session, "b").await?;
            wait_attached(&a, alpha_session, "a").await?;
            wait_without(&frame_a, "a", "(not in your view)").await?;
            ensure!(
                views(&mut owner_one).await? == before,
                "following the leader's switch changed the follower's views"
            );
            ensure!(
                attached_session(&c) == Some(alpha_session),
                "the leader's switch moved an attachment that does not follow it"
            );
            // Ending the follow returns A to its own tab.
            a.try_send(key('g', true, false))?;
            wait_attached(&a, gamma_session, "a").await?;
            wait_without(&frame_a, "a", "Following").await?;
            ensure!(
                views(&mut owner_one).await? == before,
                "ending the follow changed the follower's views"
            );

            // Deleting work rebinds A to the default view; its tab is there.
            a.try_send(key('d', true, false))?;
            wait_without(&frame_a, "a", "View: work").await?;
            wait_bar(&frame_a, "a", &["alpha", "beta", "gamma2"], &[]).await?;
            ensure!(
                attached_session(&a) == Some(gamma_session),
                "A left its tab on rebinding"
            );
            let remaining = views(&mut owner_one).await?;
            ensure!(
                remaining.len() == 1 && remaining[0].0 == "default" && remaining[0].1,
                "after deleting work: {remaining:?}"
            );
            // The default view itself cannot be deleted.
            a.try_send(key('e', true, false))?;
            let settle = Instant::now() + Duration::from_secs(2);
            while Instant::now() < settle {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            ensure!(
                views(&mut owner_one).await? == remaining,
                "the default view was deleted"
            );
            wait_bar(&frame_a, "a", &["alpha", "beta", "gamma2"], &[]).await?;
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
        .with_context(|| {
            format!(
                "personal view management attach deadline: {}",
                daemon.logs()
            )
        })?;
    checks?;
    ensure!(
        outcome_a?.status_code == 0 && outcome_c?.status_code == 0 && outcome_b?.status_code == 0,
        "attach failed"
    );
    Ok(())
}

#[test]
fn views_labels_and_follow_stay_personal_through_real_attachments() -> Result<()> {
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
