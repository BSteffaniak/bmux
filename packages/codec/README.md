# bmux_codec

Custom binary serialization codec for the bmux IPC wire protocol.

## Overview

A serde-based binary serializer/deserializer optimized for the bmux wire
protocol. Uses LEB128 varints for compact integer encoding, length-prefixed
containers, and varint enum discriminants. No field names or type tags are
written -- struct fields are serialized in declaration order.

## Wire Format

| Rust type | Wire encoding |
|-----------|---------------|
| `bool` | single byte (0/1) |
| `u8` | single byte |
| `u16`..`u64` | unsigned LEB128 |
| `i8`..`i64` | ZigZag + unsigned LEB128 |
| `f32`/`f64` | little-endian IEEE 754 |
| `&str`/`String` | varint length + UTF-8 bytes |
| `&[u8]`/`Vec<u8>` | varint length + raw bytes |
| `Option<T>` | 0x00 (None) or 0x01 + value |
| `enum` variant | varint discriminant + fields |
| `Vec<T>` / maps | varint length + elements |
| structs | fields in order, no names |

## Core Types

- **`Error`**: Serialization/deserialization error type
- **`varint`** module: LEB128 encoding/decoding helpers

## Usage

```rust
use bmux_codec::{to_vec, from_bytes};
use serde::{Serialize, Deserialize};

#[derive(Serialize, Deserialize)]
struct Msg { id: u32, payload: Vec<u8> }

let msg = Msg { id: 42, payload: vec![1, 2, 3] };
let bytes = to_vec(&msg)?;
let decoded: Msg = from_bytes(&bytes)?;
```
