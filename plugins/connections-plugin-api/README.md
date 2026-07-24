# bmux_connections_plugin_api

Stable, transport-neutral typed contract for resolving configured bmux connection targets and invoking typed services on a selected endpoint.

The API exposes endpoint descriptions and opaque typed-service request/response bytes. It deliberately contains no cluster, workspace, placement, or gateway-selection policy. Runtime target resolution, SSH/TLS/Iroh connection establishment, trust checks, and IO belong in `bmux_connections_plugin`.
