//! Generated service endpoints call Host directly. Participant IDs are bound to
//! the authenticated BMUX connection, never accepted as bearer credentials.
use bmux_plugin_sdk::{NativeServiceContext, ServiceResponse};
use bmux_presentation::NavigationAction;
use bmux_presentation_plugin_api::{presentation_host_v1 as manage, presentation_view_v1 as view};
use bmux_presentation_runtime::{
    Action, Credential, Cursor, Host, HostConfig, HostError, InteractionMode, PROTOCOL_VERSION,
    Poll, Response, Scopes, Submission,
};
use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{Mutex, OnceLock},
    time::{Duration, Instant},
};

struct Managed {
    host: Host,
    source: PathBuf,
    run_id: String,
    participants: BTreeMap<String, Option<uuid::Uuid>>,
}
static HOSTS: OnceLock<Mutex<BTreeMap<String, Managed>>> = OnceLock::new();
fn hosts() -> &'static Mutex<BTreeMap<String, Managed>> {
    HOSTS.get_or_init(Mutex::default)
}
fn transport(error: impl ToString) -> ServiceResponse {
    ServiceResponse::error("presentation_failed", error.to_string())
}

pub(crate) fn build(
    source: &std::path::Path,
    public: bool,
    presenter: String,
    audience: Option<String>,
) -> anyhow::Result<Host> {
    let loaded = crate::source::loaded(source)?;
    let mut owner_rich = crate::rich::RichAssets::prepare(&loaded.assets)?;
    owner_rich.code = crate::rich::highlight(&loaded.deck);
    let owner = crate::owner::DemoOwner::new(loaded.deck.clone(), owner_rich);
    let mut host = Host::new(
        loaded.deck,
        loaded.assets,
        HostConfig {
            public_audience: public,
            ..HostConfig::default()
        },
    )?;
    host.set_owner(Box::new(owner))?;
    let expires_at = Instant::now() + Duration::from_secs(24 * 60 * 60);
    host.add_credential(Credential {
        secret: presenter,
        scopes: Scopes {
            notes: true,
            navigate: true,
            demo_input: true,
            effects: true,
        },
        expires_at,
    })?;
    if let Some(secret) = audience {
        host.add_credential(Credential {
            secret,
            scopes: Scopes {
                notes: false,
                navigate: true,
                demo_input: true,
                effects: false,
            },
            expires_at,
        })?;
    }
    Ok(host)
}

pub(crate) fn reload(
    host: &mut Host,
    loaded: bmux_presentation_runtime::loader::LoadedDeck,
) -> anyhow::Result<()> {
    let mut rich = crate::rich::RichAssets::prepare(&loaded.assets)?;
    rich.code = crate::rich::highlight(&loaded.deck);
    let owner = crate::owner::DemoOwner::new(loaded.deck.clone(), rich);
    host.reload_with_owner(loaded.deck, loaded.assets, Box::new(owner))?;
    Ok(())
}

fn watch_source(id: String, source: PathBuf) {
    std::thread::spawn(move || {
        let fingerprint = |path: &std::path::Path| {
            std::fs::metadata(path)
                .ok()
                .map(|metadata| (metadata.modified().ok(), metadata.len()))
        };
        let mut modified = fingerprint(&source);
        loop {
            std::thread::sleep(Duration::from_millis(300));
            if hosts().lock().map_or(true, |all| !all.contains_key(&id)) {
                break;
            }
            let next = fingerprint(&source);
            if next == modified {
                continue;
            }
            // Prepare outside the owner lock. Invalid reload retains the old deck.
            let prepared = crate::source::loaded(&source);
            modified = next;
            if let Ok(loaded) = prepared {
                let Ok(mut all) = hosts().lock() else {
                    break;
                };
                let Some(host) = all.get_mut(&id) else {
                    break;
                };
                let _ = reload(&mut host.host, loaded);
            }
        }
    });
}

fn access<T>(
    ctx: &NativeServiceContext,
    host_id: &str,
    participant: &str,
    operation: impl FnOnce(&mut Host) -> Result<T, HostError>,
) -> Result<T, view::Error> {
    if ctx.caller_client_id.is_none() {
        return Err(view::Error::Authentication);
    }
    let mut hosts = hosts().lock().map_err(|_| view::Error::Invalid)?;
    let managed = hosts.get_mut(host_id).ok_or(view::Error::NotFound)?;
    if managed.participants.get(participant) != Some(&ctx.caller_client_id) {
        return Err(view::Error::Authentication);
    }
    operation(&mut managed.host).map_err(error)
}

pub(crate) fn invoke(context: NativeServiceContext) -> ServiceResponse {
    bmux_plugin_sdk::route_service!(context, {
        "presentation-host-v1", "start" => |r: manage::client::StartRequest, _ctx| {
            let result = (|| -> anyhow::Result<manage::HostInfo> {
                let source = std::fs::canonicalize(&r.source)?;
                let secret = r.presenter_credential;
                let mut host = build(&source, r.public_read, secret.clone(), r.audience_credential)?;
                let p = host.authenticate(PROTOCOL_VERSION, Some(&secret), InteractionMode::ReceiveOnly)?;
                let run_id = host.cursor(&p)?.run_id;
                host.disconnect(&p);
                let id = uuid::Uuid::new_v4().to_string();
                let mut all = hosts().lock().map_err(|_| anyhow::anyhow!("host registry poisoned"))?;
                anyhow::ensure!(all.len() < 16, "managed host limit exceeded");
                let info = manage::HostInfo { id: id.clone(), run_id: run_id.clone(), source: source.to_string_lossy().into_owned() };
                all.insert(id.clone(), Managed { host, source: source.clone(), run_id, participants: BTreeMap::new() });
                if r.live_reload { watch_source(id, source); }
                Ok(info)
            })().map_err(|e| e.to_string());
            Ok::<_, ServiceResponse>(result)
        },
        "presentation-host-v1", "list" => |(): (), _ctx| {
            let all = hosts().lock().map_err(transport)?;
            Ok::<_, ServiceResponse>(all.iter().map(|(id, h)| manage::HostInfo { id: id.clone(), run_id: h.run_id.clone(), source: h.source.to_string_lossy().into_owned() }).collect::<Vec<_>>())
        },
        "presentation-host-v1", "reload" => |r: manage::client::ReloadRequest, _ctx| {
            let result = (|| -> anyhow::Result<()> {
                let mut all = hosts().lock().map_err(|_| anyhow::anyhow!("host registry poisoned"))?;
                let h = all.get_mut(&r.host_id).ok_or_else(|| anyhow::anyhow!("host not found"))?;
                let loaded = crate::source::loaded(&h.source)?;
                reload(&mut h.host, loaded)?;
                Ok(())
            })().map_err(|e| e.to_string());
            Ok::<_, ServiceResponse>(result)
        },
        "presentation-host-v1", "stop" => |r: manage::client::StopRequest, _ctx| {
            let result = hosts().lock().map_err(|e| e.to_string()).and_then(|mut all| all.remove(&r.host_id).map(|_| ()).ok_or_else(|| "host not found".into()));
            Ok::<_, ServiceResponse>(result)
        },
        "presentation-view-v1", "authenticate" => |r: view::client::AuthenticateRequest, ctx| {
            let result = (|| -> Result<view::Participant, view::Error> {
                if ctx.caller_client_id.is_none() { return Err(view::Error::Authentication); }
                let mut all = hosts().lock().map_err(|_| view::Error::Invalid)?;
                let h = all.get_mut(&r.host_id).ok_or(view::Error::NotFound)?;
                let mode = mode(r.mode);
                let id = h.host.authenticate(PROTOCOL_VERSION, r.credential.as_deref(), mode).map_err(error)?;
                let cursor = h.host.cursor(&id).map_err(error)?;
                let next_action_id = h.host.next_action_id(&id).map_err(error)?;
                h.participants.insert(id.clone(), ctx.caller_client_id);
                Ok(view::Participant { id, cursor: wire_cursor(cursor), mode: r.mode, next_action_id })
            })();
            Ok::<_, ServiceResponse>(result)
        },
        "presentation-view-v1", "poll" => |r: view::client::PollRequest, ctx| {
            let previous = r.cursor.map(runtime_cursor);
            let result = access(ctx, &r.host_id, &r.participant_id, |host| host.poll(&r.participant_id, previous.as_ref())).and_then(project_poll);
            Ok::<_, ServiceResponse>(result)
        },
        "presentation-view-v1", "snapshot" => |r: view::client::SnapshotRequest, ctx| {
            let result = access(ctx, &r.host_id, &r.participant_id, |host| host.snapshot(&r.participant_id))
                .and_then(|snapshot| match project_poll(Poll::Snapshot { resync: true, snapshot: Box::new(snapshot) })? {
                    view::Poll::Snapshot { snapshot, .. } => Ok(snapshot), _ => Err(view::Error::Invalid),
                });
            Ok::<_, ServiceResponse>(result)
        },
        "presentation-view-v1", "submit" => |r: view::client::SubmitRequest, ctx| {
            let submission = Submission { expected: runtime_cursor(r.expected), action: action(r.action), action_id: r.action_id };
            let result = access(ctx, &r.host_id, &r.participant_id, |host| host.submit(&r.participant_id, submission))
                .map(|ack| view::Acknowledgment { action_id: ack.action_id, cursor: wire_cursor(ack.cursor), outcome: ack.outcome });
            Ok::<_, ServiceResponse>(result)
        },
        "presentation-view-v1", "asset" => |r: view::client::AssetRequest, ctx| {
            let result = access(ctx, &r.host_id, &r.participant_id, |host| {
                host.asset(&r.participant_id, &r.id, usize::try_from(r.offset).map_err(|_| HostError::Limit)?, usize::try_from(r.length).map_err(|_| HostError::Limit)?)
            }).and_then(|response| match response {
                Response::Asset { id, offset, total, bytes } => Ok(view::AssetChunk { id, offset: offset as u64, total: total as u64, data: bytes }),
                _ => Err(view::Error::Invalid),
            });
            Ok::<_, ServiceResponse>(result)
        },
        "presentation-view-v1", "leave" => |r: view::client::LeaveRequest, ctx| {
            let _ = access(ctx, &r.host_id, &r.participant_id, |host| { host.disconnect(&r.participant_id); Ok(()) });
            if let Ok(mut all) = hosts().lock() && let Some(h) = all.get_mut(&r.host_id) && h.participants.get(&r.participant_id) == Some(&ctx.caller_client_id) { h.participants.remove(&r.participant_id); }
            Ok::<_, ServiceResponse>(())
        }
    })
}

pub(crate) fn mode(value: view::InteractionMode) -> InteractionMode {
    match value {
        view::InteractionMode::ReceiveOnly => InteractionMode::ReceiveOnly,
        view::InteractionMode::SubmitOnly => InteractionMode::SubmitOnly,
        view::InteractionMode::Shared => InteractionMode::Shared,
        view::InteractionMode::Independent => InteractionMode::Independent,
    }
}
fn wire_cursor(v: Cursor) -> view::Cursor {
    view::Cursor {
        run_id: v.run_id,
        deck_revision: v.deck_revision,
        revision: v.revision,
    }
}
fn runtime_cursor(v: view::Cursor) -> Cursor {
    Cursor {
        run_id: v.run_id,
        deck_revision: v.deck_revision,
        revision: v.revision,
    }
}
fn error(e: HostError) -> view::Error {
    match e {
        HostError::Authentication => view::Error::Authentication,
        HostError::Unauthorized => view::Error::Unauthorized,
        HostError::Version => view::Error::ProtocolVersion,
        HostError::Limit => view::Error::Limit,
        HostError::Stale => view::Error::Stale,
        HostError::NotFound => view::Error::NotFound,
        HostError::Invalid => view::Error::Invalid,
        HostError::ActionConflict => view::Error::ActionConflict,
        HostError::OutcomeEvicted => view::Error::OutcomeEvicted,
        HostError::ActionSequence => view::Error::ActionSequence,
        HostError::EffectUncertain => view::Error::EffectUncertain,
    }
}
fn project_poll(poll: Poll) -> Result<view::Poll, view::Error> {
    Ok(match poll {
        Poll::Unchanged(cursor) => view::Poll::Unchanged {
            cursor: wire_cursor(cursor),
        },
        Poll::Snapshot {
            resync,
            snapshot: s,
        } => view::Poll::Snapshot {
            resync,
            snapshot: view::Snapshot {
                cursor: wire_cursor(s.cursor),
                public_deck_json: serde_json::to_vec(&s.deck.public())
                    .map_err(|_| view::Error::Invalid)?,
                slide_id: s.state.slide_id.map(|id| id.as_str().to_owned()),
                reveal_step: s.state.reveal_step,
                notes: s.notes,
                surfaces_json: serde_json::to_vec(&s.surfaces).map_err(|_| view::Error::Invalid)?,
                assets_json: serde_json::to_vec(&s.assets).map_err(|_| view::Error::Invalid)?,
            },
        },
    })
}
fn action(value: view::Action) -> Action {
    match value {
        view::Action::Next => Action::Navigate(NavigationAction::Next),
        view::Action::Previous => Action::Navigate(NavigationAction::Previous),
        view::Action::First => Action::Navigate(NavigationAction::First),
        view::Action::Last => Action::Navigate(NavigationAction::Last),
        view::Action::GoTo { slide_id } => {
            Action::Navigate(NavigationAction::GoTo(slide_id.into()))
        }
        view::Action::DemoInput {
            surface_id,
            surface_revision,
            x,
            y,
            input,
        } => Action::DemoInput(bmux_presentation_runtime::SurfaceInput {
            surface_id,
            surface_revision,
            x,
            y,
            input,
        }),
        view::Action::Effect { name, input } => Action::Effect { name, input },
    }
}

#[cfg(test)]
include!("service_tests.rs");
