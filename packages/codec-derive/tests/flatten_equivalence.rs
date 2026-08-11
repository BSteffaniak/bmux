//! Phase B: prove a flattened (domain-composed) enum encodes identically to a flat enum.
//!
//! This is the contract consumers rely on when replacing a large flat enum with domain enums: the
//! wire form must not change, in any encoding mode, for any variant shape.

use bmux_codec_derive::{FlattenedEnum, FlattenedVariants};
use serde::{Deserialize, Serialize};

/// Payload used to exercise newtype variants.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct WorktreeList {
    working_directory: String,
}

/// The current shape: one flat enum holding every variant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum FlatRequest {
    Ping,
    ListPermissions,
    ResolvePermission {
        permission_id: String,
        approved: bool,
        remember: bool,
    },
    SessionHistory {
        session_id: String,
    },
    ListWorktrees(WorktreeList),
}

/// Domain enum: daemon-level requests.
#[derive(Debug, Clone, PartialEq, Eq, FlattenedVariants)]
#[serde(rename_all = "snake_case")]
enum DaemonRequest {
    Ping,
}

/// Domain enum: permission requests.
#[derive(Debug, Clone, PartialEq, Eq, FlattenedVariants)]
#[serde(rename_all = "snake_case")]
enum PermissionRequest {
    ListPermissions,
    ResolvePermission {
        permission_id: String,
        approved: bool,
        remember: bool,
    },
}

/// Domain enum: session requests.
#[derive(Debug, Clone, PartialEq, Eq, FlattenedVariants)]
#[serde(rename_all = "snake_case")]
enum SessionRequest {
    SessionHistory { session_id: String },
}

/// Domain enum: worktree requests.
#[derive(Debug, Clone, PartialEq, Eq, FlattenedVariants)]
#[serde(rename_all = "snake_case")]
enum WorktreeRequest {
    ListWorktrees(WorktreeList),
}

/// The composed shape: domains hoisted into one flat variant space.
#[derive(Debug, Clone, PartialEq, Eq, FlattenedEnum)]
enum ComposedRequest {
    #[flattened]
    Daemon(DaemonRequest),
    #[flattened]
    Permission(PermissionRequest),
    #[flattened]
    Session(SessionRequest),
    #[flattened]
    Worktree(WorktreeRequest),
}

fn cases() -> Vec<(FlatRequest, ComposedRequest)> {
    vec![
        (
            FlatRequest::Ping,
            ComposedRequest::Daemon(DaemonRequest::Ping),
        ),
        (
            FlatRequest::ListPermissions,
            ComposedRequest::Permission(PermissionRequest::ListPermissions),
        ),
        (
            FlatRequest::ResolvePermission {
                permission_id: "p-1".to_owned(),
                approved: true,
                remember: false,
            },
            ComposedRequest::Permission(PermissionRequest::ResolvePermission {
                permission_id: "p-1".to_owned(),
                approved: true,
                remember: false,
            }),
        ),
        (
            FlatRequest::SessionHistory {
                session_id: "s-1".to_owned(),
            },
            ComposedRequest::Session(SessionRequest::SessionHistory {
                session_id: "s-1".to_owned(),
            }),
        ),
        (
            FlatRequest::ListWorktrees(WorktreeList {
                working_directory: "/tmp".to_owned(),
            }),
            ComposedRequest::Worktree(WorktreeRequest::ListWorktrees(WorktreeList {
                working_directory: "/tmp".to_owned(),
            })),
        ),
    ]
}

#[test]
fn typed_stable_encoding_is_byte_identical() {
    for (flat, composed) in cases() {
        let flat_bytes = bmux_codec::to_typed_vec(&flat).expect("encode flat");
        let composed_bytes = bmux_codec::to_typed_vec(&composed).expect("encode composed");
        assert_eq!(
            flat_bytes, composed_bytes,
            "typed-stable bytes must match for {flat:?}"
        );
    }
}

#[test]
fn stable_encoding_is_byte_identical() {
    for (flat, composed) in cases() {
        let flat_bytes = bmux_codec::to_vec(&flat).expect("encode flat");
        let composed_bytes = bmux_codec::to_vec(&composed).expect("encode composed");
        assert_eq!(
            flat_bytes, composed_bytes,
            "stable bytes must match for {flat:?}"
        );
    }
}

#[test]
fn positional_encoding_differs_and_is_unsupported() {
    // Positional mode identifies variants by index, and a domain enum numbers its variants from
    // zero. A flattened enum therefore CANNOT reproduce flat positional bytes: `list_permissions`
    // is index 1 in the flat enum but index 0 in `PermissionRequest`.
    //
    // This is a documented limitation rather than a bug. Consumers replacing a flat enum with a
    // flattened one must use a name-based mode (`stable` or `typed-stable`). This test pins the
    // behaviour so the limitation cannot regress silently into a false compatibility claim.
    let flat_bytes = bmux_codec::to_positional_vec(&FlatRequest::ListPermissions).expect("flat");
    let composed_bytes = bmux_codec::to_positional_vec(&ComposedRequest::Permission(
        PermissionRequest::ListPermissions,
    ))
    .expect("composed");

    assert_eq!(
        flat_bytes,
        vec![1],
        "flat enum encodes its own variant index"
    );
    assert_eq!(
        composed_bytes,
        vec![0],
        "domain enum encodes a domain-local index, so positional bytes diverge"
    );
    assert_ne!(
        flat_bytes, composed_bytes,
        "positional compatibility is not offered; use a name-based encoding mode"
    );
}

#[test]
fn composed_decodes_existing_flat_bytes() {
    for (flat, composed) in cases() {
        let flat_bytes = bmux_codec::to_typed_vec(&flat).expect("encode flat");
        let decoded: ComposedRequest =
            bmux_codec::from_typed_bytes(&flat_bytes).expect("decode flat bytes as composed");
        assert_eq!(
            decoded, composed,
            "flat bytes must route to the owning domain"
        );
    }
}

#[test]
fn flat_decodes_composed_bytes() {
    for (flat, composed) in cases() {
        let composed_bytes = bmux_codec::to_typed_vec(&composed).expect("encode composed");
        let decoded: FlatRequest =
            bmux_codec::from_typed_bytes(&composed_bytes).expect("decode composed as flat");
        assert_eq!(decoded, flat, "existing readers must accept composed bytes");
    }
}

/// A boxed domain keeps its variants in the same flat space and the same wire form.
///
/// Large domains are boxed so they do not set the composed enum's size, so this must hold for the
/// composition to be usable.
#[test]
fn boxed_domain_preserves_the_flat_wire_form() {
    #[derive(Debug, Clone, PartialEq, Eq, FlattenedEnum)]
    enum BoxedComposed {
        #[flattened]
        Daemon(DaemonRequest),
        #[flattened]
        Permission(Box<PermissionRequest>),
    }

    let flat = FlatRequest::ResolvePermission {
        permission_id: "p-1".to_owned(),
        approved: true,
        remember: false,
    };
    let boxed = BoxedComposed::Permission(Box::new(PermissionRequest::ResolvePermission {
        permission_id: "p-1".to_owned(),
        approved: true,
        remember: false,
    }));

    let flat_bytes = bmux_codec::to_typed_vec(&flat).expect("encode flat");
    let boxed_bytes = bmux_codec::to_typed_vec(&boxed).expect("encode boxed composed");
    assert_eq!(
        flat_bytes, boxed_bytes,
        "boxing must not change the wire form"
    );

    let decoded: BoxedComposed =
        bmux_codec::from_typed_bytes(&flat_bytes).expect("decode flat bytes as boxed composed");
    assert_eq!(decoded, boxed);
}

#[test]
fn flattened_variant_space_is_reported() {
    let names = ComposedRequest::flattened_variants();
    assert!(names.contains(&"ping"));
    assert!(names.contains(&"resolve_permission"));
    assert!(names.contains(&"list_worktrees"));
    assert_eq!(
        names.len(),
        5,
        "every domain variant must be listed: {names:?}"
    );
}
