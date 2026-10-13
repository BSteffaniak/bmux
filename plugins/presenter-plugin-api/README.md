# Presentation plugin API

Stable generated BPDL contracts only. `presenter-view-v2` provides bounded
revision polling, explicit bootstrap snapshots (including submit-only),
authentication, monotonic action submission/acknowledgment, asset reads, and leave.
`presenter-host-v1` provides privileged managed host lifecycle operations.

Public deck content and separately authorized notes are distinct snapshot fields.
Runtime state, authorization, TLS, source IO, terminal lifecycle, and ownership
remain in `bmux_presenter_plugin` / `bmux_presenter_runtime`. There are no
handwritten public transport clients or envelopes.
