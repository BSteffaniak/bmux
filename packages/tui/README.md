# bmux_tui

Native terminal UI primitives for BMUX.

This crate is intentionally domain-agnostic. It provides reusable geometry,
layout, styled text, and render-buffer foundations for terminal interfaces.
BMUX sessions, windows, panes, clients, contexts, permissions, and product UI
behavior live outside this crate.

See [`../../docs/tui-framework.md`](../../docs/tui-framework.md) for the broader architecture and roadmap.
