#!/usr/bin/env bash
# Run from the workspace in its Nix shell. Preserve both variants (same bin name).
set -euo pipefail
out=$(mktemp -d /tmp/bmux-pty-images.XXXXXXXX)
printf 'PTY test images: %s\n' "$out"
mkdir "$out/diagnostics" "$out/bare" "$out/local-splits"
build_cli=(cargo build --locked --offline --no-default-features -p bmux_cli --bin bmux)
# Cargo's artifact path comes from this exact build, not a PATH-installed CLI.
collect() {
    python3 -c '
import json, os, sys
found = set()
for line in sys.stdin:
    item = json.loads(line)
    if item["reason"] == "compiler-artifact" and item.get("executable"):
        path = item["executable"]
        name = os.path.basename(path)
        if name in sys.argv[2:]:
            import shutil
            shutil.copy2(path, os.path.join(sys.argv[1], name))
            found.add(name)
if found != set(sys.argv[2:]):
    sys.exit("missing build artifacts: " + str(set(sys.argv[2:]) - found))
' "$@"
}
"${build_cli[@]}" --message-format=json | collect "$out/bare" bmux
"${build_cli[@]}" --features bundled-plugin-windows,bundled-plugin-pane-runtime,bundled-plugin-sessions,bundled-plugin-contexts,bundled-plugin-clients --message-format=json | collect "$out/local-splits" bmux
cargo build --locked --offline -p bmux_pane_runtime_plugin --bin bmux-pane-execution-supervisor --message-format=json | collect "$out/local-splits" bmux-pane-execution-supervisor
"${build_cli[@]}" --features bundled-plugin-performance --message-format=json | collect "$out/diagnostics" bmux
python3 scripts/cli-bootstrap-boundary/pty-input.py --diagnostics "$out/diagnostics/bmux" "$out/bare/bmux" "$out/local-splits/bmux"
