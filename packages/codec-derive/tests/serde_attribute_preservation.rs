//! Verify the flattening derive preserves field-level serde attributes.
//!
//! `Request` variants use `#[serde(default)]` on optional fields. The derive generates a private
//! `Fields` struct per struct-shaped variant; if it drops those attributes, decoding a payload that
//! omits the field breaks. This test pins that behaviour before the migration relies on it.

use bmux_codec_derive::{FlattenedEnum, FlattenedVariants};
use serde::{Deserialize, Serialize};

/// Reference flat shape with a defaulted optional field.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum FlatRequest {
    ServerStatus {
        #[serde(default)]
        working_directory: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, FlattenedVariants)]
#[serde(rename_all = "snake_case")]
enum DaemonRequest {
    ServerStatus {
        #[serde(default)]
        working_directory: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, FlattenedEnum)]
enum ComposedRequest {
    #[flattened]
    Daemon(DaemonRequest),
}

#[test]
fn defaulted_fields_round_trip_through_the_composed_form() {
    let flat = FlatRequest::ServerStatus {
        working_directory: None,
    };
    let composed = ComposedRequest::Daemon(DaemonRequest::ServerStatus {
        working_directory: None,
    });

    let flat_bytes = bmux_codec::to_typed_vec(&flat).expect("encode flat");
    let composed_bytes = bmux_codec::to_typed_vec(&composed).expect("encode composed");
    assert_eq!(flat_bytes, composed_bytes, "wire bytes must match");

    let decoded: ComposedRequest =
        bmux_codec::from_typed_bytes(&flat_bytes).expect("decode flat bytes as composed");
    assert_eq!(decoded, composed);
}

#[test]
fn omitted_defaulted_field_still_decodes() {
    // `bmux_codec` writes every field, so a same-version peer always supplies it. A *legacy* peer
    // that predates the field omits it entirely, which is exactly what `#[serde(default)]` exists
    // to tolerate. Encode a narrower shape to simulate that peer.
    #[derive(Serialize)]
    #[serde(rename_all = "snake_case")]
    enum LegacyRequest {
        ServerStatus {},
    }

    let legacy_bytes = bmux_codec::to_typed_vec(&LegacyRequest::ServerStatus {}).expect("encode");

    let flat: Result<FlatRequest, _> = bmux_codec::from_typed_bytes(&legacy_bytes);
    let composed: Result<ComposedRequest, _> = bmux_codec::from_typed_bytes(&legacy_bytes);

    // The composed form must tolerate the omission exactly as the flat form does; if the derive
    // dropped `#[serde(default)]` these would disagree.
    assert!(
        flat.is_ok(),
        "the flat form must tolerate a legacy peer omitting a defaulted field: {flat:?}"
    );
    assert_eq!(
        flat.is_ok(),
        composed.is_ok(),
        "flat={flat:?} composed={composed:?}"
    );
}

/// A field type that cannot be defaulted, so `#[serde(default = "...")]` must be forwarded for the
/// generated struct to compile at all.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct RequiredMarker(u8);

fn default_marker() -> RequiredMarker {
    RequiredMarker(7)
}

#[derive(Debug, Clone, PartialEq, Eq, FlattenedVariants)]
#[serde(rename_all = "snake_case")]
enum AttributedRequest {
    Marked {
        #[serde(default = "default_marker")]
        marker: RequiredMarker,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, FlattenedEnum)]
enum AttributedComposed {
    #[flattened]
    Attributed(AttributedRequest),
}

/// If the derive dropped field attributes, `default = "default_marker"` would be lost and the
/// omitted-field payload below would fail to decode.
#[test]
fn custom_default_attribute_is_forwarded_to_the_generated_struct() {
    #[derive(Serialize)]
    #[serde(rename_all = "snake_case")]
    enum LegacyMarked {
        Marked {},
    }

    let legacy_bytes = bmux_codec::to_typed_vec(&LegacyMarked::Marked {}).expect("encode");
    let decoded: AttributedComposed =
        bmux_codec::from_typed_bytes(&legacy_bytes).expect("custom default must be honored");
    assert_eq!(
        decoded,
        AttributedComposed::Attributed(AttributedRequest::Marked {
            marker: default_marker(),
        })
    );
}

#[test]
fn populated_fields_round_trip_through_the_composed_form() {
    let flat = FlatRequest::ServerStatus {
        working_directory: Some("/repo".to_owned()),
    };
    let composed = ComposedRequest::Daemon(DaemonRequest::ServerStatus {
        working_directory: Some("/repo".to_owned()),
    });

    let flat_bytes = bmux_codec::to_typed_vec(&flat).expect("encode flat");
    let composed_bytes = bmux_codec::to_typed_vec(&composed).expect("encode composed");
    assert_eq!(flat_bytes, composed_bytes, "wire bytes must match");

    let decoded: ComposedRequest =
        bmux_codec::from_typed_bytes(&flat_bytes).expect("decode flat bytes as composed");
    assert_eq!(decoded, composed);
}
