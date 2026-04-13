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
  --mode MODE         Override mode: warn or soft-fail
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

policy_mode="$(jq -r '.mode // "warn"' "$POLICY_FILE")"
if [[ -n "$MODE" ]]; then
	policy_mode="$MODE"
fi
if [[ "$policy_mode" != "warn" && "$policy_mode" != "soft-fail" ]]; then
	echo "invalid mode: $policy_mode (expected warn or soft-fail)" >&2
	exit 2
fi

echo "variance policy check: report_dir=$REPORT_DIR policy=$POLICY_FILE mode=$policy_mode"

violations=0
shopt -s nullglob
for report in "$REPORT_DIR"/*.json; do
	name="$(basename "$report")"
	matches="$(jq -r --slurpfile policy "$POLICY_FILE" '
    .metrics[]?
    | select(
        (.label as $label | ($policy[0].enforced_metrics | index($label) != null))
        and (.status as $status | ($policy[0].regression_statuses | index($status) != null))
        and (.variance as $variance | ($policy[0].regression_variance | index($variance) != null))
      )
    | "\(.label) status=\(.status) variance=\(.variance) delta_median=\(.delta_median) delta_max=\(.delta_max)"
  ' "$report")"

	if [[ -n "$matches" ]]; then
		while IFS= read -r line; do
			[[ -z "$line" ]] && continue
			warn "variance policy match in $name: $line"
			violations=$((violations + 1))
		done <<<"$matches"
	fi
done

if ((violations == 0)); then
	echo "variance policy check: no regression matches"
	exit 0
fi

if [[ "$policy_mode" == "soft-fail" ]]; then
	echo "variance policy check: $violations match(es), failing in soft-fail mode" >&2
	exit 1
fi

echo "variance policy check: $violations match(es), warn-only mode"
