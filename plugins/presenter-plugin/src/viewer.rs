//! Remote caller viewer. Generated BMUX services and standalone TLS share only
//! runtime models; neither creates a second domain IPC envelope.
use anyhow::{Context, Result, bail};
use bmux_plugin_sdk::NativeCommandContext;
use bmux_presenter_plugin_api::presenter_view_v2 as view;
use bmux_presenter_runtime::{
    Action, Cursor, InteractionMode, Poll, Request, Response, Snapshot, Submission,
};
use std::time::Duration;

pub(crate) struct RemoteUpdate {
    pub snapshot: Snapshot,
    pub rich: crate::rich::RichAssets,
    pub acknowledgment: Option<String>,
}

pub(crate) enum Connection {
    Local {
        host: std::sync::Arc<std::sync::Mutex<bmux_presenter_runtime::Host>>,
        participant: String,
    },
    Tls(bmux_presenter_runtime::transport::Client),
    Bmux {
        client: bmux_client::BmuxClient,
        host: String,
        participant: String,
    },
}
impl Connection {
    pub(crate) async fn snapshot(&mut self) -> Result<Snapshot> {
        match self {
            Self::Local { host, participant } => Ok(host
                .lock()
                .map_err(|_| anyhow::anyhow!("host poisoned"))?
                .snapshot(participant)?),
            Self::Tls(client) => match client.request(Request::Snapshot).await? {
                Response::Poll(Poll::Snapshot { snapshot, .. }) => Ok(*snapshot),
                Response::Error(e) => Err(e.into()),
                _ => bail!("invalid bootstrap response"),
            },
            Self::Bmux {
                client,
                host,
                participant,
            } => {
                let s = view::client::snapshot(client, host.clone(), participant.clone())
                    .await?
                    .map_err(|e| anyhow::anyhow!("{e:?}"))?;
                decode(s)
            }
        }
    }
    pub(crate) async fn poll(&mut self, cursor: Cursor) -> Result<Option<Snapshot>> {
        match self {
            Self::Local { host, participant } => match host
                .lock()
                .map_err(|_| anyhow::anyhow!("host poisoned"))?
                .poll(participant, Some(&cursor))?
            {
                Poll::Snapshot { snapshot, .. } => Ok(Some(*snapshot)),
                Poll::Unchanged(_) => Ok(None),
            },
            Self::Tls(client) => match client
                .request(Request::Poll {
                    cursor: Some(cursor),
                })
                .await?
            {
                Response::Poll(Poll::Unchanged(_)) => Ok(None),
                Response::Poll(Poll::Snapshot { snapshot, .. }) => Ok(Some(*snapshot)),
                Response::Error(e) => Err(e.into()),
                _ => bail!("invalid poll response"),
            },
            Self::Bmux {
                client,
                host,
                participant,
            } => match view::client::poll(
                client,
                host.clone(),
                participant.clone(),
                Some(wire(cursor)),
            )
            .await?
            .map_err(|e| anyhow::anyhow!("{e:?}"))?
            {
                view::Poll::Unchanged { .. } => Ok(None),
                view::Poll::Snapshot { snapshot, .. } => decode(snapshot).map(Some),
            },
        }
    }
    async fn navigate(
        &mut self,
        cursor: Cursor,
        action_id: u64,
        action: Action,
    ) -> Result<(Cursor, String)> {
        match self {
            Self::Local { host, participant } => {
                let ack = host
                    .lock()
                    .map_err(|_| anyhow::anyhow!("host poisoned"))?
                    .submit(
                        participant,
                        Submission {
                            expected: cursor,
                            action_id,
                            action,
                        },
                    )?;
                Ok((
                    ack.cursor,
                    format!("ack {}: {}", ack.action_id, ack.outcome),
                ))
            }
            Self::Tls(client) => match client
                .request(Request::Submit(Submission {
                    expected: cursor,
                    action_id,
                    action,
                }))
                .await?
            {
                Response::Acknowledged(ack) => Ok((
                    ack.cursor,
                    format!("ack {}: {}", ack.action_id, ack.outcome),
                )),
                Response::Error(e) => Err(e.into()),
                _ => bail!("invalid acknowledgment"),
            },
            Self::Bmux {
                client,
                host,
                participant,
            } => {
                let action = match action {
                    Action::DemoInput(input) => view::Action::DemoInput {
                        surface_id: input.surface_id,
                        surface_revision: input.surface_revision,
                        x: input.x,
                        y: input.y,
                        input: input.input,
                    },
                    Action::Effect { name, input } => view::Action::Effect { name, input },
                    Action::Navigate(navigation) => match navigation {
                        bmux_presenter::NavigationAction::Next => view::Action::Next,
                        bmux_presenter::NavigationAction::Previous => view::Action::Previous,
                        bmux_presenter::NavigationAction::First => view::Action::First,
                        bmux_presenter::NavigationAction::Last => view::Action::Last,
                        bmux_presenter::NavigationAction::GoTo(id) => view::Action::GoTo {
                            slide_id: id.as_str().into(),
                        },
                        bmux_presenter::NavigationAction::Reconcile => {
                            bail!("reconcile is owner-only")
                        }
                    },
                };
                let ack = view::client::submit(
                    client,
                    host.clone(),
                    participant.clone(),
                    action_id,
                    wire(cursor),
                    action,
                )
                .await?
                .map_err(|e| anyhow::anyhow!("{e:?}"))?;
                Ok((
                    runtime(ack.cursor),
                    format!("ack {}: {}", ack.action_id, ack.outcome),
                ))
            }
        }
    }
    pub(crate) async fn prepare(&mut self, snapshot: Snapshot) -> Result<RemoteUpdate> {
        anyhow::ensure!(
            snapshot
                .surfaces
                .iter()
                .all(|surface| surface.validate(16_384, 512 * 1024).is_ok()),
            "invalid audience mirror scene"
        );
        anyhow::ensure!(
            snapshot.assets.len() <= 128,
            "remote asset count exceeds budget"
        );
        let mut assets = Vec::new();
        let mut total = 0_usize;
        for info in &snapshot.assets {
            anyhow::ensure!(
                info.bytes <= 8 * 1024 * 1024
                    && total.saturating_add(info.bytes) <= 32 * 1024 * 1024,
                "remote asset budget exceeded"
            );
            total += info.bytes;
            let mut bytes = Vec::with_capacity(info.bytes);
            while bytes.len() < info.bytes {
                let offset = bytes.len();
                let chunk = match self {
                    Self::Local { host, participant } => match host
                        .lock()
                        .map_err(|_| anyhow::anyhow!("host poisoned"))?
                        .asset(participant, &info.id, offset, 64 * 1024)?
                    {
                        Response::Asset { bytes, .. } => bytes,
                        _ => bail!("invalid local asset response"),
                    },
                    Self::Tls(client) => match client
                        .request(Request::Asset {
                            id: info.id.clone(),
                            offset,
                            length: 64 * 1024,
                        })
                        .await?
                    {
                        Response::Asset {
                            id,
                            offset: got,
                            total,
                            bytes,
                        } if id == info.id && got == offset && total == info.bytes => bytes,
                        Response::Error(error) => return Err(error.into()),
                        _ => bail!("invalid asset chunk"),
                    },
                    Self::Bmux {
                        client,
                        host,
                        participant,
                    } => {
                        let chunk = view::client::asset(
                            client,
                            host.clone(),
                            participant.clone(),
                            info.id.clone(),
                            offset as u64,
                            64 * 1024,
                        )
                        .await?
                        .map_err(|e| anyhow::anyhow!("{e:?}"))?;
                        anyhow::ensure!(
                            chunk.id == info.id
                                && chunk.offset == offset as u64
                                && chunk.total == info.bytes as u64,
                            "asset chunk identity mismatch"
                        );
                        chunk.data
                    }
                };
                anyhow::ensure!(
                    !chunk.is_empty()
                        && chunk.len() <= 64 * 1024
                        && bytes.len() + chunk.len() <= info.bytes,
                    "invalid asset chunk size"
                );
                bytes.extend(chunk);
            }
            assets.push(bmux_presenter_runtime::Asset {
                id: info.id.clone(),
                media_type: info.media_type.clone(),
                bytes,
            });
        }
        let deck = snapshot.deck.clone();
        let rich = tokio::task::spawn_blocking(move || {
            let mut rich = crate::rich::RichAssets::prepare(&assets)?;
            rich.code = crate::rich::highlight(&deck);
            Ok::<_, anyhow::Error>(rich)
        })
        .await??;
        Ok(RemoteUpdate {
            snapshot,
            rich,
            acknowledgment: None,
        })
    }
    pub(crate) async fn leave(&mut self) {
        if let Self::Local { host, participant } = self
            && let Ok(mut host) = host.lock()
        {
            host.disconnect(participant);
        }
        if let Self::Bmux {
            client,
            host,
            participant,
        } = self
        {
            let _ = view::client::leave(client, host.clone(), participant.clone()).await;
        }
    }
}
fn runtime(c: view::Cursor) -> Cursor {
    Cursor {
        run_id: c.run_id,
        deck_revision: c.deck_revision,
        revision: c.revision,
    }
}
fn wire(c: Cursor) -> view::Cursor {
    view::Cursor {
        run_id: c.run_id,
        deck_revision: c.deck_revision,
        revision: c.revision,
    }
}
fn decode(s: view::Snapshot) -> Result<Snapshot> {
    Ok(Snapshot {
        cursor: runtime(s.cursor),
        deck: serde_json::from_slice(&s.public_deck_json)?,
        state: bmux_presenter::PresentationState {
            slide_id: s.slide_id.map(Into::into),
            reveal_step: s.reveal_step,
        },
        notes: s.notes,
        surfaces: serde_json::from_slice(&s.surfaces_json)?,
        assets: serde_json::from_slice(&s.assets_json)?,
    })
}

pub(crate) async fn connect(
    options: &crate::cli::JoinOptions,
    paths: &bmux_config::ConfigPaths,
) -> Result<(Connection, Option<u64>)> {
    let credential = options
        .credential
        .as_ref()
        .map(|path| crate::source::credential(path))
        .transpose()?;
    let (connection, next_action_id) = if let Some(host) = options.endpoint.strip_prefix("bmux:") {
        let mut client =
            bmux_client::BmuxClient::connect_with_paths(paths, "presentation viewer").await?;
        let mode = match options.mode {
            InteractionMode::ReceiveOnly => view::InteractionMode::ReceiveOnly,
            InteractionMode::SubmitOnly => view::InteractionMode::SubmitOnly,
            InteractionMode::Shared => view::InteractionMode::Shared,
            InteractionMode::Independent => view::InteractionMode::Independent,
        };
        let auth = view::client::authenticate(&mut client, host.to_owned(), credential, mode)
            .await?
            .map_err(|e| anyhow::anyhow!("{e:?}"))?;
        (
            Connection::Bmux {
                client,
                host: host.to_owned(),
                participant: auth.id,
            },
            auth.next_action_id,
        )
    } else {
        let address = options
            .endpoint
            .strip_prefix("tls://")
            .context("TLS endpoint required")?;
        let server_name = options.server_name.clone().unwrap_or_else(|| {
            address
                .rsplit_once(':')
                .map_or(address, |(host, _)| host)
                .trim_matches(['[', ']'])
                .to_owned()
        });
        let socket = tokio::net::lookup_host(address)
            .await?
            .next()
            .context("endpoint resolves to no addresses")?;
        let tls = crate::tls::client(options.ca.as_deref().context("CA required")?)?;
        let mut client = bmux_presenter_runtime::transport::Client::connect_tls(
            socket,
            server_name,
            tls,
            4 * 1024 * 1024,
            Duration::from_secs(5),
        )
        .await?;
        let next_action_id = match client.authenticate(credential, options.mode).await? {
            Response::Authenticated { next_action_id, .. } => next_action_id,
            Response::Error(e) => return Err(e.into()),
            _ => bail!("invalid authentication response"),
        };
        (Connection::Tls(client), next_action_id)
    };
    Ok((connection, next_action_id))
}

pub(crate) async fn join(
    options: crate::cli::JoinOptions,
    context: &NativeCommandContext,
) -> Result<()> {
    let paths = crate::commands::paths(context);
    let (connection, next_action_id) = connect(&options, &paths).await?;
    run_connected(options, paths, connection, next_action_id, None, None).await
}

pub(crate) async fn run_connected(
    options: crate::cli::JoinOptions,
    paths: bmux_config::ConfigPaths,
    mut connection: Connection,
    next_action_id: Option<u64>,
    browser: Option<crate::web::Browser>,
    external_stop: Option<tokio::sync::watch::Receiver<bool>>,
) -> Result<()> {
    let initial = connection.snapshot().await?;
    let prepared = connection.prepare(initial).await?;
    let initial = prepared.snapshot;
    let (actions, mut action_rx) = tokio::sync::mpsc::channel(16);
    let (updates, update_rx) = tokio::sync::mpsc::channel(2);
    let presenter = options.presenter;
    let submit_only = options.mode == InteractionMode::SubmitOnly;
    let receive_only = options.mode == InteractionMode::ReceiveOnly;
    let mut player = crate::player::Player::local(initial.deck.clone(), None, presenter);
    player.state = initial.state.clone();
    player.notes = if presenter {
        initial.notes.clone()
    } else {
        None
    };
    player.rich = prepared.rich;
    player.surfaces = initial.surfaces.clone();
    player.remote = Some(crate::player::RemoteConnection {
        actions,
        updates: update_rx,
        receive_only,
    });
    let (stop, mut stopping) = tokio::sync::oneshot::channel();
    let network = tokio::spawn(async move {
        let mut cursor = initial.cursor;
        let mut action_id = next_action_id.unwrap_or(1).saturating_sub(1);
        let mut acknowledgment = None;
        loop {
            let result: Result<Option<Snapshot>> = tokio::select! {
                _ = &mut stopping => break,
                action = action_rx.recv() => {
                    let Some(action) = action else { break; };
                    action_id = action_id.saturating_add(1);
                    match connection.navigate(cursor.clone(), action_id, action).await {
                        Ok((next, outcome)) => { cursor = next; acknowledgment = Some(outcome); connection.snapshot().await.map(Some) }
                        Err(error) => Err(error),
                    }
                }
                () = tokio::time::sleep(Duration::from_millis(150)), if !submit_only => connection.poll(cursor.clone()).await,
            };
            match result {
                Ok(Some(snapshot)) => {
                    cursor = snapshot.cursor.clone();
                    match connection.prepare(snapshot).await {
                        Ok(mut prepared) => {
                            prepared.acknowledgment = acknowledgment.take();
                            if updates.send(Ok(prepared)).await.is_err() {
                                break;
                            }
                        }
                        Err(error) => {
                            let _ = updates.send(Err(error.to_string())).await;
                            break;
                        }
                    }
                }
                Ok(None) => (),
                Err(error) => {
                    // Reauthenticate only this live host. Never resend the failed action.
                    // Even if its outcome was uncertain, next_action_id reconciles the ledger.
                    let run_id = cursor.run_id.clone();
                    if matches!(&connection, Connection::Local { .. }) {
                        let _ = updates
                            .send(Err(format!("local host observation failed: {error}")))
                            .await;
                        break;
                    }
                    let mut recovered = None;
                    for _ in 0..3 {
                        tokio::time::sleep(Duration::from_millis(300)).await;
                        if let Ok((mut candidate, next)) = connect(&options, &paths).await {
                            if let Ok(snapshot) = candidate.snapshot().await {
                                if snapshot.cursor.run_id != run_id {
                                    candidate.leave().await;
                                    break;
                                }
                                if let Ok(prepared) = candidate.prepare(snapshot).await {
                                    recovered = Some((candidate, next, prepared));
                                    break;
                                }
                            }
                            candidate.leave().await;
                        }
                    }
                    if let Some((candidate, next, prepared)) = recovered {
                        connection.leave().await;
                        connection = candidate;
                        action_id = next.unwrap_or(1).saturating_sub(1);
                        cursor = prepared.snapshot.cursor.clone();
                        if updates.send(Ok(prepared)).await.is_err() {
                            break;
                        }
                    } else {
                        let _ = updates
                            .send(Err(format!(
                                "live-host reconnect failed; action not replayed: {error}"
                            )))
                            .await;
                        break;
                    }
                }
            }
        }
        connection.leave().await;
    });
    let browser = browser.map(|browser| std::sync::Arc::new(std::sync::Mutex::new(browser)));
    player.browser = browser.clone();
    player.stop = external_stop;
    let result = crate::player::run(player).await;
    let _ = stop.send(());
    let _ = network.await;
    if let Some(browser) = browser {
        let browser = std::sync::Arc::try_unwrap(browser)
            .map_err(|_| anyhow::anyhow!("browser lifecycle still shared"))?
            .into_inner()
            .map_err(|_| anyhow::anyhow!("browser poisoned"))?;
        browser.shutdown().await;
    }
    result
}
