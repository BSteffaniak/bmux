#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

run_missing_plugin_scenario() {
	echo
	echo "[scenario 1] missing plugin invocation"
	set +e
	local output
	output="$(bmux plugin run missing.plugin-id no-op 2>&1)"
	local status=$?
	set -e

	if [[ $status -eq 0 ]]; then
		echo "scenario 1 expected non-zero exit" >&2
		exit 1
	fi
	if [[ "$output" != *"missing.plugin-id"* ]]; then
		echo "scenario 1 missing plugin id in output" >&2
		exit 1
	fi
	if [[ "$output" != *"bmux plugin list --json"* ]]; then
		echo "scenario 1 missing expected list guidance" >&2
		exit 1
	fi
	echo "scenario 1 pass"
}

run_policy_denial_contract_scenario() {
	echo
	echo "[scenario 2] policy denial guidance contract"
	cargo test -q -p bmux_plugin_cli_plugin run_cmd::tests::format_plugin_command_run_error_adds_policy_hint_when_denied
	echo "scenario 2 pass"
}

run_perf_regression_drill() {
	echo
	echo "[scenario 3] perf threshold regression drill"
	set +e
	./scripts/perf-plugin-command-latency.sh \
		--iterations 1 \
		--warmup 0 \
		--max-p95-ms 1 \
		--max-p99-ms 1 \
		-- plugin list --json >/tmp/bmux-plugin-game-day-perf.txt 2>&1
	local status=$?
	set -e

	if [[ $status -eq 0 ]]; then
		echo "scenario 3 expected failure from impossible thresholds" >&2
		exit 1
	fi

	local perf_output
	perf_output="$(cat /tmp/bmux-plugin-game-day-perf.txt)"
	if [[ "$perf_output" != *"SLO check failed"* ]]; then
		echo "scenario 3 missing perf failure marker" >&2
		exit 1
	fi
	echo "scenario 3 pass"
}

main() {
	cd "$ROOT_DIR"
	run_missing_plugin_scenario
	run_policy_denial_contract_scenario
	run_perf_regression_drill
	echo
	echo "plugin game day complete"
}

main "$@"
