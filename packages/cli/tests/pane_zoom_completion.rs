//! Real owned daemon and CLI native attach reducer, with a complete retained
//! headless sink. This is NOT Bterm Application/GPU/native-window acceptance.
#![cfg(unix)]

#[allow(dead_code)] // Shared process/path fixture supports other integration targets too.
mod support;

// Exercise the actual Bterm caller-owned sizing controller over a real owned
// daemon, without introducing a production CLI dependency on Bterm.
#[path = "../../bterm/src/positioned_zoom_sizing.rs"]
mod positioned_zoom_sizing;

use std::{
    os::unix::fs::PermissionsExt as _,
    process::{Child, Command, Stdio},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use anyhow::{Context as _, Result, bail, ensure};
use bmux_cli::attach::{
    NativeAttachEvent, NativeAttachHandle, NativeAttachHost, NativePaneZoomRequestStatus,
    NativePaneZoomTarget, initialize_native_runtime_state, run_native_with_client,
};
use bmux_config::{BmuxConfig, ConfigLoadOverrides, ConfigPaths};
use bmux_frontend_runtime::{
    input::{InputEvent, KeyEvent, KeyPhase, LogicalKey, Modifiers, NamedKey, PhysicalKey},
    presentation::PreparedDelivery,
};
use bmux_pane_runtime_plugin_api::{
    pane_runtime_commands::client as commands, pane_runtime_pointer_grid_v1::client as grids,
    pane_runtime_state::client as state,
};

const LIMIT: Duration = Duration::from_secs(15);

struct OwnedDaemon {
    env: support::ServerEnv,
    child: Child,
}

impl OwnedDaemon {
    fn start() -> Result<Self> {
        let env = support::ServerEnv::new("pane-zoom-completion");
        std::fs::set_permissions(env.root(), std::fs::Permissions::from_mode(0o700))?;
        for name in ["home", "tmp", "cache"] {
            std::fs::create_dir(env.root().join(name))?;
        }
        let shell = env.root().join("zoom-source");
        // No shell startup files, timers or background writers. Long logical text
        // and graphemes enter the actual terminal parser, not a reconstructed grid.
        std::fs::write(
            &shell,
            "#!/bin/sh\nstty -echo\nprintf 'ZOOM_SOURCE é界👩‍💻 abcdefghijklmnopqrstuvwxyz0123456789\\r\\n'\nwhile IFS= read -r line; do [ \"$line\" = quit ] && exit 0; if [ \"$line\" = history ]; then n=0; while [ \"$n\" -lt 80 ]; do printf 'HISTORY_%03d é界👩‍💻 abcdefghijklmnopqrstuvwxyz0123456789\\r\\n' \"$n\"; n=$((n+1)); done; fi; if [ \"$line\" = alternate ]; then printf '\\033[?1049h\\033[H'; fi; if [ \"$line\" = normal ]; then printf '\\033[?1049l'; fi; if [ \"$line\" = size ]; then printf 'PTY_SIZE '; stty size; fi; done\n",
        )?;
        std::fs::set_permissions(&shell, std::fs::Permissions::from_mode(0o700))?;
        env.write_config(&format!(
            "[general]\ndefault_shell = {}\n[behavior]\nrestore_last_layout = false\npane_shell_integration = false\n[behavior.scrollback]\nmode = 'frozen'\n[keybindings.modes.insert.bindings]\n'ctrl+g' = 'enter_scroll_mode'\n",
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
        // Explicit isolated endpoint, then bounded fallback on our exact child.
        // Graceful stop lets the daemon reap its PTYs/helper before root removal.
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

#[derive(Default)]
struct Sink {
    // A bounded last complete output, never an independent hit-test scene.
    committed: Option<PreparedDelivery>,
    reject: bool,
    rejected: u64,
    attempts: u64,
}

fn host(
    sink: Arc<Mutex<Sink>>,
    paths: &ConfigPaths,
) -> Result<(NativeAttachHandle, NativeAttachHost)> {
    let (handle, mut host) = NativeAttachHost::channel(100, 24, 1, move |delivery| {
        let mut sink = sink.lock().unwrap();
        sink.attempts += 1;
        ensure!(
            delivery.prepared.buffer().cells().len() <= 100 * 24,
            "unbounded root output"
        );
        if sink.reject {
            sink.rejected += 1;
            bail!("injected complete output failure");
        }
        sink.committed = Some(delivery);
        Ok(())
    })?;
    host.set_config_paths(paths.clone());
    Ok((handle, host))
}

async fn target(
    handle: &NativeAttachHandle,
    point: Option<[f64; 2]>,
    scale: u16,
) -> Result<NativePaneZoomTarget> {
    let until = Instant::now() + LIMIT;
    loop {
        if let Some(target) = handle.pane_zoom_target(point)
            && target.scale_millis == scale
        {
            return Ok(target);
        }
        ensure!(
            Instant::now() < until,
            "committed zoom target timeout: point={point:?} scale={scale}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

async fn connect(paths: &ConfigPaths) -> Result<bmux_client::BmuxClient> {
    let until = Instant::now() + LIMIT;
    loop {
        match bmux_client::BmuxClient::connect_with_paths_and_timeout(
            paths,
            "pane-zoom-completion",
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

async fn canonical(
    client: &mut bmux_client::BmuxClient,
    session: uuid::Uuid,
    pane: uuid::Uuid,
) -> Result<serde_json::Value> {
    use bmux_pane_runtime_plugin_api::attach_runtime_commands::{self, client as attach};
    let grant = attach::attach_session(
        client,
        attach_runtime_commands::SessionSelector {
            id: Some(session),
            name: None,
        },
        true,
    )
    .await?
    .map_err(|error| anyhow::anyhow!("snapshot attach: {error:?}"))?;
    attach::attach_open(client, session, grant.token)
        .await?
        .map_err(|error| anyhow::anyhow!("snapshot attach open: {error:?}"))?;
    let state = grids::snapshots(client, 1, session, vec![pane], 128)
        .await?
        .map_err(|error| anyhow::anyhow!("grid snapshot: {error:?}"))?;
    let snapshot = state
        .snapshots
        .into_iter()
        .find(|snapshot| snapshot.pane_id == pane)
        .context("pane missing from canonical snapshot")?;
    Ok(serde_json::from_slice(&snapshot.encoded)?)
}

fn native_key(
    logical: LogicalKey,
    modifiers: Modifiers,
    text: Option<String>,
) -> NativeAttachEvent {
    NativeAttachEvent::Input(InputEvent::Key(KeyEvent {
        physical: PhysicalKey::Unidentified,
        logical,
        phase: KeyPhase::Press,
        modifiers,
        text,
    }))
}

async fn captured_history_keyboard_zoom(
    handle: &NativeAttachHandle,
    other_view: &NativeAttachHandle,
    sink: &Mutex<Sink>,
) -> Result<()> {
    let live = target(handle, None, 1750).await?;
    handle.try_send(NativeAttachEvent::SetPaneZoom {
        target: live,
        scale_millis: 1000,
    })?;
    target(handle, None, 1000).await?;
    // Feed the owned PTY through native key encoding, not direct pane-input IPC.
    for character in "history".chars() {
        handle.try_send(native_key(
            LogicalKey::Character(character.to_string()),
            Modifiers::default(),
            Some(character.to_string()),
        ))?;
    }
    handle.try_send(native_key(
        LogicalKey::Named(NamedKey::Enter),
        Modifiers::default(),
        None,
    ))?;
    let until = Instant::now() + LIMIT;
    loop {
        let ready = {
            let sink = sink.lock().unwrap();
            sink.committed.as_ref().is_some_and(|output| {
                output.prepared.grid_fragments().iter().any(|fragment| {
                    let buffer = fragment.buffer();
                    (0..buffer.area().height).any(|row| {
                        buffer
                            .row_symbols(row)
                            .is_some_and(|text| text.contains("HISTORY_079"))
                    })
                })
            })
        };
        if ready {
            break;
        }
        ensure!(
            Instant::now() < until,
            "native history workload did not reach complete output"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    // Explicit insert-mode binding routes the real EnterScrollMode action.
    // No synthetic pin or reconstructed frozen snapshot is installed by the test.
    handle.try_send(native_key(
        LogicalKey::Character("g".into()),
        Modifiers {
            control: true,
            ..Modifiers::default()
        },
        None,
    ))?;
    let until = Instant::now() + LIMIT;
    let baseline = loop {
        let dimensions = {
            let sink = sink.lock().unwrap();
            sink.committed.as_ref().and_then(|output| {
                output
                    .prepared
                    .grid_fragments()
                    .iter()
                    .find(|fragment| fragment.id().starts_with("pane-zoom-history:"))
                    .map(|fragment| {
                        (
                            fragment.buffer().area().width,
                            fragment.buffer().area().height,
                        )
                    })
            })
        };
        if let Some(dimensions) = dimensions {
            break dimensions;
        }
        if Instant::now() >= until {
            let state = sink.lock().unwrap();
            if let Some(output) = &state.committed {
                eprintln!(
                    "history deadline fragments {:?}",
                    output
                        .prepared
                        .grid_fragments()
                        .iter()
                        .map(|fragment| fragment.id())
                        .collect::<Vec<_>>()
                );
                for y in 0..output.prepared.buffer().area().height {
                    eprintln!("root row {y} {:?}", output.prepared.buffer().row_symbols(y));
                }
            }
        }
        ensure!(
            Instant::now() < until,
            "EnterScrollMode did not commit captured-history grid"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    };
    let frozen = target(handle, None, 1000).await?;
    handle.try_send(NativeAttachEvent::SetPaneZoom {
        target: frozen,
        scale_millis: 500,
    })?;
    let until = Instant::now() + LIMIT;
    loop {
        let current = handle.pane_zoom_target(None);
        let converged = {
            let sink = sink.lock().unwrap();
            if let (Some(output), Some(current)) = (&sink.committed, current) {
                if output.identity == current.identity && current.scale_millis == 500 {
                    if let Some(fragment) = output
                        .prepared
                        .grid_fragments()
                        .iter()
                        .find(|fragment| fragment.id().starts_with("pane-zoom-history:"))
                    {
                        let area = fragment.buffer().area();
                        if area.width > baseline.0 && area.height > baseline.1 {
                            let selection = output.prepared.selection();
                            let text = output.prepared.selection_text();
                            ensure!(
                                !selection.fragments().is_empty(),
                                "history zoom lost source selection"
                            );
                            ensure!(
                                text.values().any(|text| text.contains("HISTORY_")),
                                "history source text unavailable"
                            );
                            for fragment in selection.fragments() {
                                let source = text
                                    .get(&fragment.content_id)
                                    .context("selection content missing")?;
                                ensure!(
                                    fragment.source_is_grapheme_aligned(source),
                                    "history source range splits grapheme/UTF-8"
                                );
                            }
                            true
                        } else {
                            false
                        }
                    } else {
                        false
                    }
                } else {
                    false
                }
            } else {
                false
            }
        };
        if converged {
            break;
        }
        ensure!(
            Instant::now() < until,
            "captured-history zoom fetch did not converge: initial={baseline:?} target={current:?}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    target(other_view, None, 1000).await?;
    Ok(())
}

async fn type_command(handle: &NativeAttachHandle, command: &str) -> Result<()> {
    for character in command.chars() {
        handle.try_send(native_key(
            LogicalKey::Character(character.to_string()),
            Modifiers::default(),
            Some(character.to_string()),
        ))?;
    }
    handle.try_send(native_key(
        LogicalKey::Named(NamedKey::Enter),
        Modifiers::default(),
        None,
    ))?;
    Ok(())
}

async fn set_scale(handle: &NativeAttachHandle, scale: u16) -> Result<()> {
    let until = Instant::now() + LIMIT;
    loop {
        if let Some(target) = handle.pane_zoom_target(None) {
            if target.scale_millis == scale {
                return Ok(());
            }
            let receipt = handle.try_set_pane_zoom(target, scale)?;
            while receipt.status() == NativePaneZoomRequestStatus::Pending {
                ensure!(Instant::now() < until, "scale request settlement timeout");
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }
        ensure!(Instant::now() < until, "scale convergence timeout: {scale}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

async fn positioned_transition_sizing(
    handle: &NativeAttachHandle,
    sink: &Mutex<Sink>,
) -> Result<()> {
    use bmux_tui::grid_fragment::GridContentLayout;
    let mut sizing = positioned_zoom_sizing::PositionedZoomSizing::default();
    target(handle, None, 1750).await?;
    set_scale(handle, 750).await?;
    for _ in 0..2 {
        type_command(handle, "alternate").await?;
        let until = Instant::now() + LIMIT;
        let mut sent = false;
        loop {
            let output = sink
                .lock()
                .unwrap()
                .committed
                .clone()
                .context("committed output")?;
            let positioned = output
                .prepared
                .grid_fragments()
                .iter()
                .find(|fragment| fragment.content_layout() == GridContentLayout::Positioned);
            sizing.observe(output.identity, &output.prepared);
            sizing.tick(
                Instant::now(),
                |point| handle.pane_zoom_target(Some(point)),
                |target| {
                    sent = true;
                    handle.try_set_pane_zoom(target, target.scale_millis)
                },
            );
            if let Some(fragment) = positioned {
                let viewport = fragment.viewport().context("allocated viewport")?;
                let expected = [
                    u32::from(viewport.width) * 1000 / 750,
                    u32::from(viewport.height) * 1000 / 750,
                ];
                if expected
                    == [
                        u32::from(fragment.buffer().area().width),
                        u32::from(fragment.buffer().area().height),
                    ]
                {
                    break;
                }
            }
            ensure!(
                Instant::now() < until,
                "zoom-before-launch positioned geometry did not converge"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        ensure!(sent, "transition did not exercise authorized resize");
        type_command(handle, "size").await?;
        let until = Instant::now() + LIMIT;
        loop {
            let output = sink
                .lock()
                .unwrap()
                .committed
                .clone()
                .context("committed output")?;
            sizing.observe(output.identity, &output.prepared);
            let mut repeated = false;
            sizing.tick(
                Instant::now(),
                |point| handle.pane_zoom_target(Some(point)),
                |_| {
                    repeated = true;
                    anyhow::bail!("settled sizing repeated")
                },
            );
            ensure!(!repeated, "settled sizing repeated");
            let confirmed = output.prepared.grid_fragments().iter().any(|fragment| {
                let buffer = fragment.buffer();
                (0..buffer.area().height).any(|row| {
                    buffer.row_symbols(row).is_some_and(|text| {
                        text.contains(&format!(
                            "PTY_SIZE {} {}",
                            buffer.area().height,
                            buffer.area().width
                        ))
                    })
                })
            });
            if confirmed {
                break;
            }
            ensure!(Instant::now() < until, "PTY size probe absent");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        set_scale(handle, 1000).await?;
        type_command(handle, "normal").await?;
        let until = Instant::now() + LIMIT;
        loop {
            let output = sink
                .lock()
                .unwrap()
                .committed
                .clone()
                .context("committed output")?;
            sizing.observe(output.identity, &output.prepared);
            if output
                .prepared
                .grid_fragments()
                .iter()
                .any(|fragment| fragment.content_layout() == GridContentLayout::Flow)
            {
                break;
            }
            ensure!(Instant::now() < until, "alternate exit timeout");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        set_scale(handle, 750).await?;
    }
    set_scale(handle, 1750).await?;
    Ok(())
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
    let readiness =
        bmux_pane_runtime_plugin_api::pane_runtime_readiness_v1::client::runtime_readiness(
            &mut control,
        )
        .await?;
    ensure!(
        readiness.helper_ready,
        "isolated helper unavailable: {:?}",
        readiness.helper_error
    );
    ensure!(
        readiness.runtime_dir == paths.runtime_dir.to_string_lossy(),
        "runtime mismatch"
    );
    let session = commands::new_session_with_runtime(&mut control, Some("zoom-acceptance".into()))
        .await?
        .map_err(|error| anyhow::anyhow!("create: {error:?}"))?
        .session_id;
    let first = state::list_panes(&mut control, Some(session))
        .await?
        .map_err(|error| anyhow::anyhow!("list: {error:?}"))?
        .panes[0]
        .id;
    let second = commands::split_pane(&mut control, session, Some(first), "horizontal".into(), 50)
        .await?
        .map_err(|error| anyhow::anyhow!("split: {error:?}"))?
        .pane_id;
    for pane in [first, second] {
        let source = bmux_pane_runtime_plugin_api::pane_runtime_spawn_v1::client::capture_source(
            &mut control,
            session,
            pane,
        )
        .await?
        .map_err(|error| anyhow::anyhow!("fixture source capture: {error:?}"))?;
        eprintln!(
            "verified fixture source pane={pane} execution={}",
            source.execution_token
        );
    }
    let client_a = connect(&paths).await?;
    let client_b = connect(&paths).await?;
    initialize_native_runtime_state(&config, &paths)?;
    let sink_a = Arc::new(Mutex::new(Sink::default()));
    let sink_b = Arc::new(Mutex::new(Sink::default()));
    let (a, host_a) = host(Arc::clone(&sink_a), &paths)?;
    let (b, host_b) = host(Arc::clone(&sink_b), &paths)?;
    let controller = async {
        let result: Result<()> = async {
            let focused = target(&a, None, 1000).await?;
            let other_view = target(&b, None, 1000).await?;
            ensure!(
                focused.pane_id == other_view.pane_id,
                "views did not attach same focused source"
            );
            ensure!(
                focused.identity.epoch != other_view.identity.epoch,
                "attach lifetimes alias"
            );
            // Query the retained compositor, never guess a split's cell rectangle.
            let mut other = None;
            for y in [2.5, 8.5, 16.5] {
                for x in [5.5, 30.5, 60.5, 90.5] {
                    if let Some(candidate) = a.pane_zoom_target(Some([x, y]))
                        && candidate.pane_id != focused.pane_id
                    {
                        other = Some(candidate);
                    }
                }
            }
            let other = other.context("second visible pane absent from retained hit query")?;
            ensure!([first, second].contains(&other.pane_id), "wrong source");
            // Both workloads have emitted their entire source before comparison.
            let until = Instant::now() + LIMIT;
            loop {
                let left = canonical(&mut control, session, first).await?;
                let right = canonical(&mut control, session, second).await?;
                if left.to_string().contains("ZOOM_SOURCE")
                    && right.to_string().contains("ZOOM_SOURCE")
                {
                    break;
                }
                ensure!(Instant::now() < until, "source workload readiness timeout");
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            let before_first = canonical(&mut control, session, first).await?;
            let before_second = canonical(&mut control, session, second).await?;
            let focused = target(&a, None, 1000).await?;
            a.try_send(NativeAttachEvent::SetPaneZoom {
                target: focused,
                scale_millis: 1500,
            })?;
            let zoomed = target(&a, None, 1500).await?;
            ensure!(zoomed.pane_id == focused.pane_id, "zoom changed focus");
            ensure!(
                target(&b, None, 1000).await?.pane_id == focused.pane_id,
                "second view lost source"
            );
            ensure!(
                canonical(&mut control, session, first).await? == before_first,
                "normal zoom changed first canonical grid/PTY"
            );
            ensure!(
                canonical(&mut control, session, second).await? == before_second,
                "normal zoom changed second canonical grid/PTY"
            );
            let old = sink_a.lock().unwrap().committed.as_ref().unwrap().identity;
            sink_a.lock().unwrap().reject = true;
            let receipt = a.try_set_pane_zoom(zoomed, 1750)?;
            let until = Instant::now() + LIMIT;
            loop {
                if sink_a.lock().unwrap().rejected > 0 {
                    break;
                }
                ensure!(Instant::now() < until, "failed output was not exercised");
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            ensure!(
                receipt.status() == NativePaneZoomRequestStatus::Applied,
                "reducer request not applied before output failure"
            );
            ensure!(
                sink_a.lock().unwrap().committed.as_ref().unwrap().identity == old,
                "failure promoted sink output"
            );
            if let Some(current) = a.pane_zoom_target(None) {
                ensure!(
                    current.identity == old && current.scale_millis == 1500,
                    "failure promoted interaction authority"
                );
            }
            sink_a.lock().unwrap().reject = false;
            a.try_send(NativeAttachEvent::RetryPresentation {
                viewport_generation: 1,
            })?;
            target(&a, None, 1750).await?;
            target(&b, None, 1000).await?;
            // A source token from another attach lifetime cannot mutate this view.
            let foreign = a.try_set_pane_zoom(target(&b, None, 1000).await?, 2000)?;
            let until = Instant::now() + LIMIT;
            while foreign.status() == NativePaneZoomRequestStatus::Pending {
                ensure!(Instant::now() < until, "foreign token did not settle");
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            ensure!(
                foreign.status() == NativePaneZoomRequestStatus::Rejected,
                "cross-view token was admitted"
            );
            target(&a, None, 1750).await?;
            ensure!(
                canonical(&mut control, session, first).await? == before_first,
                "retry changed first source"
            );
            ensure!(
                canonical(&mut control, session, second).await? == before_second,
                "retry changed second source"
            );
            positioned_transition_sizing(&a, &sink_a).await?;
            captured_history_keyboard_zoom(&a, &b, &sink_a).await?;
            Ok(())
        }
        .await;
        if result.is_err() {
            for (label, handle, sink) in [("a", &a, &sink_a), ("b", &b, &sink_b)] {
                let sink = sink.lock().unwrap();
                eprintln!(
                    "view {label}: attempts={} rejected={} focused={:?} committed={:?}",
                    sink.attempts,
                    sink.rejected,
                    handle.pane_zoom_target(None),
                    sink.committed.as_ref().map(|output| output.identity)
                );
                if let Some(output) = &sink.committed {
                    eprintln!(
                        "view {label} cursor={:?} fragments={}",
                        output.prepared.cursor(),
                        output.prepared.grid_fragments().len()
                    );
                    let buffer = output.prepared.buffer();
                    for row in 0..buffer.area().height.min(24) {
                        eprintln!("view {label} row {row}: {:?}", buffer.row_symbols(row));
                    }
                }
                for y in [2.5, 8.5, 16.5] {
                    for x in [5.5, 30.5, 60.5, 90.5] {
                        eprintln!(
                            "view {label} point=({x},{y}) target={:?}",
                            handle.pane_zoom_target(Some([x, y]))
                        );
                    }
                }
            }
        }
        let close_a = a.try_send(NativeAttachEvent::Close);
        let close_b = b.try_send(NativeAttachEvent::Close);
        result?;
        close_a?;
        close_b?;
        Ok::<(), anyhow::Error>(())
    };
    let session_target = session.to_string();
    let (outcome_a, outcome_b, checks) = tokio::time::timeout(Duration::from_secs(90), async {
        tokio::join!(
            async {
                let result =
                    run_native_with_client(client_a, &session_target, config.clone(), host_a).await;
                eprintln!("attach A settled: {result:?}");
                result
            },
            async {
                let result =
                    run_native_with_client(client_b, &session_target, config.clone(), host_b).await;
                eprintln!("attach B settled: {result:?}");
                result
            },
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
fn two_panes_two_views_normal_zoom_and_failed_output_preserve_source_authority() -> Result<()> {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("warn")
        .with_test_writer()
        .try_init();
    // Run in this integration-test process only. Native plugin configuration is
    // process-global/thread-local; neither attach future migrates across threads.
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?
        .block_on(exercise())
}
