# bmux_codec_derive

Derive macros for flattening nested enums into a single serde variant space.

## Problem

Protocols outgrow a single enum. A message type with a hundred variants is hard to maintain and
forces every consumer to depend on every domain's payload types. The natural fix is to group
variants into per-domain enums:

```rust,ignore
enum Request {
    Session(SessionRequest),
    Permission(PermissionRequest),
}
```

But that changes the wire format: the outer variant name is encoded alongside the inner one. Serde
offers no way out — `#[serde(untagged)]` serializes correctly but cannot deserialize from
self-describing formats that present enum input, and `#[serde(flatten)]` does not apply to enums.

## Usage

```rust,ignore
use bmux_codec_derive::{FlattenedEnum, FlattenedVariants};

#[derive(FlattenedVariants)]
#[serde(rename_all = "snake_case")]
enum PermissionRequest {
    ListPermissions,
    ResolvePermission { permission_id: String, approved: bool },
}

#[derive(FlattenedVariants)]
#[serde(rename_all = "snake_case")]
enum SessionRequest {
    SessionHistory { session_id: String },
}

#[derive(FlattenedEnum)]
enum Request {
    #[flattened]
    Permission(PermissionRequest),
    #[flattened]
    Session(SessionRequest),
}
```

`Request` now encodes exactly like a flat enum containing `list_permissions`,
`resolve_permission`, and `session_history`, so a nested type can replace a flat one without a
protocol change.

Because both directions are generated from one declaration, serialization and deserialization
cannot drift, and an unrouted variant is a compile error rather than a runtime failure.

## Requirements and limits

* Every variant of a `FlattenedEnum` must be `#[flattened]` and hold exactly one enum implementing
  `bmux_codec::FlattenedVariants`. Mixing flattened and plain variants is rejected, because a plain
  variant's own name would silently occupy the same flat namespace.
* Variant names must be unique across the whole flattened space.
* Flattening is single-level per declaration; nested domains compose by deriving at each level.
* **Name-based encodings only.** Positional encodings identify variants by index, and a domain enum
  numbers its own variants from zero, so flattened positional bytes cannot match a flat enum's. Use
  `stable` or `typed-stable` when replacing a flat enum with a flattened one.

## Why the runtime half lives in `bmux_codec`

Serde's `VariantAccess` is consumed by the first read of the variant payload, so a flattened
deserializer cannot try each domain in turn and rewind on failure. It must know which domain owns a
variant name *before* touching the payload. The `FlattenedVariants` trait provides that routing
table, and since a `proc-macro` crate can only export macros, the trait and the
`FlattenedVariantName` identifier helper live in `bmux_codec` behind its `serde` feature.
