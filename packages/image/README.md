# bmux_image

Terminal image protocol support for bmux.

## Overview

Containment boundary for all image-related logic in bmux. Intercepts image
protocol escape sequences from pane output, maintains per-pane image
registries, and provides a compositor overlay layer for rendering images
alongside text. Feature-gated per protocol.

## Features

- **Sixel** graphics interception and registry (`sixel` feature)
- **Kitty** graphics protocol support (`kitty` feature)
- **iTerm2** inline image protocol support (`iterm2` feature)
- Host capability detection for negotiating supported protocols
- Per-pane image registry with placement tracking
- Compositor overlay layer for image rendering during attach

## Core Types

- **`ImageInterceptor`**: Parses image protocol sequences out of terminal output
- **`InterceptResult`**: Outcome of interception (passthrough, consumed, or stored placement)
- **`ImageRegistry`**: Per-pane storage of active image placements
- **`ImageConfig`**: Protocol-specific configuration
- **`HostImageCapabilities`**: Detected terminal image support

## Modules

- **`codec`**: Protocol-specific encoding/decoding
- **`compositor`**: Overlay rendering for image placements
- **`intercept`**: Escape sequence interception state machine
- **`registry`**: Per-pane image storage and lookup
- **`host_caps`**: Terminal capability detection
- **`ipc_convert`**: Serialization for IPC transport
