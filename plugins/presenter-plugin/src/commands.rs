//! Native caller command orchestration; generated services own BMUX integration.
use anyhow::{Context, Result};
use bmux_plugin_sdk::NativeCommandContext;
use bmux_presenter_plugin_api::{presenter_host_v1 as manage, presenter_host_v2 as trusted_manage};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

pub(crate) fn paths(context: &NativeCommandContext) -> bmux_config::ConfigPaths {
    bmux_config::ConfigPaths::new(
        context.connection.config_dir.clone().into(),
        context.connection.runtime_dir.clone().into(),
        context.connection.data_dir.clone().into(),
        context.connection.state_dir.clone().into(),
    )
}
pub(crate) async fn run(
    command: crate::cli::Command,
    context: &NativeCommandContext,
) -> Result<()> {
    match command {
        crate::cli::Command::Check { source } => {
            let deck = crate::source::load(&source)?;
            println!("{}: {} slides; valid", source.display(), deck.slides.len());
            Ok(())
        }
        crate::cli::Command::Play {
            source,
            presenter,
            reload,
            allow_playbooks,
            web,
        } => {
            let loaded = crate::source::loaded(&source)?;
            let approval = allow_playbooks
                .then(|| crate::playbook::Approval::prepare(&source, &loaded.deck))
                .transpose()?;
            let mut rich = crate::rich::RichAssets::prepare(&loaded.assets)?;
            rich.code = crate::rich::highlight(&loaded.deck);
            let mut player =
                crate::player::Player::local(loaded.deck, reload.then_some(source), presenter);
            if let Some(approval) = approval.clone() {
                let attempts_root = paths(context).state_dir.join("presenter-playbooks");
                let environment = crate::playbook_live::DemoEnvironment {
                    paths: paths(context),
                    approval,
                    executable: std::env::current_exe()?,
                    attempts_root,
                    runtime: tokio::runtime::Handle::current(),
                };
                player.live_demos = Some(
                    crate::playbook_live::LiveDemos::prepare_local(
                        &player.deck,
                        &environment,
                        context
                            .plugin_search_roots
                            .iter()
                            .map(std::path::PathBuf::from)
                            .collect(),
                    )
                    .map_err(anyhow::Error::msg)?,
                );
            }
            player._playbook_approval = approval;
            player.rich = rich;
            match crate::web::Browser::start(web, &paths(context)).await? {
                Some(browser) if browser.only => crate::player::run_headless(player, browser).await,
                Some(browser) => {
                    let browser = Arc::new(Mutex::new(browser));
                    player.browser = Some(Arc::clone(&browser));
                    let result = crate::player::run(player).await;
                    let browser = Arc::try_unwrap(browser)
                        .map_err(|_| anyhow::anyhow!("browser lifecycle still shared"))?
                        .into_inner()
                        .map_err(|_| anyhow::anyhow!("browser poisoned"))?;
                    browser.shutdown().await;
                    result
                }
                None => crate::player::run(player).await,
            }
        }
        crate::cli::Command::Join(options) => crate::viewer::join(options, context).await,
        crate::cli::Command::Host(options) => {
            let browser = crate::web::Browser::start(options.web, &paths(context)).await?;
            let presenter = crate::source::credential(&options.presenter_credential)?;
            let audience = options
                .audience_credential
                .as_ref()
                .map(|path| crate::source::credential(path))
                .transpose()?;
            if options.managed {
                let mut client = bmux_client::BmuxClient::connect_with_paths(
                    &paths(context),
                    "presentation host",
                )
                .await?;
                let source = std::fs::canonicalize(&options.source)?
                    .to_string_lossy()
                    .into_owned();
                let info = if options.allow_playbooks {
                    let readiness = bmux_playbook_plugin_api::playbook_state::client::lookup_preparation(
                        &mut client, "presenter-readiness".to_owned(),
                    ).await.map_err(|error| anyhow::anyhow!(
                        "managed presentation playbook provider unavailable; no local fallback: {error}"
                    ))?;
                    if let Err(bmux_playbook_plugin_api::playbook_types::PlaybookError::Rejected { message }) = readiness {
                        anyhow::ensure!(message.starts_with("preparation identity not found"),
                            "managed playbook provider unavailable: {message}");
                    }
                    trusted_manage::client::start(
                        &mut client,
                        source,
                        options.public_read,
                        presenter.clone(),
                        audience.clone(),
                        options.reload,
                        true,
                    )
                    .await?
                } else {
                    manage::client::start(
                        &mut client,
                        source,
                        options.public_read,
                        presenter.clone(),
                        audience.clone(),
                        options.reload,
                    )
                    .await?
                }
                .map_err(anyhow::Error::msg)?;
                println!("bmux:{} (run {})", info.id, info.run_id);
                if let Some(browser) = browser {
                    let join = crate::cli::JoinOptions {
                        endpoint: format!("bmux:{}", info.id),
                        ca: None,
                        server_name: None,
                        // Host owner projection is authenticated using its existing credential;
                        // browser auth is independent and never sees this secret.
                        credential: Some(options.presenter_credential),
                        mode: if browser.only || browser.broadcast_only() {
                            bmux_presenter_runtime::InteractionMode::ReceiveOnly
                        } else {
                            bmux_presenter_runtime::InteractionMode::Shared
                        },
                        presenter: !(browser.only || browser.broadcast_only()),
                    };
                    let (connection, next) = crate::viewer::connect(&join, &paths(context)).await?;
                    if browser.only || browser.broadcast_only() {
                        let (_stop, receiver) = tokio::sync::watch::channel(false);
                        crate::web_host::run(connection, browser, receiver).await
                    } else {
                        crate::viewer::run_connected(
                            join,
                            paths(context),
                            connection,
                            next,
                            Some(browser),
                            None,
                        )
                        .await
                    }
                } else {
                    Ok(())
                }
            } else {
                let host = if options.allow_playbooks {
                    let loaded = crate::source::loaded(&options.source)?;
                    let approval =
                        crate::playbook::Approval::prepare(&options.source, &loaded.deck)?;
                    let attempts_root = paths(context).state_dir.join("presenter-playbooks");
                    std::fs::create_dir_all(&attempts_root)?;
                    let environment = crate::playbook_live::DemoEnvironment {
                        paths: paths(context),
                        approval,
                        executable: std::env::current_exe()?,
                        attempts_root,
                        runtime: tokio::runtime::Handle::current(),
                    };
                    crate::services::build_trusted(
                        &options.source,
                        options.public_read,
                        presenter.clone(),
                        audience,
                        environment,
                    )?
                } else {
                    crate::services::build(
                        &options.source,
                        options.public_read,
                        presenter.clone(),
                        audience,
                    )?
                };
                let host = Arc::new(Mutex::new(host));
                let update_host = Arc::downgrade(&host);
                tokio::spawn(async move {
                    let mut tick = tokio::time::interval(Duration::from_millis(50));
                    loop {
                        tick.tick().await;
                        let Some(host) = update_host.upgrade() else {
                            break;
                        };
                        if let Ok(mut host) = host.lock() {
                            let _ = host.advance_owner();
                        }
                    }
                });
                let tls = crate::tls::server(
                    options
                        .cert
                        .as_deref()
                        .context("TLS certificate required")?,
                    options.key.as_deref().context("TLS key required")?,
                )?;
                let listener = tokio::net::TcpListener::bind(&options.listen).await?;
                println!("tls://{}", listener.local_addr()?);
                let (shutdown, receiver) = tokio::sync::watch::channel(false);
                let transport_stop = shutdown.clone();
                let browser_task = if let Some(browser) = browser {
                    let mode = if browser.only || browser.broadcast_only() {
                        bmux_presenter_runtime::InteractionMode::ReceiveOnly
                    } else {
                        bmux_presenter_runtime::InteractionMode::Shared
                    };
                    let (participant, next) = {
                        let mut host = host.lock().map_err(|_| anyhow::anyhow!("host poisoned"))?;
                        let participant = host.authenticate(
                            bmux_presenter_runtime::protocol::PROTOCOL_VERSION,
                            Some(&presenter),
                            mode,
                        )?;
                        let next = host.next_action_id(&participant)?;
                        (participant, next)
                    };
                    let connection = crate::viewer::Connection::Local {
                        host: Arc::clone(&host),
                        participant,
                    };
                    let stop = receiver.clone();
                    let shutdown = shutdown.clone();
                    let join = crate::cli::JoinOptions {
                        endpoint: "local-owner".into(),
                        ca: None,
                        server_name: None,
                        credential: None,
                        mode,
                        presenter: true,
                    };
                    let paths = paths(context);
                    Some(async move {
                        let result = if browser.only || browser.broadcast_only() {
                            crate::web_host::run(connection, browser, stop).await
                        } else {
                            crate::viewer::run_connected(
                                join,
                                paths,
                                connection,
                                next,
                                Some(browser),
                                Some(stop),
                            )
                            .await
                        };
                        let _ = shutdown.send(true);
                        result
                    })
                } else {
                    None
                };
                let reload_host = Arc::clone(&host);
                let reload_source = options.source;
                let reload = options.reload;
                let monitor = tokio::spawn(async move {
                    let mut modified = std::fs::metadata(&reload_source)
                        .and_then(|m| m.modified())
                        .ok();
                    loop {
                        tokio::select! {
                            _ = tokio::signal::ctrl_c() => { let _ = shutdown.send(true); break; }
                            () = tokio::time::sleep(Duration::from_millis(300)), if reload => {
                                let next = std::fs::metadata(&reload_source).and_then(|m| m.modified()).ok();
                                if next != modified {
                                    match crate::source::loaded(&reload_source).and_then(|loaded| reload_host.lock().map_err(|_| anyhow::anyhow!("host poisoned")).and_then(|mut host| crate::services::reload(&mut host, loaded))) {
                                        Ok(()) => modified = next,
                                        Err(error) => eprintln!("reload rejected; last valid deck retained: {error}"),
                                    }
                                }
                            }
                        }
                    }
                });
                let transport = async {
                    let result = bmux_presenter_runtime::transport::serve_tls(
                        listener,
                        host,
                        tls,
                        bmux_presenter_runtime::transport::NetworkConfig::default(),
                        receiver,
                    )
                    .await;
                    let _ = transport_stop.send(true);
                    result.map_err(anyhow::Error::from)
                };
                let result = if let Some(browser) = browser_task {
                    let (transport, browser) = tokio::join!(transport, browser);
                    transport.and(browser)
                } else {
                    transport.await
                };
                monitor.abort();
                let _ = monitor.await;
                result
            }
        }
    }
}
