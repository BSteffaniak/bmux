//! Pane output pushed to attach clients (see `docs/development/pane-output-performance.md`).
//!
//! A client that subscribes stops reading its pane output with `attach-pane-output-batch`. When a
//! subscribed pane has new output, its reader wakes the client's subscription through the wire
//! sink; once the server can send that client a message it pulls from this source, which reads
//! everything the client has not seen for its panes, as one batch. A client that falls behind is
//! not queued messages: its output collects in the pane's buffer and is read later, together, or
//! reported as a gap past the buffer's retention (the client then resynchronizes from a snapshot).
//!
//! Frames are pushed the same way (`subscribe_frames`): a subscribed client is sent, when it can
//! take a message, frames bringing its replicas of its panes level with their grids. Frames are
//! built when sent, from the grids as they then are, so a client that falls behind is sent one
//! frame a pane, never a backlog.
//!
//! The same mechanism tells a client that its input producer barriers changed, so it reads them
//! when they do instead of polling.

use std::collections::BTreeMap;
use std::sync::OnceLock;
use switchy_thread::sync::MutexGuard;
use switchy_thread::sync::{Arc, Mutex, PoisonError};

use bmux_pane_runtime_plugin_api::attach_runtime_state::AttachPaneOutputBatch;
use bmux_plugin_sdk::WireEventSink;
use bmux_session_models::{ClientId, SessionId};
use uuid::Uuid;

use bmux_pane_runtime_plugin_api::{
    FRAME_PUSH_KIND, OUTPUT_PUSH_KIND, PRODUCER_BARRIERS_CHANGED_KIND,
};

struct Subscription {
    session_id: Uuid,
    pane_ids: Vec<Uuid>,
    max_bytes: usize,
}

/// One pane-runtime activation's push subscriptions, pending barrier
/// notices and pull-source registration. Owned by that activation's host
/// handles; nothing here is process-wide.
/// A client's pushed frames: its panes (session, pane), the history a reset frame carries at
/// most, and whether it observes them (need not be attached to their sessions).
struct FrameSubscription {
    targets: Vec<(Uuid, Uuid)>,
    max_history_lines: usize,
    observer: bool,
}

#[derive(Default)]
pub(crate) struct OutputPushState {
    subscriptions: Mutex<BTreeMap<Uuid, Subscription>>,
    frame_subscriptions: Mutex<BTreeMap<Uuid, FrameSubscription>>,
    barriers_changed: Mutex<std::collections::BTreeSet<Uuid>>,
    registered: OnceLock<bool>,
}

fn subscriptions(
    host: &crate::runtime::PaneRuntimeHost,
) -> MutexGuard<'_, BTreeMap<Uuid, Subscription>> {
    host.output_push()
        .subscriptions
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
}

fn frame_subscriptions(
    host: &crate::runtime::PaneRuntimeHost,
) -> MutexGuard<'_, BTreeMap<Uuid, FrameSubscription>> {
    host.output_push()
        .frame_subscriptions
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
}

fn sink(host: &crate::runtime::PaneRuntimeHost) -> Option<Arc<dyn WireEventSink>> {
    host.states()
        .get::<bmux_plugin_sdk::WireEventSinkHandle>()
        .and_then(|sink| sink.read().ok().map(|sink| sink.0.clone()))
}

fn barriers_changed(
    host: &crate::runtime::PaneRuntimeHost,
) -> MutexGuard<'_, std::collections::BTreeSet<Uuid>> {
    host.output_push()
        .barriers_changed
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
}

/// Register the pull sources with the host once. False when the host cannot push.
fn source_registered(host: &crate::runtime::PaneRuntimeHost) -> bool {
    *host.output_push().registered.get_or_init(|| {
        sink(host).is_some_and(|sink| {
            let output_host = host.clone();
            let frame_host = host.clone();
            let barrier_host = host.clone();
            sink.register_pull_source(
                OUTPUT_PUSH_KIND.to_string(),
                Arc::new(move |client| pull(&output_host, client)),
            )
            .is_ok()
                && sink
                    .register_pull_source(
                        FRAME_PUSH_KIND.to_string(),
                        Arc::new(move |client| pull_frames(&frame_host, client)),
                    )
                    .is_ok()
                && sink
                    .register_pull_source(
                        PRODUCER_BARRIERS_CHANGED_KIND.to_string(),
                        Arc::new(move |client| pull_barriers_changed(&barrier_host, client)),
                    )
                    .is_ok()
        })
    })
}

/// Input producer barriers of these clients began or settled: tell them.
pub fn producer_barriers_changed(
    host: &crate::runtime::PaneRuntimeHost,
    clients: impl IntoIterator<Item = Uuid>,
) {
    if !source_registered(host) {
        return;
    }
    let Some(sink) = sink(host) else {
        return;
    };
    for client in clients {
        barriers_changed(host).insert(client);
        if sink.wake(client).is_err() {
            barriers_changed(host).remove(&client);
        }
    }
}

fn pull_barriers_changed(host: &crate::runtime::PaneRuntimeHost, client: Uuid) -> Option<Vec<u8>> {
    barriers_changed(host).remove(&client).then(Vec::new)
}

/// Push `client`'s output for `pane_ids` in `session_id` (replacing any earlier set; empty
/// stops). Returns whether output is now pushed.
pub fn subscribe(
    host: &crate::runtime::PaneRuntimeHost,
    client: Uuid,
    session_id: Uuid,
    pane_ids: Vec<Uuid>,
    max_bytes: usize,
) -> bool {
    if pane_ids.is_empty() {
        subscriptions(host).remove(&client);
        return false;
    }
    if !source_registered(host) {
        return false;
    }
    subscriptions(host).insert(
        client,
        Subscription {
            session_id,
            pane_ids,
            max_bytes,
        },
    );
    // Output that arrived before the subscription is sent now.
    wake(host, client)
}

/// Push frames of `pane_ids` in `session_id` to `client` (replacing any earlier set; empty
/// stops). Returns whether frames are now pushed.
pub fn subscribe_frames(
    host: &crate::runtime::PaneRuntimeHost,
    client: Uuid,
    session_id: Uuid,
    pane_ids: Vec<Uuid>,
    max_history_lines: usize,
) -> bool {
    if pane_ids.is_empty() {
        frame_subscriptions(host).remove(&client);
        return false;
    }
    if !source_registered(host) {
        return false;
    }
    frame_subscriptions(host).insert(
        client,
        FrameSubscription {
            targets: pane_ids
                .into_iter()
                .map(|pane_id| (session_id, pane_id))
                .collect(),
            max_history_lines,
            observer: false,
        },
    );
    // The panes as they are now are sent at once.
    wake(host, client)
}

/// Push `client` frames of `targets` (session, pane) as an observer, authorized by the caller;
/// empty `targets` stops it.
pub fn observe_frames(
    host: &crate::runtime::PaneRuntimeHost,
    client: Uuid,
    targets: Vec<(Uuid, Uuid)>,
    max_history_lines: usize,
) -> bool {
    if targets.is_empty() {
        frame_subscriptions(host).remove(&client);
        return false;
    }
    if !source_registered(host) {
        return false;
    }
    frame_subscriptions(host).insert(
        client,
        FrameSubscription {
            targets,
            max_history_lines,
            observer: true,
        },
    );
    wake(host, client)
}

fn wake(host: &crate::runtime::PaneRuntimeHost, client: Uuid) -> bool {
    let woken = sink(host).is_some_and(|sink| sink.wake(client).is_ok());
    if !woken {
        // Disconnected (or no host): stop pushing to it.
        subscriptions(host).remove(&client);
        frame_subscriptions(host).remove(&client);
    }
    woken
}

/// A pane has new output: wake the clients that have it pushed.
pub fn pane_output_available(
    host: &crate::runtime::PaneRuntimeHost,
    session_id: Uuid,
    pane_id: Uuid,
) {
    let mut clients: Vec<Uuid> = subscriptions(host)
        .iter()
        .filter(|(_, subscription)| {
            subscription.session_id == session_id && subscription.pane_ids.contains(&pane_id)
        })
        .map(|(client, _)| *client)
        .collect();
    clients.extend(
        frame_subscriptions(host)
            .iter()
            .filter(|(_, subscription)| subscription.targets.contains(&(session_id, pane_id)))
            .map(|(client, _)| *client),
    );
    clients.sort_unstable();
    clients.dedup();
    for client in clients {
        wake(host, client);
    }
}

/// The host can send to `client`: everything it has not seen for its panes, encoded.
fn pull(host: &crate::runtime::PaneRuntimeHost, client: Uuid) -> Option<Vec<u8>> {
    let (session_id, pane_ids, max_bytes) = {
        let subscriptions = subscriptions(host);
        let subscription = subscriptions.get(&client)?;
        (
            subscription.session_id,
            subscription.pane_ids.clone(),
            subscription.max_bytes,
        )
    };
    let handle = host.registered_session_runtime()?;
    let (chunks, still_pending) = handle.0.attach_pane_output_batch_with_dirty_check(
        SessionId(session_id),
        ClientId(client),
        &pane_ids,
        max_bytes,
    );
    let Ok(chunks) = chunks else {
        // Detached from the session: stop pushing.
        subscriptions(host).remove(&client);
        return None;
    };
    if still_pending {
        // More than one message's worth: read the rest once this one is queued.
        wake(host, client);
    }
    if chunks.is_empty() {
        return None;
    }
    let batch = AttachPaneOutputBatch {
        chunks: chunks
            .into_iter()
            .map(super::attach_state::chunk_to_record)
            .collect(),
        output_still_pending: still_pending,
    };
    bmux_ipc::encode(&batch).ok()
}

/// The host can send to `client`: frames for its panes that changed, encoded.
fn pull_frames(host: &crate::runtime::PaneRuntimeHost, client: Uuid) -> Option<Vec<u8>> {
    let (targets, max_history_lines, observer) = {
        let subscriptions = frame_subscriptions(host);
        let subscription = subscriptions.get(&client)?;
        (
            subscription.targets.clone(),
            subscription.max_history_lines,
            subscription.observer,
        )
    };
    let handle = host.registered_session_runtime()?;
    let built = if observer {
        let targets: Vec<(SessionId, Uuid)> = targets
            .iter()
            .map(|(session, pane)| (SessionId(*session), *pane))
            .collect();
        handle
            .0
            .observe_pane_frames(ClientId(client), &targets, max_history_lines)
    } else {
        // An attached client's panes are all of one session.
        let session_id = targets
            .first()
            .map_or_else(Uuid::nil, |(session, _)| *session);
        let pane_ids: Vec<Uuid> = targets.iter().map(|(_, pane)| *pane).collect();
        handle.0.attach_pane_frames(
            SessionId(session_id),
            ClientId(client),
            &pane_ids,
            max_history_lines,
        )
    };
    let Ok((frames, still_pending)) = built else {
        // Detached from the session: stop pushing.
        frame_subscriptions(host).remove(&client);
        return None;
    };
    if still_pending {
        // More than one message's worth: build the rest once this one is queued.
        wake(host, client);
    }
    if frames.is_empty() {
        return None;
    }
    bmux_ipc::encode(&super::attach_state::frames_record(frames, still_pending)).ok()
}
