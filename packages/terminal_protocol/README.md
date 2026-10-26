# bmux_terminal_protocol

Terminal query/reply protocol engine for bmux.

## Overview

Implements a state machine parser for VT/ANSI escape sequences and manages
terminal capability queries and replies. When bmux attaches to a terminal, this
engine sends protocol queries (Device Attributes, XTVERSION, etc.) and
interprets the replies to build a capability profile. The profile determines
which features bmux can use in that terminal.

## Features

- State machine parser for CSI, OSC, DCS, and SOS escape sequences
- Multiple protocol profiles with different query sets
- Automatic profile selection based on the `TERM` environment variable
- Protocol trace recording for diagnostics (`bmux terminal-doctor`)
- Kitty keyboard protocol negotiation (`kitty-keyboard` feature)

## Core Types

- **`TerminalProtocolEngine`**: Main engine that processes output and intercepts query/reply sequences
- **`ProtocolProfile`**: `Bmux`, `Xterm`, `Screen`, or `Conservative` -- controls which queries are sent
- **`ProtocolDirection`**: `Query` (bmux -> terminal) or `Reply` (terminal -> bmux)
- **`ProtocolTraceEvent`**: Timestamped record of a query or reply with decoded details
- **`ProtocolTraceBuffer`**: Ring buffer of trace events for diagnostic output

## OSC 22 pointer shapes

The public `pointer_shape` module provides all 30 published CSS shape names,
borrowed set/push/pop/query operations, fixed 16-entry screen stacks, and
bounded
setter/query-reply encoders. `PointerShapeState` is `Copy`; its serde
representation
requires version 1, validates stack lengths and unused entries, and rejects
unknown versions and shapes. An occupied unset entry is distinct from CSS
`default` and from an empty stack. Empty setters replace the top with unset;
they do not erase lower entries. This follows kitty's `change_pointer_shape`
implementation as well as its published pointer-shapes protocol.

This engine does **not** own or answer OSC 22 state. Feed application output to
the canonical terminal grid and use its ordered query callback. Supply the
callback's current request plus configured default/grabbed shapes in
`PointerShapeQueryContext`, then call `encode_pointer_shape_query_reply`.
Replies are input to the originating application, not output to reparse as
requests: a reply can be byte-identical to a setter.

The payload limit is 1022 bytes after `22;`, matching the canonical grid's
1024-byte OSC storage. Oversized operations are rejected, never truncated.

For an explicitly selected tmux passthrough route,
`encode_pointer_shape_tmux(sequence, output) -> Result<(),
PointerShapeParseError>`
wraps one sequence from `encode_pointer_shape_set`, `encode_pointer_shape_push`,
or `encode_pointer_shape_pop` in DCS `tmux;`, doubles inner ESC bytes, and
closes
with ST. It accepts at most 21 bytes and validates canonical encoder input;
queries, replies outside setter syntax, arbitrary escapes, and concatenated
sequences are not a general passthrough contract. Rejection leaves output
intact.

## Usage

```rust
use bmux_terminal_protocol::{protocol_profile_for_term, ProtocolProfile};

// Select profile based on TERM value
let profile = protocol_profile_for_term("xterm-256color");
assert_eq!(profile, ProtocolProfile::Xterm);
```
