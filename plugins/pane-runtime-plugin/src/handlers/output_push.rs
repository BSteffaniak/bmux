//! Pane output pushed to attach clients (see `docs/development/pane-output-performance.md`).
//!
//! A client that subscribes stops reading its pane output with `attach-pane-output-batch`. When a
//! subscribed pane has new output, its reader wakes the client's subscription through the wire
//! sink; once the server can send that client a message it pulls from this source, which reads
//! everything the client has not seen for its panes, as one batch. A client that falls behind is
//! not queued messages: its output collects in the pane's buffer and is read later, together, or
//! reported as a gap past the buffer's retention (the client then resynchronizes from a snapshot).

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

use bmux_pane_runtime_plugin_api::attach_runtime_state::AttachPaneOutputBatch;
use bmux_plugin_sdk::WireEventSink;
use bmux_session_models::{ClientId, SessionId};
use uuid::Uuid;

use bmux_pane_runtime_plugin_api::OUTPUT_PUSH_KIND;

struct Subscription {
    session_id: Uuid,
    pane_ids: Vec<Uuid>,
    max_bytes: usize,
}

fn subscriptions() -> std::sync::MutexGuard<'static, BTreeMap<Uuid, Subscription>> {
    static SUBSCRIPTIONS: OnceLock<Mutex<BTreeMap<Uuid, Subscription>>> = OnceLock::new();
    SUBSCRIPTIONS
        .get_or_init(Mutex::default)
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
}

fn sink() -> Option<Arc<dyn WireEventSink>> {
    bmux_plugin::global_plugin_state_registry()
        .get::<bmux_plugin_sdk::WireEventSinkHandle>()
        .and_then(|sink| sink.read().ok().map(|sink| sink.0.clone()))
}

/// Register the pull source with the host once. False when the host cannot push.
fn source_registered() -> bool {
    static REGISTERED: OnceLock<bool> = OnceLock::new();
    *REGISTERED.get_or_init(|| {
        sink().is_some_and(|sink| {
            sink.register_pull_source(OUTPUT_PUSH_KIND.to_string(), Arc::new(pull))
                .is_ok()
        })
    })
}

/// Push `client`'s output for `pane_ids` in `session_id` (replacing any earlier set; empty
/// stops). Returns whether output is now pushed.
pub fn subscribe(client: Uuid, session_id: Uuid, pane_ids: Vec<Uuid>, max_bytes: usize) -> bool {
    if pane_ids.is_empty() {
        subscriptions().remove(&client);
        return false;
    }
    if !source_registered() {
        return false;
    }
    subscriptions().insert(
        client,
        Subscription {
            session_id,
            pane_ids,
            max_bytes,
        },
    );
    // Output that arrived before the subscription is sent now.
    wake(client)
}

fn wake(client: Uuid) -> bool {
    let woken = sink().is_some_and(|sink| sink.wake(client).is_ok());
    if !woken {
        // Disconnected (or no host): stop pushing to it.
        subscriptions().remove(&client);
    }
    woken
}

/// A pane has new output: wake the clients that have it pushed.
pub fn pane_output_available(session_id: Uuid, pane_id: Uuid) {
    let clients: Vec<Uuid> = {
        let subscriptions = subscriptions();
        if subscriptions.is_empty() {
            return;
        }
        subscriptions
            .iter()
            .filter(|(_, subscription)| {
                subscription.session_id == session_id && subscription.pane_ids.contains(&pane_id)
            })
            .map(|(client, _)| *client)
            .collect()
    };
    for client in clients {
        wake(client);
    }
}

/// The host can send to `client`: everything it has not seen for its panes, encoded.
fn pull(client: Uuid) -> Option<Vec<u8>> {
    let (session_id, pane_ids, max_bytes) = {
        let subscriptions = subscriptions();
        let subscription = subscriptions.get(&client)?;
        (
            subscription.session_id,
            subscription.pane_ids.clone(),
            subscription.max_bytes,
        )
    };
    let handle = super::session_runtime_handle()?;
    let (chunks, still_pending) = handle.0.attach_pane_output_batch_with_dirty_check(
        SessionId(session_id),
        ClientId(client),
        &pane_ids,
        max_bytes,
    );
    let Ok(chunks) = chunks else {
        // Detached from the session: stop pushing.
        subscriptions().remove(&client);
        return None;
    };
    if still_pending {
        // More than one message's worth: read the rest once this one is queued.
        wake(client);
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
