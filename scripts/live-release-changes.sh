#!/usr/bin/env bash
# Clippier owns dependency analysis. This only handles non-Cargo release inputs
# and selects our artifact roots from its affected-packages JSON.
set -euo pipefail
case "${1:?inputs or result required}" in
  inputs)
    force=false
    analyze=false
    # Manual/configure runs are explicit requests. Missing push history must not
    # silently drop a release; full checkout normally makes both SHAs available.
    if [[ "${GITHUB_EVENT_NAME:?}" != push ]] ||
       ! git cat-file -e "${BASE:-}^{commit}" 2>/dev/null ||
       ! git cat-file -e "${HEAD:-}^{commit}" 2>/dev/null; then
      force=true
    else
      changed="$(mktemp)"
      trap 'rm -f "$changed"' EXIT
      git diff --no-renames --name-only -z "$BASE" "$HEAD" > "$changed"
      while IFS= read -r -d '' path; do
        case "$path" in
          Cargo.toml|Cargo.lock|*/Cargo.toml|flake.nix|flake.lock|.envrc|rust-toolchain*|.cargo/*|nix/*|clippier.toml)
            force=true ;;
          infra/live-*|.github/workflows/live-*.yml|.github/workflows/docs-deploy.yml|.github/workflows/configure-production-infra.yml)
            force=true ;;
          scripts/*live*|scripts/*web*|scripts/*presenter*|scripts/build-site-artifact.sh|scripts/smoke-pty-runtime.sh|scripts/compat-matrix.sh|scripts/check-workspace-dependencies.sh|scripts/test-compat-pty.py)
            force=true ;;
          README.md) force=true ;;
          docs/*)
            # The site registry is authoritative: development-only docs don't
            # require publishing. Check both revisions for removals/renames.
            for revision in "$BASE" "$HEAD"; do
              if git show "$revision:packages/docs-site/ui/src/doc_pages.rs" | grep -F "\"$path\"" > /dev/null; then
                force=true
              fi
            done ;;
          packages/*|plugins/*) analyze=true ;;
        esac
      done < "$changed"
    fi
    printf 'force=%s\nanalyze=%s\n' "$force" "$analyze" >> "$GITHUB_OUTPUT"
    ;;
  result)
    relevant="${FORCE:?}"
    if [[ "$relevant" != true && "${ANALYZE:?}" == true ]]; then
      # Missing/malformed analysis fails the job, never silently skips release.
      jq -e '.affected_packages | type == "array"' <<< "$CLIPPIER_RESULT" > /dev/null
      relevant="$(jq -r 'any(.affected_packages[]; .name == "bmux_cli" or
        .name == "bmux-live-relay" or .name == "bmux_docs_site" or
        .name == "bmux_web_renderer" or (.name | contains("_plugin")))' <<< "$CLIPPIER_RESULT")"
    fi
    printf 'relevant=%s\n' "$relevant" >> "$GITHUB_OUTPUT"
    echo "Release inputs affected: $relevant"
    ;;
  *) exit 2 ;;
esac
