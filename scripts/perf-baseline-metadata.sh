#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
MODE=""
BASELINE_DIR=""
PROFILE=""
ITERATIONS=""
WARMUP=""
MAX_AGE_DAYS="30"

usage() {
	cat <<'USAGE'
Usage: ./scripts/perf-baseline-metadata.sh <write|check> [options]

Commands:
  write  Write baseline metadata JSON for a baseline directory.
  check  Validate baseline metadata presence/schema/staleness and emit warnings.

Options:
  --baseline-dir DIR   Baseline directory containing JSON artifacts

Write options:
  --profile NAME       Profile used to generate baseline (for example: command-latency, runtime-matrix, runtime-matrix-scale-medium)
  --iterations N       Iteration count used for generation
  --warmup N           Warmup count used for generation

Check options:
  --max-age-days N     Warn when metadata age exceeds N days (default: 30)

Examples:
  ./scripts/perf-baseline-metadata.sh write --baseline-dir docs/perf-baselines --profile command-latency --iterations 3 --warmup 1
  ./scripts/perf-baseline-metadata.sh check --baseline-dir docs/perf-baselines --max-age-days 30
USAGE
}

warn() {
	local message="$1"
	echo "warning: $message"
	if [[ -n "${GITHUB_ACTIONS:-}" ]]; then
		echo "::warning::$message"
	fi
}

require_number() {
	local value="$1"
	local name="$2"
	if ! [[ "$value" =~ ^[0-9]+$ ]]; then
		echo "${name} must be a non-negative integer" >&2
		exit 2
	fi
}

parse_args() {
	MODE="${1:-}"
	if [[ -z "$MODE" ]]; then
		usage
		exit 2
	fi
	shift

	while (($# > 0)); do
		case "$1" in
		--baseline-dir)
			BASELINE_DIR="$2"
			shift 2
			;;
		--profile)
			PROFILE="$2"
			shift 2
			;;
		--iterations)
			ITERATIONS="$2"
			shift 2
			;;
		--warmup)
			WARMUP="$2"
			shift 2
			;;
		--max-age-days)
			MAX_AGE_DAYS="$2"
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

write_metadata() {
	if [[ -z "$BASELINE_DIR" || -z "$PROFILE" || -z "$ITERATIONS" || -z "$WARMUP" ]]; then
		echo "write requires --baseline-dir, --profile, --iterations, and --warmup" >&2
		exit 2
	fi
	require_number "$ITERATIONS" "--iterations"
	require_number "$WARMUP" "--warmup"

	mkdir -p "$BASELINE_DIR"

	local generated_at
	local generated_epoch
	local git_sha
	local git_branch
	local metadata_path
	generated_at="$(date -u +"%Y-%m-%dT%H:%M:%SZ")"
	generated_epoch="$(date -u +%s)"
	git_sha="$(git -C "$ROOT_DIR" rev-parse --short HEAD 2>/dev/null || echo unknown)"
	git_branch="$(git -C "$ROOT_DIR" rev-parse --abbrev-ref HEAD 2>/dev/null || echo unknown)"
	metadata_path="$BASELINE_DIR/baseline-metadata.json"

	cat >"$metadata_path" <<EOF
{
  "generated_at": "$generated_at",
  "generated_at_epoch": $generated_epoch,
  "generated_by": "scripts/perf-baseline-metadata.sh",
  "git": {
    "sha": "$git_sha",
    "branch": "$git_branch"
  },
  "environment": {
    "os": "$(uname -s)",
    "arch": "$(uname -m)"
  },
  "sampling": {
    "profile": "$PROFILE",
    "iterations": $ITERATIONS,
    "warmup": $WARMUP
  }
}
EOF

	echo "wrote baseline metadata: $metadata_path"
}

check_metadata() {
	if [[ -z "$BASELINE_DIR" ]]; then
		echo "check requires --baseline-dir" >&2
		exit 2
	fi
	require_number "$MAX_AGE_DAYS" "--max-age-days"

	local metadata_path
	metadata_path="$BASELINE_DIR/baseline-metadata.json"
	if [[ ! -f "$metadata_path" ]]; then
		warn "missing baseline metadata: $metadata_path"
		return 0
	fi

	if ! command -v jq >/dev/null 2>&1; then
		warn "jq is required for metadata checks but was not found"
		return 0
	fi

	local missing_fields
	missing_fields="$(jq -r '
    [
      (if .generated_at == null then "generated_at" else empty end),
      (if .generated_at_epoch == null then "generated_at_epoch" else empty end),
      (if .sampling.profile == null then "sampling.profile" else empty end),
      (if .sampling.iterations == null then "sampling.iterations" else empty end),
      (if .sampling.warmup == null then "sampling.warmup" else empty end)
    ] | join(",")
  ' "$metadata_path")"
	if [[ -n "$missing_fields" ]]; then
		warn "metadata file missing required field(s): $missing_fields ($metadata_path)"
	fi

	local generated_epoch
	generated_epoch="$(jq -r '.generated_at_epoch // empty' "$metadata_path")"
	if [[ -z "$generated_epoch" || ! "$generated_epoch" =~ ^[0-9]+$ ]]; then
		warn "metadata generated_at_epoch is missing or invalid ($metadata_path)"
		return 0
	fi

	local now_epoch
	local age_days
	now_epoch="$(date -u +%s)"
	age_days="$(((now_epoch - generated_epoch) / 86400))"
	if ((age_days > MAX_AGE_DAYS)); then
		warn "baseline metadata is stale (${age_days} days old > ${MAX_AGE_DAYS}): $metadata_path"
	else
		echo "baseline metadata fresh (${age_days} days): $metadata_path"
	fi
}

main() {
	parse_args "$@"
	case "$MODE" in
	write)
		write_metadata
		;;
	check)
		check_metadata
		;;
	*)
		echo "unknown mode: $MODE" >&2
		usage
		exit 2
		;;
	esac
}

main "$@"
