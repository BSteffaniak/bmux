#!/usr/bin/env bash
# Verify the CLI's local/remote dependency boundary, not just Cargo.lock contents.
set -euo pipefail
check_graph() {
    local expected=$1 graph
    shift
    graph=$(cargo tree --locked -p bmux_cli -e normal,build --prefix none "$@")
    if grep -q '^iroh v' <<<"$graph"; then
        [[ $expected == present ]] || { echo "Unexpected Iroh dependency: $*" >&2; return 1; }
    else
        [[ $expected == absent ]] || { echo "Missing Iroh dependency: $*" >&2; return 1; }
    fi
}
check_graph absent --no-default-features
check_graph absent --no-default-features --features bundled-plugin-pane-runtime,bundled-plugin-windows,bundled-plugin-sessions,bundled-plugin-contexts,bundled-plugin-clients
for feature in remote-iroh bundled-plugin-connections bundled-plugin-cluster; do
    check_graph present --no-default-features --features "$feature"
done
check_graph present
