#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
WORK_DIR="$(mktemp -d)"
RESULTS_FILE="$WORK_DIR/results.tsv"
HOME_DIR="$WORK_DIR/home"
CONFIG_FILE=""
TRACE_LIMIT="${BMUX_COMPAT_TRACE_LIMIT:-500}"
BMUX_BIN="$ROOT_DIR/target/debug/bmux"

if ! command -v jq >/dev/null 2>&1; then
  echo "error: jq is required for compatibility assertions" >&2
  exit 1
fi

if ! cargo build -q -p bmux_cli >/dev/null 2>&1; then
  echo "error: failed to build bmux_cli before matrix run" >&2
  exit 1
fi

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
server_timeout = 5000

[behavior]
pane_term = "$pane_term"
restore_last_layout = false
protocol_trace_enabled = true
protocol_trace_capacity = 2000
EOF
}

expected_primary_da_for_profile() {
  local profile="$1"
  case "$profile" in
    bmux|xterm|conservative)
      printf '\033[?1;2c'
      ;;
    screen)
      printf '\033[?64;1;2;6;9;15;18;21;22c'
      ;;
    *)
      return 1
      ;;
  esac
}

expected_secondary_da_for_profile() {
  local profile="$1"
  case "$profile" in
    bmux)
      printf '\033[>84;0;0c'
      ;;
    xterm)
      printf '\033[>0;115;0c'
      ;;
    screen)
      printf '\033[>83;40003;0c'
      ;;
    conservative)
      printf '\033[>0;1000;0c'
      ;;
    *)
      return 1
      ;;
  esac
}

profile_for_effective_term() {
  local term="$1"
  case "$term" in
    bmux-256color)
      printf 'bmux'
      ;;
    xterm-256color)
      printf 'xterm'
      ;;
    screen-256color|tmux-256color)
      printf 'screen'
      ;;
    *)
      printf 'conservative'
      ;;
  esac
}

doctor_trace_json() {
  "$BMUX_BIN" terminal doctor --json --trace --trace-limit "$TRACE_LIMIT"
}

event_index() {
  local trace_json_file="$1"
  local name="$2"
  local direction="$3"
  jq -r --arg name "$name" --arg direction "$direction" '(.trace.events | to_entries | map(select(.value.name == $name and .value.direction == $direction)) | .[0].key) // -1' "$trace_json_file"
}

assert_query_reply_pair() {
  local trace_json_file="$1"
  local name="$2"
  local query_index
  local reply_index
  query_index="$(event_index "$trace_json_file" "$name" "query")"
  reply_index="$(event_index "$trace_json_file" "$name" "reply")"
  if [[ "$query_index" -lt 0 || "$reply_index" -lt 0 || "$query_index" -ge "$reply_index" ]]; then
    return 1
  fi
  return 0
}

make_shell_wrapper() {
  local scenario="$1"
  local shell_bin="$2"
  local wrapper="$WORK_DIR/${scenario}.sh"
  local flow_ok_file="$WORK_DIR/${scenario}.flow.ok"
  local vim_output_file="$WORK_DIR/${scenario}.vim.out"
  local fzf_output_file="$WORK_DIR/${scenario}.fzf.out"

  case "$scenario" in
    fish)
      cat >"$wrapper" <<EOF
#!/usr/bin/env bash
printf "__SCENARIO_fish__\\n"
# Emit an explicit DA probe so transcript assertions are deterministic
printf '\\033[c'
"$shell_bin" -ic 'echo __FISH_CMD_OK__' 2>/dev/null || true
printf "ok" >"$flow_ok_file"
exec "$shell_bin"
EOF
      ;;
    vim)
      cat >"$wrapper" <<EOF
#!/usr/bin/env bash
printf "__SCENARIO_vim__\\n"
printf '\\033[?25\$p\\033P\$qm\\033\\\\'
"$shell_bin" -Nu NONE -n -Es "$vim_output_file" -c "call setline(1, ['hello from bmux'])" -c "write!" -c "qall!" >/dev/null 2>&1 || true
if [[ -f "$vim_output_file" ]] && rg -q "hello from bmux" "$vim_output_file"; then
  printf "__VIM_WRITE_OK__\\n"
  printf "ok" >"$flow_ok_file"
fi
EOF
      ;;
    fzf)
      cat >"$wrapper" <<EOF
#!/usr/bin/env bash
printf "__SCENARIO_fzf__\\n"
printf '\\033]10;?\\033\\\\\\033P+q5443;636f\\033\\\\'
printf "alpha\\nbeta\\ngamma\\n" | "$shell_bin" --filter a >/dev/null 2>&1 || true
printf "alpha\\nbeta\\ngamma\\n" | "$shell_bin" --filter alpha >"$fzf_output_file" 2>/dev/null || true
if [[ -f "$fzf_output_file" ]] && rg -q "^alpha$" "$fzf_output_file"; then
  printf "__FZF_SELECT_OK__\\n"
  printf "ok" >"$flow_ok_file"
fi
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
  local trace_json_file="$WORK_DIR/${scenario}-${profile_name}.trace.json"
  local flow_ok_file="$WORK_DIR/${scenario}.flow.ok"
  local vim_output_file="$WORK_DIR/${scenario}.vim.out"
  local fzf_output_file="$WORK_DIR/${scenario}.fzf.out"

  wrapper="$(make_shell_wrapper "$scenario" "$shell_bin")"
  write_config "$pane_term"

  rm -f "$flow_ok_file" "$vim_output_file" "$fzf_output_file"

  local status="PASS"
  local notes="ok"

  if ! script -q "$log_file" sh -lc "(sleep 2; printf '\001q') | '$BMUX_BIN' --shell '$wrapper' --no-alt-screen" >/dev/null 2>&1; then
    status="FAIL"
    notes="bmux command failed"
  fi

  if [[ "$status" == "PASS" ]]; then
    if ! doctor_trace_json >"$trace_json_file"; then
      status="FAIL"
      notes="terminal doctor trace failed"
    fi
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

  if [[ "$status" == "PASS" ]]; then
    case "$scenario" in
      fish)
        if [[ ! -f "$flow_ok_file" ]]; then
          status="FAIL"
          notes="fish_command_flow_missing"
        fi
        ;;
      vim)
        if [[ ! -f "$flow_ok_file" ]] || [[ ! -f "$vim_output_file" ]]; then
          status="FAIL"
          notes="vim_edit_flow_missing"
        fi
        ;;
      fzf)
        if [[ ! -f "$flow_ok_file" ]] || [[ ! -f "$fzf_output_file" ]]; then
          status="FAIL"
          notes="fzf_filter_select_flow_missing"
        fi
        ;;
    esac
  fi

  if [[ "$status" == "PASS" ]]; then
    local protocol_profile
    local effective_term
    local expected_profile
    local primary_da
    local secondary_da
    local expected_primary
    local expected_secondary

    protocol_profile="$(jq -r '.protocol_profile // empty' "$trace_json_file")"
    effective_term="$(jq -r '.effective_pane_term // empty' "$trace_json_file")"
    primary_da="$(jq -r '.primary_da_reply // empty' "$trace_json_file")"
    secondary_da="$(jq -r '.secondary_da_reply // empty' "$trace_json_file")"

    expected_profile="$(profile_for_effective_term "$effective_term")"
    if [[ "$protocol_profile" != "$expected_profile" ]]; then
      status="FAIL"
      notes="bad_protocol_profile"
    fi

    if [[ "$status" == "PASS" ]]; then
      expected_primary="$(expected_primary_da_for_profile "$protocol_profile")"
      expected_secondary="$(expected_secondary_da_for_profile "$protocol_profile")"

      if [[ "$primary_da" != "$expected_primary" ]]; then
        status="FAIL"
        notes="bad_primary_da_reply"
      elif [[ "$secondary_da" != "$expected_secondary" ]]; then
        status="FAIL"
        notes="bad_secondary_da_reply"
      fi
    fi
  fi

  if [[ "$status" == "PASS" ]]; then
    case "$scenario" in
      fish)
        if ! assert_query_reply_pair "$trace_json_file" "csi_primary_da"; then
          status="FAIL"
          notes="fish_missing_primary_da_pair"
        fi
        ;;
      vim)
        if ! assert_query_reply_pair "$trace_json_file" "csi_dec_mode_report"; then
          status="FAIL"
          notes="vim_missing_dec_mode_pair"
        elif ! assert_query_reply_pair "$trace_json_file" "dcs_decrqss_query"; then
          status="FAIL"
          notes="vim_missing_decrqss_pair"
        fi
        ;;
      fzf)
        if ! assert_query_reply_pair "$trace_json_file" "osc_color_query"; then
          status="FAIL"
          notes="fzf_missing_osc_color_pair"
        elif ! assert_query_reply_pair "$trace_json_file" "dcs_xtgettcap_query"; then
          status="FAIL"
          notes="fzf_missing_xtgettcap_pair"
        fi
        ;;
    esac
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
