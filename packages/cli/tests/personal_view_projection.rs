//! Personal tab views through real attachments of one owned daemon: each
//! attachment receives only its own owner's view, the tab bar shows it, and
//! two attachments sharing a view share its edits but not their active tab.
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
use bmux_client::{BmuxClient, ServerEvent, StreamingBmuxClient};
use bmux_config::{BmuxConfig, ConfigLoadOverrides, ConfigPaths};
use bmux_pane_runtime_plugin_api::attach_runtime_commands as attach;
use bmux_tabs_plugin_api::personal_view::{PersonalViewProjection, STATE_KIND};
use bmux_tabs_plugin_api::tabs_personal_attachment as attachment;
use bmux_tabs_plugin_api::tabs_personal_commands as commands;
use bmux_tabs_plugin_api::tabs_personal_views as views;
use uuid::Uuid;

const LIMIT: Duration = support::HANG_GUARD;
/// Daemon startup and attach convergence deadline. Generous because a loaded
/// host can take far longer to boot a plugin-loaded debug server.
const STARTUP: Duration = Duration::from_secs(120);
const REQUEST_TIMEOUT: Duration = support::HANG_GUARD;

struct OwnedDaemon {
    env: support::ServerEnv,
    child: Child,
}

impl OwnedDaemon {
    fn start() -> Result<Self> {
        let env = support::ServerEnv::new("personal-view-projection");
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
            "personal-view-projection",
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

/// A real attachment over the attach runtime service, observed at the wire:
/// it records every projection the server delivers to it.
struct WireAttachment {
    client: StreamingBmuxClient,
    projections: Vec<PersonalViewProjection>,
}

impl WireAttachment {
    async fn open(paths: &ConfigPaths, principal: Uuid, tab: Uuid) -> Result<Self> {
        let mut client = StreamingBmuxClient::from_client(connect_as(paths, principal).await?)?;
        let grant = attach::client::attach_context(
            &mut client,
            attach::ContextSelector {
                id: Some(tab),
                name: None,
            },
            true,
        )
        .await?
        .map_err(|error| anyhow::anyhow!("attach grant: {error:?}"))?;
        attach::client::attach_open(&mut client, grant.session_id, grant.token)
            .await?
            .map_err(|error| anyhow::anyhow!("attach open: {error:?}"))?;
        client.enable_event_push().await?;
        Ok(Self {
            client,
            projections: Vec::new(),
        })
    }

    /// Drain delivered events for a moment.
    async fn drain(&mut self) -> Result<()> {
        let deadline = tokio::time::Instant::now() + Duration::from_millis(100);
        loop {
            match tokio::time::timeout_at(deadline, self.client.event_receiver().recv()).await {
                Ok(Some(ServerEvent::PluginBusEvent { kind, payload }))
                    if kind == STATE_KIND.as_str() =>
                {
                    self.projections.push(serde_json::from_slice(&payload)?);
                }
                Ok(Some(_)) => {}
                Ok(None) => anyhow::bail!("attachment connection closed"),
                Err(_) => return Ok(()),
            }
        }
    }

    /// Wait for a delivered projection satisfying `ready`.
    async fn wait(
        &mut self,
        label: &str,
        ready: impl Fn(&PersonalViewProjection) -> bool,
    ) -> Result<PersonalViewProjection> {
        let until = Instant::now() + STARTUP;
        loop {
            self.drain().await?;
            if let Some(projection) = self.projections.last().filter(|last| ready(last)) {
                return Ok(projection.clone());
            }
            ensure!(
                Instant::now() < until,
                "attachment {label} never received the expected projection; last: {:?}",
                self.projections.last()
            );
        }
    }
}

/// `(name, active)` of each tab of the default workspace's arrangement.
fn shown(projection: &PersonalViewProjection) -> Vec<(String, bool)> {
    projection
        .workspaces
        .iter()
        .find(|workspace| workspace.workspace_id == projection.active_workspace_id)
        .map(|workspace| {
            workspace
                .members
                .iter()
                .map(|member| (member.name.clone(), member.active))
                .collect()
        })
        .unwrap_or_default()
}

fn member(projection: &PersonalViewProjection, context: Uuid) -> Result<Uuid> {
    projection
        .workspaces
        .iter()
        .flat_map(|workspace| &workspace.members)
        .find(|member| member.context_id == context)
        .map(|member| member.member_id)
        .context("tab is not a member of the view")
}

fn pairs(names: &[(&str, bool)]) -> Vec<(String, bool)> {
    names
        .iter()
        .map(|(name, active)| ((*name).to_string(), *active))
        .collect()
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
    let (beta, _) = create_tab(&mut control, "beta").await?;
    let client_a = connect_as(&paths, one).await?;
    let client_b = connect_as(&paths, two).await?;
    // Principal `one` edits its views from an attachment-less connection,
    // as a CLI would.
    let mut editor = connect_as(&paths, one).await?;
    initialize_native_runtime_state(&config, &paths)?;
    let (a, host_a, frame_a) = host(&paths)?;
    let (b, host_b, frame_b) = host(&paths)?;
    let controller = async {
        let result: Result<()> = async {
            // Both owners' default views are seeded from the shared order.
            wait_bar(&frame_a, "a", &["alpha", "beta"], &[]).await?;
            wait_bar(&frame_b, "b", &["alpha", "beta"], &[]).await?;
            // Principal one relabels alpha and moves beta first.
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
            let ack = commands::client::set_label(
                &mut editor,
                Uuid::new_v4(),
                view.view_id,
                view.revision,
                id_of(alpha)?,
                Some("Apple".into()),
            )
            .await?
            .map_err(|error| anyhow::anyhow!("set label: {error:?}"))?;
            commands::client::move_tab(
                &mut editor,
                Uuid::new_v4(),
                view.view_id,
                ack.revision,
                id_of(beta)?,
                Some(id_of(alpha)?),
            )
            .await?
            .map_err(|error| anyhow::anyhow!("move tab: {error:?}"))?;
            // Only principal one's attachment shows it.
            wait_bar(&frame_a, "a", &["beta", "Apple"], &["alpha"]).await?;
            let settle = Instant::now() + Duration::from_secs(2);
            while Instant::now() < settle {
                ensure!(
                    bar_row(&frame_b, &["alpha", "beta"]).is_some_and(|row| {
                        !row.contains("Apple") && row.find("alpha") < row.find("beta")
                    }),
                    "principal one's arrangement reached principal two's tab bar: {:#?}",
                    frame_b.lock().map(|rows| rows.clone()).unwrap_or_default()
                );
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            // Restore one's order, then check projections at the wire.
            let current = views::client::default_view(&mut editor)
                .await?
                .map_err(|error| anyhow::anyhow!("default view: {error:?}"))?;
            commands::client::move_tab(
                &mut editor,
                Uuid::new_v4(),
                view.view_id,
                current.revision,
                id_of(beta)?,
                None,
            )
            .await?
            .map_err(|error| anyhow::anyhow!("move tab: {error:?}"))?;
            wait_bar(&frame_a, "a", &["Apple", "beta"], &["alpha"]).await?;
            wire_checks(&paths, one, two, alpha, beta).await
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
    .with_context(|| format!("personal view attach deadline: {}", daemon.logs()))?;
    checks?;
    ensure!(
        outcome_a?.status_code == 0 && outcome_b?.status_code == 0,
        "attach failed"
    );
    Ok(())
}

/// Wire-level checks with three more attachments: two of principal `one`
/// (sharing its default view, which already labels alpha "Apple") and one of
/// principal `two`.
async fn wire_checks(
    paths: &ConfigPaths,
    one: Uuid,
    two: Uuid,
    alpha: Uuid,
    beta: Uuid,
) -> Result<()> {
    let mut first = WireAttachment::open(paths, one, alpha).await?;
    let mut second = WireAttachment::open(paths, one, alpha).await?;
    let mut other = WireAttachment::open(paths, two, alpha).await?;
    let labeled = |projection: &PersonalViewProjection| {
        shown(projection) == pairs(&[("Apple", true), ("beta", false)])
    };
    let view = first.wait("first", labeled).await?;
    second.wait("second", labeled).await?;
    let foreign = other
        .wait("other", |projection| {
            shown(projection) == pairs(&[("alpha", true), ("beta", false)])
        })
        .await?;
    ensure!(view.view_id != foreign.view_id, "owners share no view");

    // The second attachment selects beta through its binding; the first
    // keeps alpha. Both see one arrangement.
    let binding = attachment::client::current_binding(&mut second.client)
        .await?
        .map_err(|error| anyhow::anyhow!("current binding: {error:?}"))?;
    ensure!(binding.view_id == view.view_id, "bound to the default view");
    let selected = attachment::client::select_member(
        &mut second.client,
        member(&view, beta)?,
        binding.revision,
    )
    .await?
    .map_err(|error| anyhow::anyhow!("select member: {error:?}"))?;
    ensure!(
        selected.revision > binding.revision,
        "selection advanced the binding"
    );
    second
        .wait("second", |projection| {
            shown(projection) == pairs(&[("Apple", false), ("beta", true)])
        })
        .await?;

    // An edit by one attachment reaches both, each with its own active tab.
    let current = views::client::get_view(&mut first.client, view.view_id)
        .await?
        .map_err(|error| anyhow::anyhow!("get view: {error:?}"))?;
    commands::client::set_label(
        &mut first.client,
        Uuid::new_v4(),
        view.view_id,
        current.revision,
        member(&view, beta)?,
        Some("Banana".into()),
    )
    .await?
    .map_err(|error| anyhow::anyhow!("set label: {error:?}"))?;
    first
        .wait("first", |projection| {
            shown(projection) == pairs(&[("Apple", true), ("Banana", false)])
        })
        .await?;
    second
        .wait("second", |projection| {
            shown(projection) == pairs(&[("Apple", false), ("Banana", true)])
        })
        .await?;

    // The other owner only ever received its own, unedited view.
    other.drain().await?;
    for projection in first.projections.iter().chain(&second.projections) {
        ensure!(projection.view_id == view.view_id, "foreign view delivered");
    }
    ensure!(
        other
            .projections
            .iter()
            .all(|projection| projection.view_id == foreign.view_id
                && shown(projection)
                    .iter()
                    .all(|(name, _)| name != "Apple" && name != "Banana")),
        "another owner's view reached this attachment: {:?}",
        other.projections
    );
    Ok(())
}

#[test]
fn each_attachment_receives_only_its_own_personal_view() -> Result<()> {
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
