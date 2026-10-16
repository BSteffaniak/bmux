#!/usr/bin/env bash
# Build the complete Pages site without native plugins or backend deployment.
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
: "${BMUX_NIX_TOOLCHAIN:?Use bash scripts/dev.sh bash scripts/build-site-artifact.sh OUTPUT}"
OUTPUT="${1:?New output directory required}"
[[ ! -e "$OUTPUT" ]] || { echo 'Output already exists' >&2; exit 2; }
mkdir -p "$OUTPUT"
OUTPUT="$(cd "$OUTPUT" && pwd)"
# Stage the content-addressed viewer bundle BEFORE docs compilation.
BMUX_WEB_ASSET_OUTPUT="$OUTPUT/viewer" bash scripts/build-web-renderer.sh
cargo run --locked --release -p bmux_docs_site -- gen --output "$OUTPUT/site"
# HyperChad's static generator currently hardcodes its upstream demo metadata,
# ignoring AppBuilder's title/description (the dynamic renderer honors them).
# Normalize that known generator fallback at the static artifact boundary only.
python3 - "$OUTPUT/site" <<'PY'
from pathlib import Path
import sys
for page in Path(sys.argv[1]).rglob('*.html'):
    text = page.read_text()
    text = text.replace('<title>MoosicBox</title>', '<title>bmux — Live broadcasts and documentation</title>')
    text = text.replace('content="MoosicBox: A music app for cows"',
                        'content="bmux terminal multiplexer documentation and live broadcasts"')
    page.write_text(text)
PY
mkdir "$OUTPUT/tools"
cp infra/live-terraform/tools/package*.json "$OUTPUT/tools/"
(cd "$OUTPUT/tools" && npm ci --ignore-scripts --no-audit --no-fund)
git rev-parse HEAD > "$OUTPUT/REVISION"
# shellcheck disable=SC2094
(cd "$OUTPUT" && find . -type f ! -name SHA256SUMS -print0 | sort -z | xargs -0 sha256sum > SHA256SUMS)
echo "Immutable site candidate: $OUTPUT"
