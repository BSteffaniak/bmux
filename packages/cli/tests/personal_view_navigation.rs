//! Personal tab navigation through real attachments of one owned daemon:
//! two principals' keyboard navigation (`goto-tab N`, `next-tab`) walks each
//! one's own view order, never the shared order or the other's arrangement.
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
use bmux_tabs_plugin_api::tabs_personal_commands as commands;
use bmux_tabs_plugin_api::tabs_personal_views as views;
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
        let env = support::ServerEnv::new("personal-view-navigation");
        std::fs::set_permissions(env.root(), std::fs::Permissions::from_mode(0o700))?;
        for name in ["home", "tmp", "cache"] {
            std::fs::create_dir(env.root().join(name))?;
        }
        // A quiet shell: no startup files, timers or background output.
        let shell = env.root().join("quiet-shell");
        std::fs::write(
            &shell,
            "#!/bin/sh\nstty -echo\nwhile IFS= read -r line; do [ \"$line\" = quit ] && exit 0; done\n",
        )?;
        std::fs::set_permissions(&shell, std::fs::Permissions::from_mode(0o700))?;
        env.write_config(&format!(
            "[general]\ndefault_shell = {}\n[behavior]\nrestore_last_layout = false\npane_shell_integration = false\n[multi_client]\ndefault_follow_mode = false\n",
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
            "personal-view-navigation",
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

/// `alt+N`: the default `goto-tab N` binding.
fn goto(index: char) -> NativeAttachEvent {
    key(index, true, false)
}

/// `ctrl+s`: the default `next-tab` binding.
fn next() -> NativeAttachEvent {
    key('s', false, true)
}

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
    let (_beta, beta_session) = create_tab(&mut control, "beta").await?;
    let (gamma, gamma_session) = create_tab(&mut control, "gamma").await?;
    let client_a = connect_as(&paths, one).await?;
    let client_b = connect_as(&paths, two).await?;
    // Principal one arranges its view from an attachment-less connection,
    // as a CLI would.
    let mut editor = connect_as(&paths, one).await?;
    initialize_native_runtime_state(&config, &paths)?;
    let (a, host_a, frame_a) = host(&paths)?;
    let (b, host_b, frame_b) = host(&paths)?;
    let controller = async {
        let result: Result<()> = async {
            // Both views are seeded from the shared order.
            wait_bar(&frame_a, "a", &["alpha", "beta", "gamma"], &[]).await?;
            wait_bar(&frame_b, "b", &["alpha", "beta", "gamma"], &[]).await?;
            wait_attached(&a, alpha_session, "a").await?;
            wait_attached(&b, alpha_session, "b").await?;
            // Principal one moves gamma first: its order is gamma, alpha,
            // beta; principal two keeps alpha, beta, gamma.
            let view = views::client::default_view(&mut editor)
                .await?
                .map_err(|error| anyhow::anyhow!("default view: {error:?}"))?;
            let id_of = |context: Uuid| {
                view.workspaces
                    .iter()
                    .flat_map(|workspace| &workspace.members)
                    .find(|member| member.context_id == context)
                    .map(|member| member.member_id)
                    .context("seeded member")
            };
            commands::client::move_tab(
                &mut editor,
                Uuid::new_v4(),
                view.view_id,
                view.revision,
                id_of(gamma)?,
                Some(id_of(alpha)?),
            )
            .await?
            .map_err(|error| anyhow::anyhow!("move tab: {error:?}"))?;
            wait_bar(&frame_a, "a", &["gamma", "alpha", "beta"], &[]).await?;
            wait_bar(&frame_b, "b", &["alpha", "beta", "gamma"], &[]).await?;

            // `goto-tab 3` is each principal's own third tab.
            a.try_send(goto('3'))?;
            wait_attached(&a, beta_session, "a").await?;
            b.try_send(goto('3'))?;
            wait_attached(&b, gamma_session, "b").await?;
            // `goto-tab 1` likewise.
            a.try_send(goto('1'))?;
            wait_attached(&a, gamma_session, "a").await?;
            b.try_send(goto('1'))?;
            wait_attached(&b, alpha_session, "b").await?;
            // `next-tab` steps through each principal's own order.
            a.try_send(next())?;
            wait_attached(&a, alpha_session, "a").await?;
            b.try_send(next())?;
            wait_attached(&b, beta_session, "b").await?;
            a.try_send(next())?;
            wait_attached(&a, beta_session, "a").await?;
            b.try_send(next())?;
            wait_attached(&b, gamma_session, "b").await?;
            // And wraps at each order's own end.
            a.try_send(next())?;
            wait_attached(&a, gamma_session, "a").await?;
            b.try_send(next())?;
            wait_attached(&b, alpha_session, "b").await?;
            // Neither navigation moved the other attachment.
            let settle = Instant::now() + Duration::from_secs(1);
            while Instant::now() < settle {
                ensure!(
                    attached_session(&a) == Some(gamma_session)
                        && attached_session(&b) == Some(alpha_session),
                    "one principal's navigation moved the other's attachment"
                );
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            Ok(())
        }
        .await;
        let close_a = a.try_send(NativeAttachEvent::Close);
        let close_b = b.try_send(NativeAttachEvent::Close);
        result?;
        close_a?;
        close_b?;
        Ok::<(), anyhow::Error>(())
    };
    let target = alpha_session.to_string();
    let (outcome_a, outcome_b, checks) = tokio::time::timeout(Duration::from_secs(400), async {
        tokio::join!(
            run_native_with_client(client_a, &target, config.clone(), host_a),
            run_native_with_client(client_b, &target, config.clone(), host_b),
            controller,
        )
    })
    .await
    .with_context(|| format!("personal navigation attach deadline: {}", daemon.logs()))?;
    checks?;
    ensure!(
        outcome_a?.status_code == 0 && outcome_b?.status_code == 0,
        "attach failed"
    );
    Ok(())
}

#[test]
fn each_principals_keyboard_navigation_walks_its_own_view_order() -> Result<()> {
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
