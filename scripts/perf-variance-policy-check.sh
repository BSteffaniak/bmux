#!/usr/bin/env bash
set -euo pipefail

REPORT_DIR=""
POLICY_FILE="docs/perf-baselines/variance-policy.json"
MODE=""

usage() {
	cat <<'USAGE'
Usage: ./scripts/perf-variance-policy-check.sh [options]

Evaluates compare-report JSON files against variance policy.

Options:
  --report-dir DIR    Directory with compare-report JSON files
  --policy-file PATH  Policy JSON (default: docs/perf-baselines/variance-policy.json)
  --mode MODE         Default mode override: warn or soft-fail
  -h, --help          Show this help message
USAGE
}

warn() {
	local message="$1"
	echo "warning: $message"
	if [[ -n "${GITHUB_ACTIONS:-}" ]]; then
		echo "::warning::$message"
	fi
}

parse_args() {
	while (($# > 0)); do
		case "$1" in
		--report-dir)
			REPORT_DIR="$2"
			shift 2
			;;
		--policy-file)
			POLICY_FILE="$2"
			shift 2
			;;
		--mode)
			MODE="$2"
			shift 2
			;;
		-h | --help)
			usage
			exit 0
			;;
		*)
			echo "unknown argument: $1" >&2
			usage
			exit 2
			;;
		esac
	done
}

is_mode_valid() {
	[[ "$1" == "warn" || "$1" == "soft-fail" ]]
}

has_active_allowlist_entry() {
	local scenario="$1"
	local metric="$2"
	local status="$3"
	local variance="$4"
	local today="$5"
	local match

	match="$(jq -r \
		--arg scenario "$scenario" \
		--arg metric "$metric" \
		--arg status "$status" \
		--arg variance "$variance" \
		--arg today "$today" \
		'
      [.allowlist[]?
       | select(.scenario == $scenario)
       | select(.metric == $metric)
       | select((.status // $status) == $status)
       | select((.variance // $variance) == $variance)
       | {expires_on: (.expires_on // ""), reason: (.reason // "no reason provided")}]
      | if length == 0 then empty else .[0] | "\(.expires_on)\t\(.reason)" end
    ' "$POLICY_FILE")"

	if [[ -z "$match" ]]; then
		return 1
	fi

	local expires_on reason
	expires_on="${match%%$'\t'*}"
	reason="${match#*$'\t'}"

	if [[ -n "$expires_on" && "$expires_on" < "$today" ]]; then
		warn "expired allowlist entry for scenario=$scenario metric=$metric (expired $expires_on): $reason"
		return 1
	fi

	warn "allowlist suppression for scenario=$scenario metric=$metric: $reason"
	return 0
}

parse_args "$@"

if [[ -z "$REPORT_DIR" ]]; then
	echo "--report-dir is required" >&2
	exit 2
fi
if [[ ! -d "$REPORT_DIR" ]]; then
	echo "report directory does not exist: $REPORT_DIR" >&2
	exit 2
fi
if [[ ! -f "$POLICY_FILE" ]]; then
	echo "policy file does not exist: $POLICY_FILE" >&2
	exit 2
fi
if ! command -v jq >/dev/null 2>&1; then
	echo "jq is required for policy check" >&2
	exit 2
fi

default_mode="$(jq -r '.mode // "warn"' "$POLICY_FILE")"
if [[ -n "$MODE" ]]; then
	default_mode="$MODE"
fi
if ! is_mode_valid "$default_mode"; then
	echo "invalid mode: $default_mode (expected warn or soft-fail)" >&2
	exit 2
fi

today="$(date -u +%Y-%m-%d)"
echo "variance policy check: report_dir=$REPORT_DIR policy=$POLICY_FILE default_mode=$default_mode"

# Surface expired allowlist entries even if no matching reports appear.
jq -r --arg today "$today" '
  .allowlist[]?
  | select((.expires_on // "") != "")
  | select(.expires_on < $today)
  | "\(.scenario)\t\(.metric)\t\(.expires_on)\t\(.reason // "no reason provided")"
' "$POLICY_FILE" | while IFS=$'\t' read -r scenario metric expires_on reason; do
	[[ -z "$scenario" ]] && continue
	warn "expired allowlist entry scenario=$scenario metric=$metric expired=$expires_on: $reason"
done

matches=0
hard_fail_matches=0
shopt -s nullglob
for report in "$REPORT_DIR"/*.json; do
	name="$(basename "$report")"
	scenario="${name%.json}"

	report_matches="$(jq -r \
		--slurpfile policy "$POLICY_FILE" \
		--arg scenario "$scenario" \
		--arg default_mode "$default_mode" \
		'
      ($policy[0]) as $p
      | ($p.defaults // {}) as $defaults
      | ($p.scenarios[$scenario] // {}) as $scenario_policy
      | ($scenario_policy.mode // $default_mode) as $mode
      | ($scenario_policy.min_runs // $defaults.min_runs // 2) as $min_runs
      | ($scenario_policy.enforced_metrics // $defaults.enforced_metrics // []) as $enforced_metrics
      | ($scenario_policy.regression_statuses // $defaults.regression_statuses // []) as $regression_statuses
      | ($scenario_policy.regression_variance // $defaults.regression_variance // []) as $regression_variance
      | (.metrics // [])[]?
      | (.label // "") as $label
      | (.status // "") as $status
      | (.variance // "") as $variance
      | (.runs // 0) as $runs
      | select($runs >= $min_runs)
      | select(($enforced_metrics | index($label)) != null)
      | select(($regression_statuses | index($status)) != null)
      | select(($regression_variance | index($variance)) != null)
      | "\($label)\t\($status)\t\($variance)\t\($runs)\t\($mode)\t\($min_runs)\t\(.delta_median // 0)\t\(.delta_max // 0)"
    ' "$report")"

	if [[ -z "$report_matches" ]]; then
		continue
	fi

	while IFS=$'\t' read -r metric status variance runs mode min_runs delta_median delta_max; do
		[[ -z "$metric" ]] && continue
		if ! is_mode_valid "$mode"; then
			warn "invalid scenario mode '$mode' for $scenario; falling back to warn"
			mode="warn"
		fi

		if has_active_allowlist_entry "$scenario" "$metric" "$status" "$variance" "$today"; then
			continue
		fi

		matches=$((matches + 1))
		warn "variance policy match in $name: metric=$metric status=$status variance=$variance runs=$runs min_runs=$min_runs delta_median=$delta_median delta_max=$delta_max mode=$mode"
		if [[ "$mode" == "soft-fail" ]]; then
			hard_fail_matches=$((hard_fail_matches + 1))
		fi
	done <<<"$report_matches"
done

if ((matches == 0)); then
	echo "variance policy check: no regression matches"
	exit 0
fi

if ((hard_fail_matches > 0)); then
	echo "variance policy check: $hard_fail_matches soft-fail match(es), failing" >&2
	exit 1
fi

echo "variance policy check: $matches match(es), warn-only outcome"
