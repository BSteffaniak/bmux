# bmux_attach_layout_protocol

Typed state-channel protocol that lets any plugin observe the
current attach-layout surfaces without relying on decoration-specific
push calls.

The attach runtime publishes an `attach-layout-snapshot` every time
its surface layout changes. Subscribers (via
`EventBus::subscribe_state::<AttachLayoutSnapshot>`) see the current
value on subscribe plus a live-update stream for subsequent layout
changes.

Use cases:

- A decoration plugin subscribes to update its per-pane geometry
  cache.
- An overlay plugin subscribes to recompute interactive-region
  projections for its modal surfaces.

The protocol is domain-agnostic — no decoration, no overlay, no
specific plugin is named.
