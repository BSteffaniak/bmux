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

## Usage

```rust
use bmux_terminal_protocol::{protocol_profile_for_term, ProtocolProfile};

// Select profile based on TERM value
let profile = protocol_profile_for_term("xterm-256color");
assert_eq!(profile, ProtocolProfile::Xterm);
```
