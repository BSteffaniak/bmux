//! Personal tab views across retries and server restarts of one isolated
//! daemon: replayed mutation ids never apply twice, closing a tab twice
//! removes it once, and a graceful or abrupt restart restores each owner's
//! arrangement, labels and recorded outcomes without bringing back removed
//! or closed tabs. A fresh attachment after the restart receives it.
#![cfg(unix)]

#[allow(dead_code)] // Shared process/path fixture supports other integration targets too.
mod support;

use std::{
    os::unix::fs::PermissionsExt as _,
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

use anyhow::{Context as _, Result, ensure};
use bmux_client::{BmuxClient, ServerEvent, StreamingBmuxClient};
use bmux_pane_runtime_plugin_api::attach_runtime_commands as attach;
use bmux_tabs_plugin_api::personal_view::{PersonalViewProjection, STATE_KIND};
use bmux_tabs_plugin_api::tabs_catalog_v1 as catalog;
use bmux_tabs_plugin_api::tabs_personal_commands as commands;
use bmux_tabs_plugin_api::tabs_personal_views::{self as views, PersonalView, PersonalViewError};
use uuid::Uuid;

/// Daemon readiness and convergence deadline. Generous because a loaded host
/// can take far longer to boot a plugin-loaded debug server.
const STARTUP: Duration = Duration::from_secs(120);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// One isolated daemon that is stopped, killed and started again over the
/// same configuration, data and state directories.
struct Daemon {
    env: support::ServerEnv,
    child: Option<Child>,
    starts: usize,
}

impl Daemon {
    fn new() -> Result<Self> {
        let env = support::ServerEnv::new("personal-view-restart");
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
        // Restoring the layout brings the same tabs back after a restart.
        env.write_config(&format!(
            "[general]\ndefault_shell = {}\n[behavior]\nrestore_last_layout = true\npane_shell_integration = false\n[multi_client]\ndefault_follow_mode = false\n",
            serde_json::to_string(&shell.to_string_lossy())?,
        ));
        Ok(Self {
            env,
            child: None,
            starts: 0,
        })
    }

    fn start(&mut self) -> Result<()> {
        ensure!(self.child.is_none(), "daemon already running");
        self.starts += 1;
        let log = |stream: &str| {
            std::fs::File::create(
                self.env
                    .root()
                    .join(format!("server-{}.{stream}", self.starts)),
            )
        };
        let child = isolated_command(
            &self.env,
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
        .stdout(log("stdout")?)
        .stderr(log("stderr")?)
        .spawn()?;
        self.child = Some(child);
        Ok(())
    }

    /// Run one CLI command against the daemon and wait for it.
    fn run(&self, args: &[&str]) -> Result<()> {
        let mut full = vec!["--runtime", "default", "--target", "local"];
        full.extend_from_slice(args);
        let mut command = isolated_command(&self.env, &full)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        let until = Instant::now() + STARTUP;
        loop {
            if let Some(status) = command.try_wait()? {
                ensure!(status.success(), "bmux {args:?} failed: {status}");
                return Ok(());
            }
            if Instant::now() >= until {
                let _ = command.kill();
                let _ = command.wait();
                anyhow::bail!("bmux {args:?} did not finish");
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Graceful stop: the daemon saves and exits, and the stop succeeds. A
    /// plugin background task that fails its teardown (for example a
    /// blocking step refused because teardown began) leaves an error receipt,
    /// which fails the stop command.
    fn stop(&mut self) -> Result<()> {
        self.run(&["server", "stop"])
            .context("graceful stop failed")?;
        let mut child = self.child.take().context("daemon not running")?;
        let until = Instant::now() + STARTUP;
        while child.try_wait()?.is_none() {
            if Instant::now() >= until {
                let _ = child.kill();
                let _ = child.wait();
                anyhow::bail!("daemon did not exit after stop");
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        Ok(())
    }

    /// Abrupt loss of the daemon.
    fn kill(&mut self) -> Result<()> {
        let mut child = self.child.take().context("daemon not running")?;
        child.kill()?;
        child.wait()?;
        Ok(())
    }

    fn logs(&self) -> String {
        (1..=self.starts)
            .map(|start| {
                format!(
                    "start {start}: stdout={:?} stderr={:?}\n",
                    std::fs::read_to_string(self.env.root().join(format!("server-{start}.stdout"))),
                    std::fs::read_to_string(self.env.root().join(format!("server-{start}.stderr")))
                )
            })
            .collect()
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        if self.child.is_some() && self.stop().is_err() {
            let _ = self.kill();
        }
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

/// A connection whose handshake binds it to `principal`, as a CLI running
/// with that principal would be. It has no attachment.
async fn connect_as(server: &Daemon, principal: Uuid) -> Result<BmuxClient> {
    let endpoint = bmux_ipc::IpcEndpoint::unix_socket(server.env.runtime_dir.join("server.sock"));
    let until = Instant::now() + STARTUP;
    loop {
        match BmuxClient::connect_with_principal(
            &endpoint,
            REQUEST_TIMEOUT,
            "personal-view-restart",
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

async fn create_tab(control: &mut BmuxClient, name: &str) -> Result<Uuid> {
    let ack = bmux_contexts_plugin_api::contexts_commands::client::create_context(
        control,
        Some(name.to_string()),
        std::collections::BTreeMap::new(),
    )
    .await?
    .map_err(|error| anyhow::anyhow!("create tab {name}: {error:?}"))?;
    Ok(ack.id)
}

/// Names of the live tabs in the shared catalog, sorted.
async fn catalog_names(client: &mut BmuxClient) -> Result<Vec<String>> {
    let mut names: Vec<_> = catalog::client::list_tabs(client)
        .await?
        .map_err(|error| anyhow::anyhow!("list tabs: {error}"))?
        .into_iter()
        .map(|tab| tab.name)
        .collect();
    names.sort();
    Ok(names)
}

async fn default_view(owner: &mut BmuxClient) -> Result<PersonalView> {
    views::client::default_view(owner)
        .await?
        .map_err(|error| anyhow::anyhow!("default view: {error:?}"))
}

/// `(context, label)` of each member of the view's only arranged workspace.
fn arrangement(view: &PersonalView) -> Vec<(Uuid, Option<String>)> {
    view.workspaces
        .iter()
        .flat_map(|workspace| &workspace.members)
        .map(|member| (member.context_id, member.label.clone()))
        .collect()
}

fn member(view: &PersonalView, context: Uuid) -> Result<Uuid> {
    view.workspaces
        .iter()
        .flat_map(|workspace| &workspace.members)
        .find(|member| member.context_id == context)
        .map(|member| member.member_id)
        .context("tab is not a member of the view")
}

fn same_ack(left: &commands::PersonalAck, right: &commands::PersonalAck) -> bool {
    left.view_id == right.view_id
        && left.revision == right.revision
        && left.member_id == right.member_id
}

/// Fixed mutation ids, so the same ids can be replayed after restarts.
struct Mutations {
    ensure: Uuid,
    seed: Uuid,
    move_first: Uuid,
    remove: Uuid,
    label: Uuid,
    create: Uuid,
    work: Uuid,
}

impl Mutations {
    fn new() -> Self {
        Self {
            ensure: Uuid::new_v4(),
            seed: Uuid::new_v4(),
            move_first: Uuid::new_v4(),
            remove: Uuid::new_v4(),
            label: Uuid::new_v4(),
            create: Uuid::new_v4(),
            work: Uuid::new_v4(),
        }
    }
}

struct Tabs {
    alpha: Uuid,
    beta: Uuid,
    gamma: Uuid,
}

/// Before any restart: arrange principal `one`'s default view, replaying
/// every mutation id once, and close a tab twice. Returns the recorded
/// removal acknowledgement for replay after the restarts.
async fn arrange_with_retries(
    server: &Daemon,
    one: Uuid,
    ids: &Mutations,
) -> Result<(Tabs, commands::PersonalAck, commands::PersonalAck)> {
    let mut control = connect_as(server, Uuid::new_v4()).await?;
    let alpha = create_tab(&mut control, "alpha").await?;
    let beta = create_tab(&mut control, "beta").await?;
    let gamma = create_tab(&mut control, "gamma").await?;
    let delta = create_tab(&mut control, "delta").await?;
    let mut owner = connect_as(server, one).await?;

    // ensure-default creates once.
    let ensured = commands::client::ensure_default(&mut owner, ids.ensure)
        .await?
        .map_err(|error| anyhow::anyhow!("ensure default: {error:?}"))?;
    let again = commands::client::ensure_default(&mut owner, ids.ensure)
        .await?
        .map_err(|error| anyhow::anyhow!("replayed ensure default: {error:?}"))?;
    ensure!(
        same_ack(&ensured, &again),
        "replayed ensure-default differs"
    );
    let view_id = ensured.view_id;

    // Adding a tab to an unarranged workspace seeds it once from the shared
    // order; replaying the add neither reseeds nor duplicates.
    let seeded =
        commands::client::add_tab(&mut owner, ids.seed, view_id, ensured.revision, alpha, None)
            .await?
            .map_err(|error| anyhow::anyhow!("seed: {error:?}"))?;
    let replayed =
        commands::client::add_tab(&mut owner, ids.seed, view_id, ensured.revision, alpha, None)
            .await?
            .map_err(|error| anyhow::anyhow!("replayed seed: {error:?}"))?;
    ensure!(same_ack(&seeded, &replayed), "replayed add-tab differs");
    let view = default_view(&mut owner).await?;
    ensure!(
        view.revision == seeded.revision
            && arrangement(&view)
                == vec![(alpha, None), (beta, None), (gamma, None), (delta, None)],
        "seeding is not the shared order exactly once: {view:?}"
    );

    // Move gamma first, replayed.
    let moved = commands::client::move_tab(
        &mut owner,
        ids.move_first,
        view_id,
        view.revision,
        member(&view, gamma)?,
        Some(member(&view, alpha)?),
    )
    .await?
    .map_err(|error| anyhow::anyhow!("move: {error:?}"))?;
    let replayed = commands::client::move_tab(
        &mut owner,
        ids.move_first,
        view_id,
        view.revision,
        member(&view, gamma)?,
        Some(member(&view, alpha)?),
    )
    .await?
    .map_err(|error| anyhow::anyhow!("replayed move: {error:?}"))?;
    ensure!(same_ack(&moved, &replayed), "replayed move-tab differs");

    // Remove beta from the view, replayed; reusing the id for another
    // request is refused.
    let removed = commands::client::remove_tab(
        &mut owner,
        ids.remove,
        view_id,
        moved.revision,
        member(&view, beta)?,
    )
    .await?
    .map_err(|error| anyhow::anyhow!("remove: {error:?}"))?;
    let replayed = commands::client::remove_tab(
        &mut owner,
        ids.remove,
        view_id,
        moved.revision,
        member(&view, beta)?,
    )
    .await?
    .map_err(|error| anyhow::anyhow!("replayed remove: {error:?}"))?;
    ensure!(same_ack(&removed, &replayed), "replayed remove-tab differs");
    let conflict = commands::client::remove_tab(
        &mut owner,
        ids.remove,
        view_id,
        removed.revision,
        member(&view, alpha)?,
    )
    .await?;
    ensure!(
        matches!(conflict, Err(PersonalViewError::MutationConflict)),
        "a reused mutation id with another request was not refused: {conflict:?}"
    );

    // Label alpha, replayed.
    let labeled = commands::client::set_label(
        &mut owner,
        ids.label,
        view_id,
        removed.revision,
        member(&view, alpha)?,
        Some("Apple".into()),
    )
    .await?
    .map_err(|error| anyhow::anyhow!("label: {error:?}"))?;
    let replayed = commands::client::set_label(
        &mut owner,
        ids.label,
        view_id,
        removed.revision,
        member(&view, alpha)?,
        Some("Apple".into()),
    )
    .await?
    .map_err(|error| anyhow::anyhow!("replayed label: {error:?}"))?;
    ensure!(same_ack(&labeled, &replayed), "replayed set-label differs");
    let view = default_view(&mut owner).await?;
    ensure!(
        view.revision == labeled.revision
            && arrangement(&view)
                == vec![(gamma, None), (alpha, Some("Apple".into())), (delta, None)],
        "replays changed the arrangement: {view:?}"
    );

    // create-view creates one view however often it is retried.
    let created = commands::client::create_view(&mut owner, ids.create, ids.work, "work".into())
        .await?
        .map_err(|error| anyhow::anyhow!("create view: {error:?}"))?;
    let replayed = commands::client::create_view(&mut owner, ids.create, ids.work, "work".into())
        .await?
        .map_err(|error| anyhow::anyhow!("replayed create view: {error:?}"))?;
    ensure!(
        same_ack(&created, &replayed),
        "replayed create-view differs"
    );
    let conflict =
        commands::client::create_view(&mut owner, ids.create, Uuid::new_v4(), "work".into())
            .await?;
    ensure!(
        matches!(conflict, Err(PersonalViewError::MutationConflict)),
        "a reused create-view id with another view was not refused: {conflict:?}"
    );
    let listed = views::client::list_views(&mut owner)
        .await?
        .map_err(|error| anyhow::anyhow!("list views: {error:?}"))?;
    ensure!(
        listed.len() == 2 && listed.iter().filter(|view| view.name == "work").count() == 1,
        "retried creation duplicated a view: {listed:?}"
    );

    // Closing delta for everyone twice closes it once: the retry finds
    // nothing, and confirmed deletion prunes it from the view.
    bmux_tabs_plugin_api::tabs_commands::client::kill_tab(&mut control, delta.to_string(), false)
        .await?
        .map_err(|error| anyhow::anyhow!("close delta: {error:?}"))?;
    let retried = bmux_tabs_plugin_api::tabs_commands::client::kill_tab(
        &mut control,
        delta.to_string(),
        false,
    )
    .await?;
    ensure!(
        retried.is_err(),
        "closing an already closed tab succeeded again: {retried:?}"
    );
    let until = Instant::now() + STARTUP;
    loop {
        let view = default_view(&mut owner).await?;
        if arrangement(&view) == vec![(gamma, None), (alpha, Some("Apple".into()))] {
            break;
        }
        ensure!(
            Instant::now() < until,
            "closing delta never pruned it from the view: {view:?}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    ensure!(
        catalog_names(&mut owner).await? == ["alpha", "beta", "gamma"],
        "closing twice touched another tab"
    );
    Ok((Tabs { alpha, beta, gamma }, removed, created))
}

/// After a restart: the arrangement, labels, views and recorded outcomes
/// are back, the removed tab still runs outside the view and the closed one
/// stays closed.
async fn verify_restored(
    server: &Daemon,
    one: Uuid,
    ids: &Mutations,
    tabs: &Tabs,
    expected: &[(Uuid, Option<String>)],
    recorded: (&commands::PersonalAck, &commands::PersonalAck),
) -> Result<()> {
    let mut owner = connect_as(server, one).await?;
    let until = Instant::now() + STARTUP;
    loop {
        if catalog_names(&mut owner).await? == ["alpha", "beta", "gamma"] {
            break;
        }
        ensure!(
            Instant::now() < until,
            "restart did not restore exactly the open tabs: {:?}",
            catalog_names(&mut owner).await?
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let view = default_view(&mut owner).await?;
    ensure!(
        arrangement(&view) == expected,
        "restart did not restore the arrangement: {view:?}"
    );
    ensure!(
        member(&view, tabs.beta).is_err(),
        "restart resurrected a removed tab"
    );
    let listed = views::client::list_views(&mut owner)
        .await?
        .map_err(|error| anyhow::anyhow!("list views: {error:?}"))?;
    ensure!(
        listed.len() == 2 && listed.iter().any(|view| view.view_id == ids.work),
        "restart lost or duplicated a view: {listed:?}"
    );
    // Recorded outcomes survive: replays still return the original
    // acknowledgements and change nothing.
    let (removed, created) = recorded;
    let replayed = commands::client::remove_tab(
        &mut owner,
        ids.remove,
        removed.view_id,
        removed.revision.saturating_sub(1),
        Uuid::nil(),
    )
    .await?;
    ensure!(
        matches!(replayed, Err(PersonalViewError::MutationConflict)),
        "a different request under a recorded id was accepted after restart: {replayed:?}"
    );
    let unchanged = default_view(&mut owner).await?;
    ensure!(
        unchanged.revision == view.revision,
        "a refused replay changed the view"
    );
    let replayed = commands::client::create_view(&mut owner, ids.create, ids.work, "work".into())
        .await?
        .map_err(|error| anyhow::anyhow!("create view replay after restart: {error:?}"))?;
    ensure!(
        same_ack(&replayed, created),
        "create-view replay after restart differs"
    );
    ensure!(
        views::client::list_views(&mut owner)
            .await?
            .map_err(|error| anyhow::anyhow!("list views: {error:?}"))?
            .len()
            == 2,
        "create-view replay after restart duplicated the view"
    );

    // A fresh attachment (every reconnect is one) receives the restored
    // arrangement with its own tab active.
    let projection = attach_and_wait(server, one, tabs.alpha, |projection| {
        let shown: Vec<_> = projection
            .workspaces
            .iter()
            .flat_map(|workspace| &workspace.members)
            .map(|member| member.context_id)
            .collect();
        shown
            == expected
                .iter()
                .map(|(context, _)| *context)
                .collect::<Vec<_>>()
    })
    .await?;
    let alpha = projection
        .workspaces
        .iter()
        .flat_map(|workspace| &workspace.members)
        .find(|member| member.context_id == tabs.alpha)
        .context("alpha missing from the projection")?;
    ensure!(
        alpha.active && alpha.name == "Apple",
        "the attachment shows the wrong active tab or label: {projection:?}"
    );
    ensure!(
        projection
            .workspaces
            .iter()
            .flat_map(|workspace| &workspace.members)
            .all(|member| member.context_id != tabs.beta),
        "the projection resurrected the removed tab"
    );
    Ok(())
}

/// Attach `principal` to `tab` and wait for a delivered projection
/// satisfying `ready`.
async fn attach_and_wait(
    server: &Daemon,
    principal: Uuid,
    tab: Uuid,
    ready: impl Fn(&PersonalViewProjection) -> bool,
) -> Result<PersonalViewProjection> {
    let mut client = StreamingBmuxClient::from_client(connect_as(server, principal).await?)?;
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
    let until = tokio::time::Instant::now() + STARTUP;
    let mut last = None;
    loop {
        match tokio::time::timeout_at(until, client.event_receiver().recv()).await {
            Ok(Some(ServerEvent::PluginBusEvent { kind, payload }))
                if kind == STATE_KIND.as_str() =>
            {
                let projection: PersonalViewProjection = serde_json::from_slice(&payload)?;
                if ready(&projection) {
                    return Ok(projection);
                }
                last = Some(projection);
            }
            Ok(Some(_)) => {}
            Ok(None) => anyhow::bail!("attachment connection closed"),
            Err(_) => anyhow::bail!("the attachment never received the arrangement: {last:?}"),
        }
    }
}

/// Reorder once more, so the abrupt restart has something new to keep.
async fn move_alpha_first(server: &Daemon, one: Uuid, tabs: &Tabs) -> Result<()> {
    let mut owner = connect_as(server, one).await?;
    let view = default_view(&mut owner).await?;
    commands::client::move_tab(
        &mut owner,
        Uuid::new_v4(),
        view.view_id,
        view.revision,
        member(&view, tabs.alpha)?,
        Some(member(&view, tabs.gamma)?),
    )
    .await?
    .map_err(|error| anyhow::anyhow!("move: {error:?}"))?;
    Ok(())
}

fn exercise(runtime: &tokio::runtime::Runtime) -> Result<()> {
    let mut server = Daemon::new()?;
    server.start()?;
    let one = Uuid::new_v4();
    let ids = Mutations::new();
    let (tabs, removed, created) = runtime
        .block_on(arrange_with_retries(&server, one, &ids))
        .with_context(|| server.logs())?;
    let labeled = vec![(tabs.gamma, None), (tabs.alpha, Some("Apple".into()))];

    // Graceful restart.
    server.stop().with_context(|| server.logs())?;
    server.start()?;
    runtime
        .block_on(verify_restored(
            &server,
            one,
            &ids,
            &tabs,
            &labeled,
            (&removed, &created),
        ))
        .with_context(|| format!("after a graceful restart: {}", server.logs()))?;

    // Abrupt restart after a saved edit.
    runtime
        .block_on(move_alpha_first(&server, one, &tabs))
        .with_context(|| server.logs())?;
    server
        .run(&["server", "save"])
        .with_context(|| server.logs())?;
    server.kill()?;
    server.start()?;
    let reordered = vec![(tabs.alpha, Some("Apple".into())), (tabs.gamma, None)];
    runtime
        .block_on(verify_restored(
            &server,
            one,
            &ids,
            &tabs,
            &reordered,
            (&removed, &created),
        ))
        .with_context(|| format!("after an abrupt restart: {}", server.logs()))?;
    Ok(())
}

#[test]
fn retries_and_restarts_keep_each_personal_arrangement_exact() -> Result<()> {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("warn")
        .with_test_writer()
        .try_init();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;
    exercise(&runtime)
}
