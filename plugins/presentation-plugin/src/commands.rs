//! Native caller command orchestration; generated services own BMUX integration.
use anyhow::{Context, Result};
use bmux_plugin_sdk::NativeCommandContext;
use bmux_presentation_plugin_api::presentation_host_v1 as manage;
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
        } => {
            let loaded = crate::source::loaded(&source)?;
            let mut rich = crate::rich::RichAssets::prepare(&loaded.assets)?;
            rich.code = crate::rich::highlight(&loaded.deck);
            let mut player =
                crate::player::Player::local(loaded.deck, reload.then_some(source), presenter);
            player.rich = rich;
            crate::player::run(player).await
        }
        crate::cli::Command::Join(options) => crate::viewer::join(options, context).await,
        crate::cli::Command::Host(options) => {
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
                let info = manage::client::start(
                    &mut client,
                    source,
                    options.public_read,
                    presenter,
                    audience,
                    options.reload,
                )
                .await?
                .map_err(anyhow::Error::msg)?;
                println!("bmux:{} (run {})", info.id, info.run_id);
                Ok(())
            } else {
                let host = crate::services::build(
                    &options.source,
                    options.public_read,
                    presenter,
                    audience,
                )?;
                let host = Arc::new(Mutex::new(host));
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
                let result = bmux_presentation_runtime::transport::serve_tls(
                    listener,
                    host,
                    tls,
                    bmux_presentation_runtime::transport::NetworkConfig::default(),
                    receiver,
                )
                .await;
                monitor.abort();
                let _ = monitor.await;
                result.map_err(anyhow::Error::from)
            }
        }
    }
}
