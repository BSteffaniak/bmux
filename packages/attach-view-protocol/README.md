# bmux_attach_view_protocol

Neutral attach view and client-local presentation protocol DTOs shared by bmux
runtime crates.

This crate defines stable wire types for coarse attached-view component changes
that may require resynchronization, plus the resolved local labels and hints an
attach presentation companion may place and style. It is a protocol/model crate
only and does not implement attach runtime behavior, domain catalogs, access
policy, or plugin orchestration.
