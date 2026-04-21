//! bmux control-catalog plugin — cross-cutting catalog aggregator.
//!
//! Subscribes to [`SessionEvent`], [`ContextEvent`], and [`ClientEvent`]
//! on the plugin event bus, maintains a monotonic revision counter,
//! and exposes a typed `control-catalog-state::snapshot` query that
//! returns an aggregate view over sessions, contexts, and
//! context-to-session bindings.
//!
//! Attach UIs use this plugin as a single-shot catalog snapshot source
//! (formerly `Request::ControlCatalogSnapshot` in `bmux_ipc`).

#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

use bmux_clients_plugin_api::clients_events::{self, ClientEvent};
use bmux_contexts_plugin_api::ContextState;
use bmux_contexts_plugin_api::contexts_events::{self, ContextEvent};
use bmux_control_catalog_plugin_api::control_catalog_events::{self, CatalogEvent, CatalogScope};
use bmux_control_catalog_plugin_api::control_catalog_state::{
    self, ContextRow, ContextSessionBinding, SessionRow, Snapshot,
};
use bmux_plugin::{
    ServiceCaller, TypedServiceCaller, global_event_bus, global_plugin_state_registry,
};
use bmux_plugin_sdk::prelude::*;
use bmux_plugin_sdk::{HostScope, TypedServiceRegistrationContext, TypedServiceRegistry};
use bmux_sessions_plugin_api::SessionManager;
use bmux_sessions_plugin_api::sessions_events::{self, SessionEvent};
use serde::{Deserialize, Serialize};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// Process-wide revision counter for the control catalog.
///
/// Plugin lifecycle in a single process creates the plugin once at
/// activation; the revision counter lives in a `static` so it survives
/// any test-time reactivation while still starting from 1 on each
/// process start (matches the pre-migration server-side counter).
static CATALOG_REVISION: AtomicU64 = AtomicU64::new(1);

fn current_revision() -> u64 {
    CATALOG_REVISION.load(Ordering::SeqCst)
}

fn bump_revision_and_emit(scopes: Vec<CatalogScope>, full_resync: bool) {
    let new_rev = CATALOG_REVISION
        .fetch_add(1, Ordering::SeqCst)
        .saturating_add(1);
    let _ = global_event_bus().emit(
        &control_catalog_events::EVENT_KIND,
        CatalogEvent::Changed {
            revision: new_rev,
            scopes,
            full_resync,
        },
    );
}

/// Argument record for the `snapshot` query.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct SnapshotArgs {
    #[serde(default)]
    since_revision: Option<u64>,
}

#[derive(Default)]
pub struct ControlCatalogPlugin;

impl RustPlugin for ControlCatalogPlugin {
    fn activate(
        &mut self,
        _context: NativeLifecycleContext,
    ) -> std::result::Result<i32, PluginCommandError> {
        global_event_bus().register_channel::<CatalogEvent>(control_catalog_events::EVENT_KIND);

        // Spawn background subscribers that tick the revision when any
        // foundational-plugin event arrives. Each subscriber runs in a
        // dedicated OS thread (plugins aren't guaranteed a tokio
        // runtime; a bare thread + blocking recv is the simplest
        // approach).
        spawn_session_events_subscriber();
        spawn_context_events_subscriber();
        spawn_client_events_subscriber();

        Ok(bmux_plugin_sdk::EXIT_OK)
    }

    fn run_command(
        &mut self,
        _context: NativeCommandContext,
    ) -> std::result::Result<i32, PluginCommandError> {
        Err(PluginCommandError::unknown_command(""))
    }

    fn invoke_service(&mut self, context: NativeServiceContext) -> ServiceResponse {
        bmux_plugin_sdk::route_service!(context, {
            "control-catalog-state", "snapshot" => |req: SnapshotArgs, _ctx| {
                build_snapshot(req.since_revision)
                    .map_err(|e| ServiceResponse::error("snapshot_failed", e))
            },
        })
    }

    fn register_typed_services(
        &self,
        context: TypedServiceRegistrationContext<'_>,
        registry: &mut TypedServiceRegistry,
    ) {
        let caller = Arc::new(TypedServiceCaller::from_registration_context(&context));

        let Ok(read_cap) =
            HostScope::new(bmux_control_catalog_plugin_api::capabilities::CATALOG_READ.as_str())
        else {
            return;
        };

        let state: Arc<dyn control_catalog_state::ControlCatalogStateService + Send + Sync> =
            Arc::new(ControlCatalogStateHandle::new(caller));
        registry
            .insert_typed::<dyn control_catalog_state::ControlCatalogStateService + Send + Sync>(
                read_cap,
                ServiceKind::Query,
                control_catalog_state::INTERFACE_ID,
                state,
            );
    }
}

// ── Event subscribers ────────────────────────────────────────────────

fn spawn_session_events_subscriber() {
    let Ok(mut rx) = global_event_bus().subscribe::<SessionEvent>(&sessions_events::EVENT_KIND)
    else {
        return;
    };
    std::thread::spawn(move || {
        while let Ok(_event) = rx.blocking_recv() {
            bump_revision_and_emit(vec![CatalogScope::Sessions, CatalogScope::Bindings], false);
        }
    });
}

fn spawn_context_events_subscriber() {
    let Ok(mut rx) = global_event_bus().subscribe::<ContextEvent>(&contexts_events::EVENT_KIND)
    else {
        return;
    };
    std::thread::spawn(move || {
        while let Ok(_event) = rx.blocking_recv() {
            bump_revision_and_emit(vec![CatalogScope::Contexts, CatalogScope::Bindings], false);
        }
    });
}

fn spawn_client_events_subscriber() {
    let Ok(mut rx) = global_event_bus().subscribe::<ClientEvent>(&clients_events::EVENT_KIND)
    else {
        return;
    };
    std::thread::spawn(move || {
        while let Ok(_event) = rx.blocking_recv() {
            // Client events don't directly change sessions/contexts,
            // but client_count on session-rows can shift. Tick sessions
            // scope so UIs re-pull.
            bump_revision_and_emit(vec![CatalogScope::Sessions], false);
        }
    });
}

// ── Snapshot builder ─────────────────────────────────────────────────

fn build_snapshot(_since_revision: Option<u64>) -> Result<Snapshot, String> {
    let revision = current_revision();
    let sessions = read_sessions()?;
    let contexts = read_contexts()?;
    let context_session_bindings = read_bindings()?;
    Ok(Snapshot {
        revision,
        sessions,
        contexts,
        context_session_bindings,
    })
}

fn read_sessions() -> Result<Vec<SessionRow>, String> {
    let Some(state) = global_plugin_state_registry().get::<SessionManager>() else {
        return Ok(Vec::new());
    };
    let manager = state
        .read()
        .map_err(|_| "session manager lock poisoned".to_string())?;
    Ok(manager
        .list_sessions()
        .into_iter()
        .map(|info| SessionRow {
            id: info.id.0,
            name: info.name,
            client_count: u32::try_from(info.client_count).unwrap_or(u32::MAX),
        })
        .collect())
}

fn read_contexts() -> Result<Vec<ContextRow>, String> {
    let Some(state) = global_plugin_state_registry().get::<ContextState>() else {
        return Ok(Vec::new());
    };
    let guard = state
        .read()
        .map_err(|_| "context state lock poisoned".to_string())?;
    Ok(guard
        .list()
        .into_iter()
        .map(|ctx| ContextRow {
            id: ctx.id,
            name: ctx.name,
        })
        .collect())
}

fn read_bindings() -> Result<Vec<ContextSessionBinding>, String> {
    let Some(state) = global_plugin_state_registry().get::<ContextState>() else {
        return Ok(Vec::new());
    };
    let guard = state
        .read()
        .map_err(|_| "context state lock poisoned".to_string())?;
    Ok(guard
        .session_by_context
        .iter()
        .map(|(context_id, session_id)| ContextSessionBinding {
            context_id: *context_id,
            session_id: session_id.0,
        })
        .collect())
}

// ── Typed state handle ───────────────────────────────────────────────

pub struct ControlCatalogStateHandle {
    caller: Arc<TypedServiceCaller>,
}

impl ControlCatalogStateHandle {
    const fn new(caller: Arc<TypedServiceCaller>) -> Self {
        Self { caller }
    }
}

impl control_catalog_state::ControlCatalogStateService for ControlCatalogStateHandle {
    fn snapshot<'a>(
        &'a self,
        since_revision: Option<u64>,
    ) -> Pin<Box<dyn Future<Output = Snapshot> + Send + 'a>> {
        Box::pin(async move {
            self.caller
                .call_service::<SnapshotArgs, Snapshot>(
                    bmux_control_catalog_plugin_api::capabilities::CATALOG_READ.as_str(),
                    ServiceKind::Query,
                    control_catalog_state::INTERFACE_ID.as_str(),
                    "snapshot",
                    &SnapshotArgs { since_revision },
                )
                .unwrap_or_else(|_| Snapshot {
                    revision: 0,
                    sessions: Vec::new(),
                    contexts: Vec::new(),
                    context_session_bindings: Vec::new(),
                })
        })
    }
}

bmux_plugin_sdk::export_plugin!(ControlCatalogPlugin, include_str!("../plugin.toml"));
