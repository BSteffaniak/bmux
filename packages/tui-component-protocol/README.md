# BMUX TUI Component Protocol

Serializable declarative component models for BMUX-hosted TUI surfaces.

This crate intentionally contains protocol data only. Rendering and input
handling live in BMUX TUI/component runtime crates, while plugins and hosts can
exchange these models without depending on concrete widget implementations.

Serialization support is opt-in:

- `serde` enables serde derives on protocol models.
- `serde-json` enables JSON helper functions.
- `bmux-codec` enables BMUX codec helper functions.
