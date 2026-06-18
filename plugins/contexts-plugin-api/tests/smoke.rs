//! Smoke test: the BPDL-generated contexts-plugin-api bindings compile
//! and their types can be constructed, serialized, and the declared
//! constants match the schema.

use bmux_contexts_plugin_api::{
    capabilities::{CONTEXTS_READ, CONTEXTS_WRITE},
    contexts_commands::{self, CloseContextError, ContextAck, CreateContextError},
    contexts_events::{self, ContextEvent},
    contexts_state::{self, ContextSelector, ContextSummary},
};
use bmux_ipc::InvokeServiceKind;
use bmux_plugin_sdk::{TypedDispatchClient, TypedDispatchClientResult};

struct FakeClient {
    response: Vec<u8>,
    last_capability: Option<String>,
    last_kind: Option<InvokeServiceKind>,
    last_interface: Option<String>,
    last_operation: Option<String>,
    last_payload: Option<Vec<u8>>,
}

impl FakeClient {
    fn new<Resp: serde::Serialize>(response: &Resp) -> Self {
        Self {
            response: bmux_ipc::encode(response).expect("response should encode"),
            last_capability: None,
            last_kind: None,
            last_interface: None,
            last_operation: None,
            last_payload: None,
        }
    }
}

impl TypedDispatchClient for FakeClient {
    async fn invoke_service_raw(
        &mut self,
        capability: &str,
        kind: InvokeServiceKind,
        interface_id: &str,
        operation: &str,
        payload: Vec<u8>,
    ) -> TypedDispatchClientResult<Vec<u8>> {
        self.last_capability = Some(capability.to_string());
        self.last_kind = Some(kind);
        self.last_interface = Some(interface_id.to_string());
        self.last_operation = Some(operation.to_string());
        self.last_payload = Some(payload);
        Ok(self.response.clone())
    }
}

fn block_on<F: std::future::Future>(future: F) -> F::Output {
    use std::pin::pin;
    use std::task::{Context, Poll};

    let waker = std::task::Waker::noop();
    let mut context = Context::from_waker(waker);
    let mut future = pin!(future);
    loop {
        match future.as_mut().poll(&mut context) {
            Poll::Ready(output) => return output,
            Poll::Pending => std::thread::yield_now(),
        }
    }
}

#[test]
fn context_summary_round_trips() {
    let mut attrs = std::collections::BTreeMap::new();
    attrs.insert("project".to_string(), "bmux".to_string());
    let c = ContextSummary {
        id: uuid::Uuid::nil(),
        name: Some("work".to_string()),
        attributes: attrs,
    };
    let json = serde_json::to_string(&c).expect("serialize");
    let round: ContextSummary = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(c, round);
}

#[test]
fn context_selector_allows_id_or_name() {
    let by_id = ContextSelector {
        id: Some(uuid::Uuid::nil()),
        name: None,
    };
    let by_name = ContextSelector {
        id: None,
        name: Some("work".to_string()),
    };
    let json_id = serde_json::to_string(&by_id).expect("serialize id");
    let json_name = serde_json::to_string(&by_name).expect("serialize name");
    assert!(json_id.contains("id"));
    assert!(json_name.contains("work"));
}

#[test]
fn context_ack_and_error_variants_serialize() {
    let ack = ContextAck {
        id: uuid::Uuid::nil(),
        session_id: None,
    };
    assert!(serde_json::to_string(&ack).expect("ack").contains("id"));

    // `CreateContextError` intentionally has no `NameAlreadyExists`
    // variant: context names are display hints, not identity. Two
    // contexts may share a name deliberately.
    let err = CreateContextError::InvalidName {
        reason: "empty".to_string(),
    };
    let json = serde_json::to_string(&err).expect("err");
    assert!(json.contains("invalid_name"));
    assert!(json.contains("empty"));

    let close_err = CloseContextError::NotFound;
    assert!(
        serde_json::to_string(&close_err)
            .expect("close_err")
            .contains("not_found")
    );
}

#[test]
fn context_event_variants_are_tagged() {
    let ev = ContextEvent::Created {
        context_id: uuid::Uuid::nil(),
        name: Some("work".to_string()),
    };
    let json = serde_json::to_string(&ev).expect("serialize");
    assert!(json.contains("\"created\""));
}

#[test]
fn interface_ids_match_bpdl_source() {
    assert_eq!(contexts_state::INTERFACE_ID, "contexts-state");
    assert_eq!(contexts_events::INTERFACE_ID, "contexts-events");
}

#[test]
fn event_kind_is_namespaced_by_plugin_id() {
    assert_eq!(contexts_events::EVENT_KIND, "bmux.contexts/contexts-events");
}

#[test]
fn event_payload_alias_matches_declared_type() {
    let via_alias: contexts_events::EventPayload = contexts_events::EventPayload::Closed {
        context_id: uuid::Uuid::nil(),
    };
    let via_type: ContextEvent = ContextEvent::Closed {
        context_id: uuid::Uuid::nil(),
    };
    assert_eq!(
        serde_json::to_string(&via_alias).unwrap(),
        serde_json::to_string(&via_type).unwrap()
    );
}

#[test]
fn session_active_context_changed_round_trips() {
    // Regression: the multi-client retarget broadcast carries
    // session id, context id, and the initiating client (None =
    // server-initiated) so attach runtimes can apply follow policy.
    let initiator = uuid::Uuid::from_u128(0x1234);
    let session = uuid::Uuid::from_u128(0x0A01);
    let context = uuid::Uuid::from_u128(0x0B02);
    let ev = ContextEvent::SessionActiveContextChanged {
        session_id: session,
        context_id: context,
        initiator_client_id: Some(initiator),
    };
    let json = serde_json::to_string(&ev).expect("serialize");
    assert!(json.contains("session_active_context_changed"), "{json}");
    assert!(json.contains(&session.to_string()), "{json}");
    assert!(json.contains(&context.to_string()), "{json}");
    assert!(json.contains(&initiator.to_string()), "{json}");
    let round: ContextEvent = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(ev, round);
}

#[test]
fn session_active_context_changed_accepts_none_initiator() {
    let ev = ContextEvent::SessionActiveContextChanged {
        session_id: uuid::Uuid::nil(),
        context_id: uuid::Uuid::nil(),
        initiator_client_id: None,
    };
    let json = serde_json::to_string(&ev).expect("serialize");
    let round: ContextEvent = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(ev, round);
}

#[test]
fn typed_client_list_contexts_uses_generated_route() {
    let context = ContextSummary {
        id: uuid::Uuid::from_u128(0x1234),
        name: Some("work".to_string()),
        attributes: std::collections::BTreeMap::new(),
    };
    let mut client = FakeClient::new(&vec![context.clone()]);

    let result = block_on(contexts_state::client::list_contexts(&mut client))
        .expect("list contexts should decode");

    assert_eq!(result, vec![context]);
    assert_eq!(
        client.last_capability.as_deref(),
        Some(CONTEXTS_READ.as_str())
    );
    assert_eq!(client.last_kind, Some(InvokeServiceKind::Query));
    assert_eq!(
        client.last_interface.as_deref(),
        Some(contexts_state::INTERFACE_ID.as_str())
    );
    assert_eq!(
        client.last_operation.as_deref(),
        Some(contexts_state::OP_LIST_CONTEXTS.as_str())
    );
}

#[test]
fn typed_client_create_context_encodes_args_and_decodes_ack() {
    let ack = ContextAck {
        id: uuid::Uuid::from_u128(0x5678),
        session_id: None,
    };
    let response: Result<ContextAck, CreateContextError> = Ok(ack.clone());
    let mut client = FakeClient::new(&response);
    let mut attributes = std::collections::BTreeMap::new();
    attributes.insert("project".to_string(), "bmux".to_string());

    let result = block_on(contexts_commands::client::create_context(
        &mut client,
        Some("work".to_string()),
        attributes,
    ))
    .expect("create context should decode")
    .expect("create context should succeed");

    assert_eq!(result, ack);
    assert_eq!(
        client.last_capability.as_deref(),
        Some(CONTEXTS_WRITE.as_str())
    );
    assert_eq!(client.last_kind, Some(InvokeServiceKind::Command));
    assert_eq!(
        client.last_interface.as_deref(),
        Some(bmux_contexts_plugin_api::contexts_commands::INTERFACE_ID.as_str())
    );
    assert_eq!(
        client.last_operation.as_deref(),
        Some(bmux_contexts_plugin_api::contexts_commands::OP_CREATE_CONTEXT.as_str())
    );
    #[derive(serde::Deserialize)]
    struct CreateArgs {
        name: Option<String>,
        attributes: std::collections::BTreeMap<String, String>,
    }
    let payload = client.last_payload.expect("payload should be captured");
    let args: CreateArgs = bmux_ipc::decode(&payload).expect("payload should decode");
    assert_eq!(args.name.as_deref(), Some("work"));
    assert_eq!(
        args.attributes.get("project").map(String::as_str),
        Some("bmux")
    );
}
