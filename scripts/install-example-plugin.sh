#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PLUGIN_ID="example.native"
PLUGIN_DIR_NAME="example-native"
BUILD_PROFILE="debug"
FORCE=0

usage() {
  cat <<'EOF'
Usage: ./scripts/install-example-plugin.sh [--release] [--force] [--print-path]

Build the in-repo example native plugin and install a generated manifest into
the local bmux plugins directory.

Options:
  --release     Build the plugin in release mode
  --force       Replace an existing installed manifest
  --print-path  Print the installed plugin directory and exit
  -h, --help    Show this help text
EOF
}

data_home() {
  if [[ -n "${XDG_DATA_HOME:-}" ]]; then
    printf '%s\n' "$XDG_DATA_HOME"
    return
  fi

  case "$(uname -s)" in
    Darwin)
      printf '%s\n' "${HOME}/Library/Application Support"
      ;;
    *)
      printf '%s\n' "${HOME}/.local/share"
      ;;
  esac
}

library_filename() {
  case "$(uname -s)" in
    Darwin)
      printf '%s\n' "libbmux_example_native_plugin.dylib"
      ;;
    Linux)
      printf '%s\n' "libbmux_example_native_plugin.so"
      ;;
    MINGW*|MSYS*|CYGWIN*)
      printf '%s\n' "bmux_example_native_plugin.dll"
      ;;
    *)
      printf 'unsupported platform for native plugin install: %s\n' "$(uname -s)" >&2
      exit 1
      ;;
  esac
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --release)
      BUILD_PROFILE="release"
      ;;
    --force)
      FORCE=1
      ;;
    --print-path)
      printf '%s\n' "$(data_home)/bmux/plugins/${PLUGIN_DIR_NAME}"
      exit 0
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      printf 'unknown option: %s\n\n' "$1" >&2
      usage >&2
      exit 1
      ;;
  esac
  shift
done

PLUGIN_ROOT="$(data_home)/bmux/plugins/${PLUGIN_DIR_NAME}"
MANIFEST_PATH="${PLUGIN_ROOT}/plugin.toml"
TARGET_DIR="${ROOT_DIR}/target/${BUILD_PROFILE}"
LIB_PATH="${TARGET_DIR}/$(library_filename)"

cd "$ROOT_DIR"

if [[ "$BUILD_PROFILE" == "release" ]]; then
  cargo build --locked --release -p bmux_example_native_plugin
else
  cargo build --locked -p bmux_example_native_plugin
fi

mkdir -p "$PLUGIN_ROOT"

if [[ -e "$MANIFEST_PATH" && "$FORCE" != "1" ]]; then
  printf 'manifest already exists at %s (use --force to replace it)\n' "$MANIFEST_PATH" >&2
  exit 1
fi

cat > "$MANIFEST_PATH" <<EOF
id = "${PLUGIN_ID}"
name = "Example Native Plugin"
version = "0.0.1-alpha.0"
runtime = "native"
entry = "${LIB_PATH}"
required_host_scopes = ["bmux.commands", "bmux.events.subscribe", "bmux.permissions.read"]
provided_features = ["example.native"]

[[commands]]
name = "hello"
summary = "Print a hello message"
execution = "host_callback"

[[commands.arguments]]
name = "message"
kind = "string"
position = 0
multiple = true
trailing_var_arg = true
allow_hyphen_values = true
summary = "Optional greeting target"
value_name = "MESSAGE"

[[commands]]
name = "permissions-list"
summary = "List session permissions through bmux host IPC"
execution = "host_callback"

[[commands.arguments]]
name = "session"
kind = "string"
position = 0
required = true
summary = "Session name or UUID"
value_name = "SESSION"

[[event_subscriptions]]
kinds = ["system", "window"]
names = ["server_started", "window_created"]

[plugin_api]
minimum = "1.0"

[native_abi]
minimum = "1.0"
EOF

printf 'installed example plugin manifest at %s\n' "$MANIFEST_PATH"
printf 'plugin binary path: %s\n' "$LIB_PATH"
printf 'enable with: [plugins] enabled = ["%s"]\n' "$PLUGIN_ID"
