#!/usr/bin/env bash
# Check the resolved graph, not just the presence of an optional manifest edge.
set -euo pipefail
check_graph() {
    local expected=$1 graph
    shift
    graph=$(cargo tree --locked -p bmux_cli -e normal,build --prefix none "$@")
    if grep -q '^qrcode v' <<<"$graph"; then
        [[ $expected == present ]] || { echo "Unexpected QR encoder: $*" >&2; return 1; }
    else
        [[ $expected == absent ]] || { echo "Missing QR encoder: $*" >&2; return 1; }
    fi
}
check_graph absent --no-default-features
check_graph absent --no-default-features --features bundled-plugin-pane-runtime,bundled-plugin-windows,bundled-plugin-sessions,bundled-plugin-contexts,bundled-plugin-clients
check_graph present --no-default-features --features bundled-plugin-connections
check_graph present
