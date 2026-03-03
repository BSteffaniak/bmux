#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
WORK_DIR="$(mktemp -d)"
RESULTS_FILE="$WORK_DIR/results.tsv"
HOME_DIR="$WORK_DIR/home"
CONFIG_FILE=""

cleanup() {
  if [[ "${BMUX_KEEP_COMPAT_TMP:-0}" == "1" ]]; then
    echo "keeping compatibility temp dir: $WORK_DIR"
    return
  fi
  rm -rf "$WORK_DIR"
}
trap cleanup EXIT

mkdir -p "$HOME_DIR"
export HOME="$HOME_DIR"
export XDG_CONFIG_HOME="$HOME_DIR/.config"
export XDG_DATA_HOME="$HOME_DIR/.local/share"
export XDG_RUNTIME_DIR="$WORK_DIR/runtime"
mkdir -p "$XDG_CONFIG_HOME/bmux" "$XDG_DATA_HOME/bmux" "$XDG_RUNTIME_DIR"

if [[ "$(uname -s)" == "Darwin" ]]; then
  CONFIG_FILE="$HOME_DIR/Library/Application Support/bmux/bmux.toml"
else
  CONFIG_FILE="$XDG_CONFIG_HOME/bmux/bmux.toml"
fi
mkdir -p "$(dirname "$CONFIG_FILE")"

write_config() {
  local pane_term="$1"
  cat >"$CONFIG_FILE" <<EOF
[general]
scrollback_limit = 10000

[behavior]
pane_term = "$pane_term"
restore_last_layout = false
EOF
}

make_shell_wrapper() {
  local scenario="$1"
  local shell_bin="$2"
  local wrapper="$WORK_DIR/${scenario}.sh"

  case "$scenario" in
    fish)
      cat >"$wrapper" <<EOF
#!/usr/bin/env bash
printf "__SCENARIO_fish__\\n"
exec "$shell_bin"
EOF
      ;;
    vim)
      cat >"$wrapper" <<EOF
#!/usr/bin/env bash
printf "__SCENARIO_vim__\\n"
"$shell_bin" -Nu NONE -n +qall!
EOF
      ;;
    fzf)
      cat >"$wrapper" <<EOF
#!/usr/bin/env bash
printf "__SCENARIO_fzf__\\n"
printf "alpha\\nbeta\\n" | "$shell_bin" --filter alpha >/dev/null
EOF
      ;;
    *)
      echo "unknown scenario '$scenario'" >&2
      return 1
      ;;
  esac

  chmod +x "$wrapper"
  printf "%s" "$wrapper"
}

run_case() {
  local scenario="$1"
  local shell_bin="$2"
  local profile_name="$3"
  local pane_term="$4"
  local wrapper
  local log_file="$WORK_DIR/${scenario}-${profile_name}.log"

  wrapper="$(make_shell_wrapper "$scenario" "$shell_bin")"
  write_config "$pane_term"

  local status="PASS"
  local notes="ok"

  if ! script -q "$log_file" sh -lc "(sleep 2; printf '\001q') | cargo run -q -p bmux_cli -- --shell '$wrapper' --no-alt-screen" >/dev/null 2>&1; then
    status="FAIL"
    notes="bmux command failed"
  fi

  if [[ "$status" == "PASS" && "$scenario" == "fish" ]]; then
    if rg -q "Primary Device Attribute query" "$log_file"; then
      status="FAIL"
      notes="fish DA warning detected"
    fi
  fi

  if [[ "$status" == "PASS" ]]; then
    if rg -q "failed to spawn shell in pane|failed reading pane PTY output|PTY output thread panicked" "$log_file"; then
      status="FAIL"
      notes="pane runtime error in log"
    fi
  fi

  printf "%s\t%s\t%s\t%s\n" "$scenario" "$profile_name" "$status" "$notes" >>"$RESULTS_FILE"
}

run_matrix_for_scenario() {
  local scenario="$1"
  local shell_bin="$2"

  run_case "$scenario" "$shell_bin" "bmux" "bmux-256color"
  run_case "$scenario" "$shell_bin" "xterm" "xterm-256color"
  run_case "$scenario" "$shell_bin" "screen" "screen-256color"
  run_case "$scenario" "$shell_bin" "conservative" "weird-term"
}

echo -e "scenario\tprofile\tstatus\tnotes" >"$RESULTS_FILE"

if command -v fish >/dev/null 2>&1; then
  run_matrix_for_scenario "fish" "$(command -v fish)"
else
  echo -e "fish\tall\tSKIP\tfish not installed" >>"$RESULTS_FILE"
fi

if command -v vim >/dev/null 2>&1; then
  run_matrix_for_scenario "vim" "$(command -v vim)"
else
  echo -e "vim\tall\tSKIP\tvim not installed" >>"$RESULTS_FILE"
fi

if command -v fzf >/dev/null 2>&1; then
  run_matrix_for_scenario "fzf" "$(command -v fzf)"
else
  echo -e "fzf\tall\tSKIP\tfzf not installed" >>"$RESULTS_FILE"
fi

printf "Compatibility Matrix\n"
printf "%-8s %-13s %-8s %s\n" "SCENARIO" "PROFILE" "STATUS" "NOTES"
printf "%-8s %-13s %-8s %s\n" "--------" "-------------" "------" "-----"
tail -n +2 "$RESULTS_FILE" | while IFS=$'\t' read -r scenario profile status notes; do
  printf "%-8s %-13s %-8s %s\n" "$scenario" "$profile" "$status" "$notes"
done

if rg -q $'\tFAIL\t' "$RESULTS_FILE"; then
  echo "compatibility matrix checks failed" >&2
  exit 1
fi

echo "compatibility matrix checks passed"
