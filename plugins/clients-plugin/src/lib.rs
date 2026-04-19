//! bmux clients plugin — typed owner of per-client identity and view
//! state.
//!
//! # Current phase: thin typed facade
//!
//! The core host runtime only exposes `current_client` today; it has
//! no `list_clients`, `set_current_session`, or `set_following`
//! operations. This plugin wires the typed `ClientsStateService` and
//! `ClientsCommandsService` contracts so consumers can start depending
//! on the types and service handles, but the unimplemented operations
//! return structured errors until Stage 6 migrates legacy CLI paths
//! and Stage 8 re-homes the underlying state into this plugin.

#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

use bmux_clients_plugin_api::clients_commands::{
    self, ClientAck, ClientsCommandsService, SetCurrentSessionError, SetFollowingError,
};
use bmux_clients_plugin_api::clients_state::{
    self, ClientQueryError, ClientSummary, ClientsStateService,
};
use bmux_plugin::{HostRuntimeApi, TypedServiceCaller};
use bmux_plugin_sdk::prelude::*;
use bmux_plugin_sdk::{HostScope, TypedServiceRegistrationContext, TypedServiceRegistry};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use uuid::Uuid;

/// Top-level plugin type. Holds no state in this phase; state re-homes
/// here in Stage 8.
#[derive(Default)]
pub struct ClientsPlugin;

impl RustPlugin for ClientsPlugin {
    fn run_command(
        &mut self,
        _context: NativeCommandContext,
    ) -> std::result::Result<i32, PluginCommandError> {
        Err(PluginCommandError::unknown_command(""))
    }

    fn invoke_service(&mut self, context: NativeServiceContext) -> ServiceResponse {
        ServiceResponse::error(
            "unsupported_service_operation",
            format!(
                "clients plugin has no byte-dispatch service handler for '{}:{}'",
                context.request.service.interface_id.as_str(),
                context.request.operation.as_str(),
            ),
        )
    }

    fn register_typed_services(
        &self,
        context: TypedServiceRegistrationContext<'_>,
        registry: &mut TypedServiceRegistry,
    ) {
        let caller = Arc::new(TypedServiceCaller::from_registration_context(&context));

        let (Ok(read_cap), Ok(write_cap)) = (
            HostScope::new(bmux_clients_plugin_api::capabilities::CLIENTS_READ.as_str()),
            HostScope::new(bmux_clients_plugin_api::capabilities::CLIENTS_WRITE.as_str()),
        ) else {
            return;
        };

        let state: Arc<dyn ClientsStateService + Send + Sync> =
            Arc::new(ClientsStateHandle::new(Arc::clone(&caller)));
        registry.insert_typed::<dyn ClientsStateService + Send + Sync>(
            read_cap,
            ServiceKind::Query,
            clients_state::INTERFACE_ID,
            state,
        );

        let commands: Arc<dyn ClientsCommandsService + Send + Sync> =
            Arc::new(ClientsCommandsHandle::new(caller));
        registry.insert_typed::<dyn ClientsCommandsService + Send + Sync>(
            write_cap,
            ServiceKind::Command,
            clients_commands::INTERFACE_ID,
            commands,
        );
    }
}

// ── Typed state handle ───────────────────────────────────────────────

pub struct ClientsStateHandle {
    caller: Arc<TypedServiceCaller>,
}

impl ClientsStateHandle {
    const fn new(caller: Arc<TypedServiceCaller>) -> Self {
        Self { caller }
    }
}

impl ClientsStateService for ClientsStateHandle {
    fn list_clients<'a>(&'a self) -> Pin<Box<dyn Future<Output = Vec<ClientSummary>> + Send + 'a>> {
        // Core has no `list_clients` operation; this returns an empty
        // list until Stage 8 re-homes the state into this plugin. The
        // `Deferred within M4` section of `.m4-scratch.md` tracks the
        // completion requirement.
        Box::pin(async move { Vec::new() })
    }

    fn current_client<'a>(
        &'a self,
    ) -> Pin<
        Box<dyn Future<Output = std::result::Result<ClientSummary, ClientQueryError>> + Send + 'a>,
    > {
        Box::pin(async move {
            let response = self
                .caller
                .current_client()
                .map_err(|_| ClientQueryError::NoCurrentClient)?;
            Ok(ClientSummary {
                id: response.id,
                selected_session_id: response.selected_session_id,
                // Core's `CurrentClientResponse` does not currently
                // carry a selected context id; the clients-plugin will
                // surface that once Stage 8 re-homes state into this
                // plugin and the notion of "current context per
                // client" moves here.
                selected_context_id: None,
                following_client_id: response.following_client_id,
                following_global: response.following_global,
            })
        })
    }
}

// ── Typed commands handle ────────────────────────────────────────────

pub struct ClientsCommandsHandle {
    // The caller is kept even though command operations are
    // unimplemented in this phase so the handle type retains the same
    // shape as its peers and is ready for Stage 6 wiring.
    _caller: Arc<TypedServiceCaller>,
}

impl ClientsCommandsHandle {
    const fn new(caller: Arc<TypedServiceCaller>) -> Self {
        Self { _caller: caller }
    }
}

impl ClientsCommandsService for ClientsCommandsHandle {
    fn set_current_session<'a>(
        &'a self,
        _session_id: Uuid,
    ) -> Pin<
        Box<
            dyn Future<Output = std::result::Result<ClientAck, SetCurrentSessionError>> + Send + 'a,
        >,
    > {
        Box::pin(async move {
            Err(SetCurrentSessionError::Denied {
                reason:
                    "set-current-session is not yet wired into the core runtime; tracked in M4 \
                     Stage 6/8"
                        .to_string(),
            })
        })
    }

    fn set_following<'a>(
        &'a self,
        _target_client_id: Option<Uuid>,
        _global: bool,
    ) -> Pin<Box<dyn Future<Output = std::result::Result<ClientAck, SetFollowingError>> + Send + 'a>>
    {
        Box::pin(async move {
            Err(SetFollowingError::Denied {
                reason: "set-following is not yet wired into the core runtime; tracked in M4 \
                         Stage 6/8"
                    .to_string(),
            })
        })
    }
}

bmux_plugin_sdk::export_plugin!(ClientsPlugin, include_str!("../plugin.toml"));
