#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
REPO_ROOT=$(cd -- "${SCRIPT_DIR}/.." && pwd)

VERSION_INPUT="${1:-v3.4.0}"
if [[ "${VERSION_INPUT}" == v* ]]; then
  VERSION="${VERSION_INPUT}"
else
  VERSION="v${VERSION_INPUT}"
fi

ASSET_NAME="JetBrainsMono.tar.xz"
ASSET_URL="https://github.com/ryanoasis/nerd-fonts/releases/download/${VERSION}/${ASSET_NAME}"

TMP_DIR=$(mktemp -d)
trap 'rm -rf "${TMP_DIR}"' EXIT

ARCHIVE_PATH="${TMP_DIR}/${ASSET_NAME}"
ASSET_DIR="${REPO_ROOT}/packages/fonts/assets/nerd-fonts"
DOC_PATH="${REPO_ROOT}/packages/fonts/THIRD_PARTY_FONTS.md"

FILES=(
  "JetBrainsMonoNerdFont-Regular.ttf"
  "JetBrainsMonoNerdFont-Bold.ttf"
  "JetBrainsMonoNerdFont-Italic.ttf"
  "JetBrainsMonoNerdFont-BoldItalic.ttf"
)

echo "Downloading ${ASSET_URL}"
curl -fL "${ASSET_URL}" -o "${ARCHIVE_PATH}"

echo "Extracting selected font files"
mkdir -p "${ASSET_DIR}"
tar -xf "${ARCHIVE_PATH}" -C "${TMP_DIR}" "${FILES[@]}"

for file in "${FILES[@]}"; do
  cp "${TMP_DIR}/${file}" "${ASSET_DIR}/${file}"
done

regular_hash="$(shasum -a 256 "${ASSET_DIR}/${FILES[0]}")"
regular_hash="${regular_hash%% *}"
bold_hash="$(shasum -a 256 "${ASSET_DIR}/${FILES[1]}")"
bold_hash="${bold_hash%% *}"
italic_hash="$(shasum -a 256 "${ASSET_DIR}/${FILES[2]}")"
italic_hash="${italic_hash%% *}"
bold_italic_hash="$(shasum -a 256 "${ASSET_DIR}/${FILES[3]}")"
bold_italic_hash="${bold_italic_hash%% *}"

cat > "${DOC_PATH}" <<EOF
# Third-party bundled fonts

This crate can optionally embed third-party font files for high-fidelity recording exports.

## Nerd Fonts JetBrains Mono preset

When bundled-nerd-fonts is enabled, the crate embeds:

- JetBrainsMonoNerdFont-Regular.ttf
- JetBrainsMonoNerdFont-Bold.ttf
- JetBrainsMonoNerdFont-Italic.ttf
- JetBrainsMonoNerdFont-BoldItalic.ttf

Source: Nerd Fonts release assets

- Repository: https://github.com/ryanoasis/nerd-fonts
- Release: ${VERSION}
- Asset: ${ASSET_NAME}

SHA-256 checksums:

- JetBrainsMonoNerdFont-Regular.ttf: ${regular_hash}
- JetBrainsMonoNerdFont-Bold.ttf: ${bold_hash}
- JetBrainsMonoNerdFont-Italic.ttf: ${italic_hash}
- JetBrainsMonoNerdFont-BoldItalic.ttf: ${bold_italic_hash}

Licenses and attribution are provided in:

- packages/fonts/licenses/NERD_FONTS_LICENSE
- packages/fonts/licenses/NERD_FONTS_LICENSE_AUDIT.md
- packages/fonts/licenses/JETBRAINS_MONO_OFL.txt
EOF

echo "Updated bundled Nerd fonts and ${DOC_PATH} for ${VERSION}"
