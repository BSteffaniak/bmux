#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BUILD_PROFILE="debug"
LIST_ONLY=0
ALL_WORKSPACE_PLUGINS=0
declare -a SELECTORS=()

usage() {
  cat <<'EOF'
Usage: ./scripts/rebuild-bundled-plugins.sh [options] [plugin-selector ...]

Rebuild bundled native plugins (auto-discovered), or selected plugin crates.

Selectors can be any of:
  - bundled plugin id (example: bmux.windows)
  - bundled short name (example: windows)
  - workspace plugin crate name (example: bmux_windows_plugin)

Options:
  --release                Build with release profile
  --all-workspace-plugins  Build all plugin cdylib crates under plugins/*
  --list                   Print discovered plugins and exit
  -h, --help               Show this help text
EOF
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --release)
      BUILD_PROFILE="release"
      ;;
    --all-workspace-plugins)
      ALL_WORKSPACE_PLUGINS=1
      ;;
    --list)
      LIST_ONLY=1
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    --)
      shift
      while [[ $# -gt 0 ]]; do
        SELECTORS+=("$1")
        shift
      done
      break
      ;;
    -*)
      printf 'unknown option: %s\n\n' "$1" >&2
      usage >&2
      exit 1
      ;;
    *)
      SELECTORS+=("$1")
      ;;
  esac
  shift
done

if ! command -v python3 >/dev/null 2>&1; then
  echo "error: python3 is required" >&2
  exit 1
fi

if ! command -v cargo >/dev/null 2>&1; then
  echo "error: cargo is required" >&2
  exit 1
fi

declare -A BUNDLED_ID_TO_CRATE=()
declare -A BUNDLED_SHORT_TO_CRATE=()
declare -A BUNDLED_CRATE_TO_ID=()
declare -A WORKSPACE_CRATE_EXISTS=()
declare -a BUNDLED_CRATES=()
declare -a WORKSPACE_CRATES=()

while IFS='|' read -r kind a b c; do
  case "$kind" in
    bundled)
      short="$a"
      plugin_id="$b"
      crate="$c"
      BUNDLED_ID_TO_CRATE["$plugin_id"]="$crate"
      BUNDLED_SHORT_TO_CRATE["$short"]="$crate"
      BUNDLED_CRATE_TO_ID["$crate"]="$plugin_id"
      BUNDLED_CRATES+=("$crate")
      ;;
    workspace)
      crate="$a"
      WORKSPACE_CRATE_EXISTS["$crate"]=1
      WORKSPACE_CRATES+=("$crate")
      ;;
  esac
done < <(
  python3 - "$ROOT_DIR" <<'PY'
import json
import pathlib
import subprocess
import sys
import tomllib

root = pathlib.Path(sys.argv[1])
bundled_root = root / "plugins" / "bundled"

meta = json.loads(
    subprocess.check_output(
        ["cargo", "metadata", "--no-deps", "--format-version", "1"],
        cwd=root,
        text=True,
    )
)

workspace_crates = set()
for package in meta["packages"]:
    package_name = package["name"]
    manifest_path = pathlib.Path(package["manifest_path"])
    if root / "plugins" not in manifest_path.parents:
        continue
    has_cdylib = any(
        "cdylib" in target.get("crate_types", [])
        for target in package.get("targets", [])
    )
    if has_cdylib:
        workspace_crates.add(package_name)

for crate in sorted(workspace_crates):
    print(f"workspace|{crate}||")

if bundled_root.exists():
    manifests = sorted(bundled_root.glob("*/plugin.toml"))
else:
    manifests = []

for manifest_path in manifests:
    with manifest_path.open("rb") as handle:
        manifest = tomllib.load(handle)

    short_name = manifest_path.parent.name
    plugin_id = str(manifest.get("id", "")).strip()
    entry = str(manifest.get("entry", "")).strip()

    if not plugin_id:
        raise SystemExit(f"invalid bundled manifest (missing id): {manifest_path}")
    if not entry:
        raise SystemExit(f"invalid bundled manifest (missing entry): {manifest_path}")

    crate = pathlib.Path(entry).name
    if crate.startswith("lib"):
        crate = crate[3:]
    if "." in crate:
        crate = crate.rsplit(".", 1)[0]

    print(f"bundled|{short_name}|{plugin_id}|{crate}")
PY
)

if [[ ${#BUNDLED_CRATES[@]} -eq 0 ]]; then
  echo "error: no bundled plugin manifests discovered under plugins/bundled" >&2
  exit 1
fi

for bundled_crate in "${BUNDLED_CRATES[@]}"; do
  if [[ -z "${WORKSPACE_CRATE_EXISTS[$bundled_crate]:-}" ]]; then
    printf 'error: bundled plugin crate not found in workspace: %s\n' "$bundled_crate" >&2
    exit 1
  fi
done

if [[ "$LIST_ONLY" == "1" ]]; then
  echo "bundled plugins:"
  for plugin_id in "${!BUNDLED_ID_TO_CRATE[@]}"; do
    crate="${BUNDLED_ID_TO_CRATE[$plugin_id]}"
    short=""
    for candidate_short in "${!BUNDLED_SHORT_TO_CRATE[@]}"; do
      if [[ "${BUNDLED_SHORT_TO_CRATE[$candidate_short]}" == "$crate" ]]; then
        short="$candidate_short"
        break
      fi
    done
    printf '  - %-18s short=%-12s crate=%s\n' "$plugin_id" "$short" "$crate"
  done
  echo "workspace plugin crates:"
  for crate in "${WORKSPACE_CRATES[@]}"; do
    printf '  - %s\n' "$crate"
  done
  exit 0
fi

declare -a TARGET_CRATES=()
declare -A TARGET_SEEN=()

add_target() {
  local crate="$1"
  if [[ -n "${TARGET_SEEN[$crate]:-}" ]]; then
    return
  fi
  TARGET_SEEN["$crate"]=1
  TARGET_CRATES+=("$crate")
}

resolve_selector() {
  local selector="$1"
  if [[ -n "${WORKSPACE_CRATE_EXISTS[$selector]:-}" ]]; then
    printf '%s\n' "$selector"
    return
  fi
  if [[ -n "${BUNDLED_ID_TO_CRATE[$selector]:-}" ]]; then
    printf '%s\n' "${BUNDLED_ID_TO_CRATE[$selector]}"
    return
  fi
  if [[ -n "${BUNDLED_SHORT_TO_CRATE[$selector]:-}" ]]; then
    printf '%s\n' "${BUNDLED_SHORT_TO_CRATE[$selector]}"
    return
  fi

  return 1
}

if [[ ${#SELECTORS[@]} -eq 0 ]]; then
  if [[ "$ALL_WORKSPACE_PLUGINS" == "1" ]]; then
    for crate in "${WORKSPACE_CRATES[@]}"; do
      add_target "$crate"
    done
  else
    for crate in "${BUNDLED_CRATES[@]}"; do
      add_target "$crate"
    done
  fi
else
  for selector in "${SELECTORS[@]}"; do
    if ! resolved="$(resolve_selector "$selector")"; then
      printf 'error: unknown plugin selector: %s\n' "$selector" >&2
      echo "known bundled ids:" >&2
      for plugin_id in "${!BUNDLED_ID_TO_CRATE[@]}"; do
        printf '  - %s\n' "$plugin_id" >&2
      done
      echo "known bundled short names:" >&2
      for short in "${!BUNDLED_SHORT_TO_CRATE[@]}"; do
        printf '  - %s\n' "$short" >&2
      done
      echo "known workspace plugin crates:" >&2
      for crate in "${WORKSPACE_CRATES[@]}"; do
        printf '  - %s\n' "$crate" >&2
      done
      exit 1
    fi
    add_target "$resolved"
  done
fi

if [[ ${#TARGET_CRATES[@]} -eq 0 ]]; then
  echo "error: no plugin crates selected to build" >&2
  exit 1
fi

declare -a CARGO_ARGS=(build)
if [[ "$BUILD_PROFILE" == "release" ]]; then
  CARGO_ARGS+=(--release)
fi
for crate in "${TARGET_CRATES[@]}"; do
  CARGO_ARGS+=(-p "$crate")
done

echo "building plugin crates (${BUILD_PROFILE}): ${TARGET_CRATES[*]}"
cd "$ROOT_DIR"
cargo "${CARGO_ARGS[@]}"
