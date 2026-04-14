#!/usr/bin/env bash

set -euo pipefail

echo "Running sandbox bundle/verify regression checks..."

cargo test -p bmux_cli --test sandbox_integration sandbox_verify_bundle_reports_unexpected_artifacts_without_failing_by_default -- --exact
cargo test -p bmux_cli --test sandbox_integration sandbox_verify_bundle_strict_fails_on_unexpected_artifacts -- --exact
cargo test -p bmux_cli --test sandbox_integration sandbox_verify_bundle_fails_for_unsupported_bundle_version -- --exact
cargo test -p bmux_cli --test sandbox_integration sandbox_verify_bundle_detects_sha256_mismatch -- --exact
cargo test -p bmux_cli --test sandbox_integration sandbox_triage_bundle_generates_verified_bundle_json -- --exact
cargo test -p bmux_cli --test sandbox_integration sandbox_triage_bundle_strict_verify_sets_strict_mode -- --exact

echo "Sandbox bundle/verify regression checks passed"
