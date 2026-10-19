#!/usr/bin/env bash
# Exercise the gate with owned Git history and real Clippier; no product builds.
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
GATE="$ROOT/scripts/live-release-changes.sh"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
cd "$WORK"
git init -q
git config user.email test@example.invalid
git config user.name 'Release gate test'
mkdir -p packages/{shared,cli,unrelated} plugins/example packages/docs-site/ui/src docs
cat > Cargo.toml <<'TOML'
[workspace]
members = ["packages/*", "plugins/*"]
resolver = "2"
[workspace.dependencies]
shared = { path = "packages/shared" }
TOML
for entry in shared:shared cli:bmux_cli unrelated:unrelated; do
  directory="packages/${entry%%:*}"
  printf '[package]\nname = "%s"\nversion = "0.1.0"\nedition = "2021"\n' "${entry#*:}" > "$directory/Cargo.toml"
  mkdir -p "$directory/src"
  touch "$directory/src/lib.rs"
done
printf '\n[dependencies]\nshared = { workspace = true, optional = true }\n' >> packages/cli/Cargo.toml
printf '[package]\nname = "example_plugin"\nversion = "0.1.0"\nedition = "2021"\n' > plugins/example/Cargo.toml
mkdir -p plugins/example/src
touch plugins/example/src/lib.rs
# Keep the fake docs registry outside a Cargo member.
sed -i 's@"packages/\*"@"packages/shared", "packages/cli", "packages/unrelated"@' Cargo.toml
printf 'source: "docs/published.md"\n' > packages/docs-site/ui/src/doc_pages.rs
git add .; git commit -qm baseline
BASE="$(git rev-parse HEAD)"
export BASE GITHUB_EVENT_NAME=push GITHUB_OUTPUT="$WORK/output"
check() {
  local path="$1" expected_force="$2" expected_analyze="$3"
  mkdir -p "$(dirname "$path")"
  echo change >> "$path"
  git add .; git commit -qm change
  HEAD="$(git rev-parse HEAD)"
  export HEAD
  : > "$GITHUB_OUTPUT"
  bash "$GATE" inputs
  grep -qx "force=$expected_force" "$GITHUB_OUTPUT"
  grep -qx "analyze=$expected_analyze" "$GITHUB_OUTPUT"
  export BASE="$HEAD"
}
check docs/development.md false false
check docs/published.md true false
check README.md true false
check examples/demo.txt false false
check .github/workflows/unrelated.yml false false
check infra/live-terraform/main.tf true false
check scripts/build-site-artifact.sh true false
check scripts/live-release-changes.sh true false
check flake.lock true false
check Cargo.lock true false
check packages/unrelated/src/lib.rs false true
check packages/shared/src/lib.rs false true
check plugins/example/src/lib.rs false true
# Manual calls and unavailable history are explicit/conservative releases.
GITHUB_EVENT_NAME=workflow_dispatch bash "$GATE" inputs
tail -2 "$GITHUB_OUTPUT" | grep -qx force=true
BASE=0000000000000000000000000000000000000000 bash "$GATE" inputs
tail -2 "$GITHUB_OUTPUT" | grep -qx force=true
select_result() {
  : > "$GITHUB_OUTPUT"
  FORCE=false ANALYZE=true CLIPPIER_RESULT="$1" bash "$GATE" result
  grep -qx "relevant=$2" "$GITHUB_OUTPUT"
}
# Real Clippier must propagate changes through optional shared dependencies.
select_result "$(clippier affected-packages . --changed-files packages/shared/src/lib.rs --output json)" true
select_result "$(clippier affected-packages . --changed-files packages/unrelated/src/lib.rs --output json)" false
select_result "$(clippier affected-packages . --changed-files plugins/example/src/lib.rs --output json)" true
select_result '{"affected_packages":[{"name":"bmux-live-relay"}]}' true
select_result '{"affected_packages":[{"name":"bmux_web_renderer"}]}' true
select_result '{"affected_packages":[{"name":"bmux_docs_site"}]}' true
select_result '{"affected_packages":[]}' false
if FORCE=false ANALYZE=true CLIPPIER_RESULT='{}' bash "$GATE" result; then
  echo 'Malformed Clippier output was accepted' >&2; exit 1
fi
FORCE=true ANALYZE=false bash "$GATE" result
# These guards ensure unrelated pushes stop before every expensive entry job.
for job in build validation site-build; do
  block="$(sed -n "/^  $job:/,/^  [a-z-]*:/p" "$ROOT/.github/workflows/live-promote.yml" | head -4)"
  grep -q 'needs: changes' <<< "$block"
  grep -q "needs.changes.outputs.relevant == 'true'" <<< "$block"
done
grep -q 'git-strategy: auto' "$ROOT/.github/workflows/live-promote.yml"
if grep -Eq 'git-strategy: manual|git-base:|git-head:' "$ROOT/.github/workflows/live-promote.yml"; then
  echo 'Release must let Clippier select its own Git range' >&2; exit 1
fi
grep -Fq "BASE: \${{ steps.affected.outputs.git-base }}" "$ROOT/.github/workflows/live-promote.yml"
grep -Fq "HEAD: \${{ steps.affected.outputs.git-head }}" "$ROOT/.github/workflows/live-promote.yml"
echo 'Release relevance tests passed'
