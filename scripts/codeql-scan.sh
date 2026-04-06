#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DB_DIR="$ROOT_DIR/.codeql/db"
RESULTS_DIR="$ROOT_DIR/.codeql/results"
CONFIG_FILE="$ROOT_DIR/.github/codeql/codeql-config.yml"

LANGUAGES=()
CLEAN=0
JSON_OUTPUT=0

usage() {
  cat <<EOF
Usage: $(basename "$0") [OPTIONS]

Run CodeQL security analysis on the bmux codebase.

Options:
  --rust        Scan Rust code only
  --actions     Scan GitHub Actions workflows only
  --all         Scan all languages (default)
  --clean       Force-recreate databases even if they exist
  --json        Output raw SARIF JSON instead of human-readable summary
  -h, --help    Show this help message
EOF
  exit 0
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --rust)     LANGUAGES+=(rust); shift ;;
    --actions)  LANGUAGES+=(actions); shift ;;
    --all)      LANGUAGES=(rust actions); shift ;;
    --clean)    CLEAN=1; shift ;;
    --json)     JSON_OUTPUT=1; shift ;;
    -h|--help)  usage ;;
    *)          echo "Unknown option: $1"; usage ;;
  esac
done

# Default to all languages if none specified.
if [[ ${#LANGUAGES[@]} -eq 0 ]]; then
  LANGUAGES=(rust actions)
fi

# Verify codeql is installed.
if ! command -v codeql &>/dev/null; then
  echo "error: codeql is not installed"
  echo "  Install via Homebrew:  brew install codeql"
  echo "  Or enter nix shell:    nix develop"
  exit 1
fi

# Verify jq is installed (needed for result formatting).
if [[ "$JSON_OUTPUT" -eq 0 ]] && ! command -v jq &>/dev/null; then
  echo "error: jq is not installed (required for human-readable output)"
  echo "  Install via Homebrew:  brew install jq"
  echo "  Or use --json flag for raw SARIF output"
  exit 1
fi

mkdir -p "$DB_DIR" "$RESULTS_DIR"

# Read query suite name from the project CodeQL config if it exists,
# otherwise default to security-extended.
SUITE_NAME="security-extended"
if [[ -f "$CONFIG_FILE" ]]; then
  configured=$(grep 'uses:' "$CONFIG_FILE" | head -1 | sed 's/.*uses:[[:space:]]*//' | tr -d '[:space:]')
  if [[ -n "$configured" ]]; then
    SUITE_NAME="$configured"
  fi
fi

TOTAL_FINDINGS=0
declare -A LANG_FINDINGS=()

echo ""
echo "=== CodeQL Security Scan ==="

for lang in "${LANGUAGES[@]}"; do
  db_path="$DB_DIR/$lang"
  sarif_file="$RESULTS_DIR/$lang.sarif"

  echo ""
  echo "--- ${lang} ---"

  # Create or reuse database.
  if [[ -d "$db_path" && "$CLEAN" -eq 0 ]]; then
    echo "Reusing existing database (use --clean to recreate)"
  else
    rm -rf "$db_path"
    echo -n "Creating database... "
    start_time=$SECONDS
    codeql database create "$db_path" \
      --language="$lang" \
      --source-root="$ROOT_DIR" \
      --overwrite \
      --verbosity=progress 2>&1
    elapsed=$(( SECONDS - start_time ))
    echo "done (${elapsed}s)"
  fi

  # Build analyze command.
  analyze_args=(
    "$db_path"
    --format=sarif-latest
    --output="$sarif_file"
    --verbosity=progress
  )

  # Construct the query suite path for this language from the config suite name.
  analyze_args+=("codeql/${lang}-queries:codeql-suites/${lang}-${SUITE_NAME}.qls")

  echo -n "Analyzing with security-extended... "
  start_time=$SECONDS
  codeql database analyze "${analyze_args[@]}" 2>&1
  elapsed=$(( SECONDS - start_time ))
  echo "done (${elapsed}s)"

  # Process results.
  if [[ "$JSON_OUTPUT" -eq 1 ]]; then
    cat "$sarif_file"
    continue
  fi

  count=$(jq '.runs[].results | length' "$sarif_file")
  LANG_FINDINGS[$lang]=$count
  TOTAL_FINDINGS=$(( TOTAL_FINDINGS + count ))

  if [[ "$count" -eq 0 ]]; then
    echo ""
    echo "0 findings."
  else
    echo ""
    echo "$count finding(s):"
    echo ""
    jq -r '.runs[].results[] |
      "  \(.locations[0].physicalLocation.artifactLocation.uri):\(.locations[0].physicalLocation.region.startLine) - [\(.ruleId)]\n    \(.message.text)\n"
    ' "$sarif_file"
  fi
done

if [[ "$JSON_OUTPUT" -eq 0 ]]; then
  echo ""
  parts=()
  for lang in "${LANGUAGES[@]}"; do
    parts+=("${LANG_FINDINGS[$lang]:-0} ${lang}")
  done
  summary=$(IFS=', '; echo "${parts[*]}")
  echo "=== Summary: $TOTAL_FINDINGS finding(s) ($summary) ==="
  echo ""
  echo "SARIF results saved to $RESULTS_DIR/"

  if [[ "$TOTAL_FINDINGS" -gt 0 ]]; then
    exit 1
  fi
fi
