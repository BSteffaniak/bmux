# bmux_keyboard

Key types and keyboard protocol encoding/decoding for bmux.

## Overview

Defines canonical key types and provides encoders for multiple terminal keyboard
protocols. The `legacy` module handles traditional VT/xterm escape sequences,
while the `csi_u` module (behind the `csi-u` feature) implements the modern
Kitty keyboard protocol. The `encode` module selects the appropriate encoding
based on terminal capabilities.

## Features

- Canonical key types independent of any terminal protocol
- Legacy VT/xterm escape sequence encoding and decoding
- Kitty keyboard protocol (CSI u) encoding and decoding (`csi-u` feature)
- Unified encoding entry point that selects protocol automatically
- Modifier key support (Shift, Ctrl, Alt, Super, Hyper, Meta)

## Core Types

- **`KeyCode`**: Platform-neutral key identifier (`Char`, `Enter`, `Esc`, `Tab`, `F(n)`, arrows, etc.)
- **`Modifiers`**: Bitflag set of active modifier keys
- **`KeyStroke`**: A `KeyCode` combined with `Modifiers`

## Modules

- **`legacy`**: VT/xterm sequence encoding and decoding
- **`csi_u`**: Kitty keyboard protocol (CSI u) encoding and decoding
- **`encode`**: Unified entry point that picks the right encoder
- **`types`**: Core type definitions

## Usage

```rust
use bmux_keyboard::{KeyCode, KeyStroke, Modifiers};
use bmux_keyboard::encode;

let stroke = KeyStroke::new(KeyCode::Char('c'), Modifiers::CTRL);
let bytes = encode::encode_keystroke(&stroke, /* kitty_enabled */ false);
```
