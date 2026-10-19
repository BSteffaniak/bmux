//! Owning-plugin adapter between local attach input and generated hook transport.

use std::sync::Arc;

use bmux_decoration_plugin_api::decoration_input_hooks::{self as wire, client};
use bmux_plugin::render::AttachInputCodec;
use bmux_plugin::{AttachInputEndpoint, AttachInputEvent, AttachInputResult};
use bmux_plugin_sdk::TypedServiceEndpoint;

struct DecorationInputCodec;

/// Checked input codec used by the attach-side decoration renderer.
#[must_use]
pub fn input_codec() -> Arc<dyn AttachInputCodec> {
    Arc::new(DecorationInputCodec)
}

impl AttachInputCodec for DecorationInputCodec {
    fn endpoint(&self) -> AttachInputEndpoint {
        AttachInputEndpoint {
            capability: client::HandleInputEndpoint::CAPABILITY.to_string(),
            interface_id: client::HandleInputEndpoint::INTERFACE_ID.to_string(),
            operation: client::HandleInputEndpoint::OPERATION.to_string(),
        }
    }

    fn encode_event(&self, event: &AttachInputEvent) -> Result<Vec<u8>, String> {
        let payload = bmux_plugin_sdk::encode_service_message(&client::HandleInputRequest {
            event: event_to_wire(event.clone()),
        })
        .map_err(|error| error.to_string())?;
        bmux_plugin_sdk::service_contract::encode(
            client::HandleInputEndpoint::INTERFACE_ID.as_str(),
            client::HandleInputEndpoint::SERVICE_CONTRACT,
            &payload,
        )
        .map_err(|error| error.to_string())
    }

    fn decode_result(&self, payload: &[u8]) -> Result<AttachInputResult, String> {
        let accepted = bmux_plugin_sdk::service_contract::decode_response(payload)
            .map_err(|error| error.to_string())?;
        bmux_plugin_sdk::decode_service_message::<wire::InputResult>(accepted)
            .map(result_from_wire)
            .map_err(|error| error.to_string())
    }
}

fn rect_to_wire(value: bmux_plugin::ExtensionRect) -> wire::InputRect {
    wire::InputRect {
        x: value.x,
        y: value.y,
        w: value.w,
        h: value.h,
    }
}

fn rect_from_wire(value: &wire::InputRect) -> bmux_plugin::ExtensionRect {
    bmux_plugin::ExtensionRect {
        x: value.x,
        y: value.y,
        w: value.w,
        h: value.h,
    }
}

fn modifiers_to_wire(value: bmux_plugin::AttachInputModifiers) -> wire::InputModifiers {
    wire::InputModifiers {
        shift: value.shift,
        alt: value.alt,
        control: value.control,
        super_key: value.super_key,
        hyper: value.hyper,
        meta: value.meta,
    }
}

fn modifiers_from_wire(value: &wire::InputModifiers) -> bmux_plugin::AttachInputModifiers {
    bmux_plugin::AttachInputModifiers {
        shift: value.shift,
        alt: value.alt,
        control: value.control,
        super_key: value.super_key,
        hyper: value.hyper,
        meta: value.meta,
    }
}

fn pane_to_wire(value: &bmux_plugin::AttachInputPaneContext) -> wire::InputPaneContext {
    wire::InputPaneContext {
        pane_id: value.pane_id,
        surface_id: value.surface_id,
        rect: rect_to_wire(value.rect),
        content_rect: rect_to_wire(value.content_rect),
        focused: value.focused,
    }
}

fn pane_from_wire(value: &wire::InputPaneContext) -> bmux_plugin::AttachInputPaneContext {
    bmux_plugin::AttachInputPaneContext {
        pane_id: value.pane_id,
        surface_id: value.surface_id,
        rect: rect_from_wire(&value.rect),
        content_rect: rect_from_wire(&value.content_rect),
        focused: value.focused,
    }
}

pub(super) fn event_to_wire(value: bmux_plugin::AttachInputEvent) -> wire::InputEvent {
    wire::InputEvent {
        hook_id: value.hook_id,
        event_kind: value.event_kind,
        phase: value.phase,
        button: value.button,
        key: value.key,
        col: value.col,
        row: value.row,
        wheel_delta: value.wheel_delta,
        modifiers: modifiers_to_wire(value.modifiers),
        focused_pane: value.focused_pane.as_ref().map(pane_to_wire),
        hovered_pane: value.hovered_pane.as_ref().map(pane_to_wire),
    }
}

pub(super) fn event_from_wire(value: wire::InputEvent) -> bmux_plugin::AttachInputEvent {
    bmux_plugin::AttachInputEvent {
        hook_id: value.hook_id,
        event_kind: value.event_kind,
        phase: value.phase,
        button: value.button,
        key: value.key,
        col: value.col,
        row: value.row,
        wheel_delta: value.wheel_delta,
        modifiers: modifiers_from_wire(&value.modifiers),
        focused_pane: value.focused_pane.as_ref().map(pane_from_wire),
        hovered_pane: value.hovered_pane.as_ref().map(pane_from_wire),
    }
}

fn endpoint_to_wire(value: bmux_plugin::AttachInputEndpoint) -> wire::InputEndpoint {
    wire::InputEndpoint {
        capability: value.capability,
        interface_id: value.interface_id,
        operation: value.operation,
    }
}

fn endpoint_from_wire(value: wire::InputEndpoint) -> bmux_plugin::AttachInputEndpoint {
    bmux_plugin::AttachInputEndpoint {
        capability: value.capability,
        interface_id: value.interface_id,
        operation: value.operation,
    }
}

fn invocation_to_wire(
    value: bmux_plugin::AttachInputServiceInvocation,
) -> wire::InputServiceInvocation {
    wire::InputServiceInvocation {
        endpoint: endpoint_to_wire(value.endpoint),
        response_endpoint: value
            .response_endpoint
            .map(|endpoint| endpoint_to_wire(*endpoint)),
        payload: value.payload,
    }
}

fn invocation_from_wire(
    value: wire::InputServiceInvocation,
) -> bmux_plugin::AttachInputServiceInvocation {
    bmux_plugin::AttachInputServiceInvocation {
        endpoint: endpoint_from_wire(value.endpoint),
        response_endpoint: value
            .response_endpoint
            .map(|endpoint| Box::new(endpoint_from_wire(endpoint))),
        payload: value.payload,
    }
}

pub(super) fn result_to_wire(value: bmux_plugin::AttachInputResult) -> wire::InputResult {
    wire::InputResult {
        consumed: value.consumed,
        preserve_focus: value.preserve_focus,
        capture_pointer: value.capture_pointer,
        capture_keyboard: value.capture_keyboard,
        release_capture: value.release_capture,
        preserve_capture_across_revisions: value.preserve_capture_across_revisions,
        dirty: value.dirty,
        status_message: value.status_message,
        service_invocation: value.service_invocation.map(invocation_to_wire),
    }
}

pub(super) fn result_from_wire(value: wire::InputResult) -> bmux_plugin::AttachInputResult {
    bmux_plugin::AttachInputResult {
        consumed: value.consumed,
        preserve_focus: value.preserve_focus,
        capture_pointer: value.capture_pointer,
        capture_keyboard: value.capture_keyboard,
        release_capture: value.release_capture,
        preserve_capture_across_revisions: value.preserve_capture_across_revisions,
        dirty: value.dirty,
        status_message: value.status_message,
        service_invocation: value.service_invocation.map(invocation_from_wire),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bmux_plugin_sdk::PluginContract;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn event() -> AttachInputEvent {
        AttachInputEvent {
            hook_id: "test".into(),
            event_kind: "pointer".into(),
            phase: "wheel".into(),
            button: None,
            key: None,
            col: Some(7),
            row: Some(9),
            wheel_delta: -3,
            modifiers: bmux_plugin::AttachInputModifiers::default(),
            focused_pane: Some(bmux_plugin::AttachInputPaneContext {
                pane_id: uuid::Uuid::nil(),
                surface_id: uuid::Uuid::from_u128(1),
                rect: bmux_plugin::ExtensionRect::new(1, 2, 30, 40),
                content_rect: bmux_plugin::ExtensionRect::new(2, 3, 28, 38),
                focused: true,
            }),
            hovered_pane: None,
        }
    }

    #[test]
    fn generated_input_contract_interoperates_after_provider_acceptance() {
        let codec = input_codec();
        let event = event();
        let framed = codec.encode_event(&event).unwrap();
        let interface = codec.endpoint().interface_id;
        assert_eq!(interface, "decoration-input-hooks/v2");
        let contract = bmux_decoration_plugin_api::Contract::service_contract_for(&interface)
            .expect("owning provider declares input interface");
        let payload =
            bmux_plugin_sdk::service_contract::decode_for(&interface, contract, &framed).unwrap();
        let request: client::HandleInputRequest =
            bmux_plugin_sdk::decode_service_message(payload).unwrap();
        assert_eq!(event_from_wire(request.event), event);
        let result = AttachInputResult {
            consumed: true,
            capture_pointer: true,
            preserve_focus: true,
            dirty: true,
            ..Default::default()
        };
        let response =
            bmux_plugin_sdk::encode_service_message(&result_to_wire(result.clone())).unwrap();
        assert!(codec.decode_result(&response).is_err());
        let response = bmux_plugin_sdk::service_contract::encode_response(&response);
        assert_eq!(codec.decode_result(&response).unwrap(), result);
    }

    #[test]
    fn unsupported_and_mismatched_contracts_cannot_reach_effects() {
        let codec = input_codec();
        let effects = AtomicUsize::new(0);
        let interface = codec.endpoint().interface_id;
        let contract =
            bmux_decoration_plugin_api::Contract::service_contract_for(&interface).unwrap();
        let dispatch = |bytes: &[u8]| -> Result<(), String> {
            let payload =
                bmux_plugin_sdk::service_contract::decode_for(&interface, contract, bytes)
                    .map_err(|error| error.to_string())?;
            let _: client::HandleInputRequest = bmux_plugin_sdk::decode_service_message(payload)
                .map_err(|error| error.to_string())?;
            effects.fetch_add(1, Ordering::SeqCst);
            Ok(())
        };
        let bare = bmux_plugin_sdk::encode_service_message(&event()).unwrap();
        assert!(dispatch(&bare).is_err());
        assert!(dispatch(b"BMUX-BPDL-CHECKED-1\0").is_err());
        let wrong = bmux_plugin_sdk::service_contract::encode(&interface, &[0; 32], &bare).unwrap();
        assert!(dispatch(&wrong).is_err());
        let wrong =
            bmux_plugin_sdk::service_contract::encode("decoration-input-hooks", contract, &bare)
                .unwrap();
        assert!(dispatch(&wrong).is_err());
        assert_eq!(effects.load(Ordering::SeqCst), 0);
        dispatch(&codec.encode_event(&event()).unwrap()).unwrap();
        assert_eq!(effects.load(Ordering::SeqCst), 1);
        assert!(
            bmux_decoration_plugin_api::Contract::service_contract_for("decoration-input-hooks")
                .is_none()
        );
    }
}
