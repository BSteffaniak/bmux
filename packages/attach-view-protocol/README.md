# bmux_attach_view_protocol

Neutral attach view change protocol DTOs shared by bmux runtime crates.

This crate defines stable wire types for coarse attached-view component changes
that may require resynchronization. It is a protocol/model crate only and does
not implement attach runtime behavior or plugin orchestration.
