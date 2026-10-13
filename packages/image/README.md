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

## Opt-in performance workloads

The dependency-light `image_pipeline` benchmark generates fixed image data and
reports encoder and public TUI/attach compositor timing distributions plus
bytes/chunk/work counts. Select fixed `palette`, `solid`, `gradient`,
`transparent`, or `detailed` fixtures with `--fixture`. A frozen pre-optimization
Sixel encoder provides a same-build, same-fixture reference. It writes no graphics to the terminal and has no
wall-clock pass/fail thresholds. See [reproduction and audit notes](../../docs/image-performance.md)
for release commands, baseline limitations, and separate nested-terminal repro.

```sh
bash scripts/dev.sh cargo bench -p bmux_image --bench image_pipeline \
  --features kitty,sixel,iterm2 -- --iterations 20 --json
```

`tests/image_work_regression.rs` checks deterministic public encoder and retained
compositor behavior without elapsed-time gates.
