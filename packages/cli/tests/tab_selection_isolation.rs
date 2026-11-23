//! Two real native attach views over one owned daemon: one client's tab switch
//! must not retarget another attached client that does not follow it.
#![cfg(unix)]

#[allow(dead_code)] // Shared process/path fixture supports other integration targets too.
mod support;

use std::{
    os::unix::fs::PermissionsExt as _,
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

use anyhow::{Context as _, Result, ensure};
use bmux_cli::attach::{
    NativeAttachEvent, NativeAttachHandle, NativeAttachHost, initialize_native_runtime_state,
    run_native_with_client,
};
use bmux_config::{BmuxConfig, ConfigLoadOverrides, ConfigPaths};
use bmux_frontend_runtime::input::{
    InputEvent, KeyEvent, KeyPhase, LogicalKey, Modifiers, PhysicalKey,
};

const LIMIT: Duration = Duration::from_secs(15);
/// Daemon startup and attach convergence deadline. Generous because a loaded
/// host can take far longer to boot a plugin-loaded debug server; behaviour
/// assertions below keep their own short settle windows.
const STARTUP: Duration = Duration::from_secs(120);

struct OwnedDaemon {
    env: support::ServerEnv,
    child: Child,
}

impl OwnedDaemon {
    fn start() -> Result<Self> {
        let env = support::ServerEnv::new("tab-selection-isolation");
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

fn host(paths: &ConfigPaths) -> Result<(NativeAttachHandle, NativeAttachHost)> {
    let (handle, mut host) = NativeAttachHost::channel(100, 24, 1, |_delivery| Ok(()))?;
    host.set_config_paths(paths.clone());
    Ok((handle, host))
}

async fn connect(paths: &ConfigPaths) -> Result<bmux_client::BmuxClient> {
    let until = Instant::now() + STARTUP;
    loop {
        match bmux_client::BmuxClient::connect_with_paths_and_timeout(
            paths,
            "tab-selection-isolation",
            Duration::from_secs(1),
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

/// The session this view currently presents, from its committed compositor.
fn attached_session(handle: &NativeAttachHandle) -> Option<uuid::Uuid> {
    handle
        .pane_zoom_target(None)
        .map(|target| target.identity.attached_id)
}

async fn wait_attached(
    handle: &NativeAttachHandle,
    session: uuid::Uuid,
    label: &str,
) -> Result<()> {
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

fn alt_digit(digit: char) -> NativeAttachEvent {
    NativeAttachEvent::Input(InputEvent::Key(KeyEvent {
        physical: PhysicalKey::Unidentified,
        logical: LogicalKey::Character(digit.to_string()),
        phase: KeyPhase::Press,
        modifiers: Modifiers {
            alt: true,
            ..Modifiers::default()
        },
        text: None,
    }))
}

async fn create_tab(
    control: &mut bmux_client::BmuxClient,
    name: &str,
) -> Result<(uuid::Uuid, uuid::Uuid)> {
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

async fn exercise() -> Result<()> {
    let daemon = OwnedDaemon::start()?;
    let paths = daemon.paths();
    let config = BmuxConfig::load_isolated_with_paths_and_overrides(
        &paths,
        &ConfigLoadOverrides::default(),
    )?;
    let mut control = connect(&paths).await.with_context(|| {
        format!(
            "isolated daemon readiness failed: stdout={:?} stderr={:?}",
            std::fs::read_to_string(daemon.env.root().join("server.stdout")),
            std::fs::read_to_string(daemon.env.root().join("server.stderr"))
        )
    })?;
    let (_first_tab, first_session) = create_tab(&mut control, "first").await?;
    let (_second_tab, second_session) = create_tab(&mut control, "second").await?;
    let client_a = connect(&paths).await?;
    let client_b = connect(&paths).await?;
    initialize_native_runtime_state(&config, &paths)?;
    let (a, host_a) = host(&paths)?;
    let (b, host_b) = host(&paths)?;
    let controller = async {
        let result: Result<()> = async {
            wait_attached(&a, first_session, "a").await?;
            wait_attached(&b, first_session, "b").await?;
            // Client A switches to the second tab through its own keybinding.
            a.try_send(alt_digit('2'))?;
            wait_attached(&a, second_session, "a").await?;
            // B does not follow A: give any broadcast selection time to land.
            let settle = Instant::now() + Duration::from_secs(2);
            while Instant::now() < settle {
                ensure!(
                    attached_session(&b) == Some(first_session),
                    "client A's switch dragged non-following client B to {:?}",
                    attached_session(&b)
                );
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            // And B's own switch still works and leaves A in place.
            b.try_send(alt_digit('2'))?;
            wait_attached(&b, second_session, "b").await?;
            a.try_send(alt_digit('1'))?;
            wait_attached(&a, first_session, "a").await?;
            let settle = Instant::now() + Duration::from_secs(1);
            while Instant::now() < settle {
                ensure!(
                    attached_session(&b) == Some(second_session),
                    "client A's switch back dragged client B to {:?}",
                    attached_session(&b)
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
    let target = first_session.to_string();
    let (outcome_a, outcome_b, checks) = tokio::time::timeout(Duration::from_secs(300), async {
        tokio::join!(
            run_native_with_client(client_a, &target, config.clone(), host_a),
            run_native_with_client(client_b, &target, config.clone(), host_b),
            controller,
        )
    })
    .await
    .context("two-view attach deadline")?;
    checks?;
    ensure!(
        outcome_a?.status_code == 0 && outcome_b?.status_code == 0,
        "attach failed"
    );
    Ok(())
}

#[test]
fn one_clients_tab_switch_does_not_retarget_a_non_following_peer() -> Result<()> {
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
