const CHANNELS = ["stable", "nightly"];
const CHANNEL_SET = new Set(CHANNELS);
const STATIC_PATH = /^\/(stable|nightly)\/(artifacts|apt|rpm)\/.+/;

function pickChannel(url) {
  const channel = url.searchParams.get("channel") || "stable";
  return CHANNEL_SET.has(channel) ? channel : "stable";
}

function fallbackVersion(channel, env) {
  return channel === "nightly" ? env.NIGHTLY_VERSION : env.STABLE_VERSION;
}

function staticContentType(pathname) {
  if (pathname.endsWith(".json")) return "application/json; charset=utf-8";
  if (pathname.endsWith(".txt")) return "text/plain; charset=utf-8";
  if (pathname.endsWith(".deb")) return "application/vnd.debian.binary-package";
  if (pathname.endsWith(".rpm")) return "application/x-rpm";
  if (pathname.endsWith(".gz")) return "application/gzip";
  if (pathname.endsWith(".gpg") || pathname.endsWith(".asc")) {
    return "application/pgp-signature";
  }
  return "application/octet-stream";
}

async function fetchJsonObject(env, key) {
  const object = await env.PACKAGES_BUCKET.get(key);
  if (!object) {
    return null;
  }

  try {
    return await object.json();
  } catch {
    return null;
  }
}

async function resolveChannelMetadata(host, channel, env) {
  const manifest = await fetchJsonObject(env, `${channel}/latest.json`);
  if (manifest?.version) {
    return {
      channel,
      version: manifest.version,
      artifacts_base:
        manifest.artifacts_base ??
        `https://${host}/${channel}/artifacts/${manifest.version}`,
      apt_base: manifest.apt_base ?? `https://${host}/${channel}/apt`,
      rpm_base: manifest.rpm_base ?? `https://${host}/${channel}/rpm`,
      updated_at: manifest.updated_at ?? null,
    };
  }

  const version = fallbackVersion(channel, env);
  return {
    channel,
    version,
    artifacts_base: `https://${host}/${channel}/artifacts/${version}`,
    apt_base: `https://${host}/${channel}/apt`,
    rpm_base: `https://${host}/${channel}/rpm`,
    updated_at: null,
  };
}

function installScript(host, channel, version) {
  if (!/^[A-Za-z0-9.-]+$/.test(host) || !CHANNEL_SET.has(channel) ||
      !/^[A-Za-z0-9][A-Za-z0-9_.+-]*$/.test(version)) {
    throw new Error("Unsafe install metadata");
  }
  return `#!/usr/bin/env sh
set -eu

CHANNEL="${channel}"
VERSION="${version}"
HOST="${host}"

OS="$(uname -s | tr '[:upper:]' '[:lower:]')"
ARCH="$(uname -m)"

case "$ARCH" in
  x86_64|amd64) ARCH="x86_64" ;;
  aarch64|arm64) ARCH="aarch64" ;;
esac

TARGET=""
case "$OS" in
  darwin)
    if [ "$ARCH" = "x86_64" ]; then TARGET="x86_64-apple-darwin"; fi
    if [ "$ARCH" = "aarch64" ]; then TARGET="aarch64-apple-darwin"; fi
    ;;
  linux)
    if [ "$ARCH" = "x86_64" ]; then TARGET="x86_64-unknown-linux-gnu"; fi
    if [ "$ARCH" = "aarch64" ]; then TARGET="aarch64-unknown-linux-gnu"; fi
    ;;
esac

if [ -z "$TARGET" ]; then
  echo "Unsupported platform: $OS/$ARCH"
  exit 1
fi

FILE="bmux-$VERSION-$TARGET.tar.gz"
URL="https://$HOST/$CHANNEL/artifacts/$VERSION/$FILE"

umask 077
TMPDIR="$(mktemp -d)"
trap 'rm -rf "$TMPDIR"' EXIT
trap 'exit 1' HUP INT TERM

fail() { echo "$*" >&2; exit 1; }
sha256() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | awk '{print $1}'
  elif command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$1" | awk '{print $1}'
  else
    fail 'A SHA-256 utility (sha256sum or shasum) is required'
  fi
}

echo "Downloading $URL"
curl --proto '=https' --proto-redir '=https' --tlsv1.2 -fsSL --max-filesize 1073741824 "$URL" -o "$TMPDIR/bmux.tar.gz"
curl --proto '=https' --proto-redir '=https' --tlsv1.2 -fsSL --max-filesize 1048576 "https://$HOST/$CHANNEL/artifacts/$VERSION/SHA256SUMS" -o "$TMPDIR/SHA256SUMS"
EXPECTED="$(awk -v file="$FILE" '
  NF == 2 { name = $2; sub(/^\\*/, "", name); if (name == file) { count++; hash = $1 } }
  END { if (count != 1 || length(hash) != 64 || hash ~ /[^0-9a-f]/) exit 1; print hash }
' "$TMPDIR/SHA256SUMS")" || fail 'Missing or ambiguous archive checksum'
ACTUAL="$(sha256 "$TMPDIR/bmux.tar.gz")"
[ "$ACTUAL" = "$EXPECTED" ] || fail 'Archive checksum mismatch'

# Only inspect the authenticated archive. Reject all non-flat names and duplicate
# members before extraction; no links, special files, or nested paths are allowed.
tar -tzf "$TMPDIR/bmux.tar.gz" > "$TMPDIR/members"
awk '
  $0 == "." || $0 == "./" { if (root++) exit 1; next }
  { name = $0; sub(/^\\.\\//, "", name)
    if (name !~ /^[A-Za-z0-9][A-Za-z0-9_.-]*$/ || seen[name]++ || ++count > 128) exit 1 }
  END { if (!seen["artifacts.json"] || !seen["bmux"] || !seen["bmux-install"]) exit 1 }
' "$TMPDIR/members" || fail 'Unsafe, duplicate, or legacy archive member'
# GNU and BSD tar verbose listings differ in owner columns. Both identify type
# in column one; size is column three (GNU) or five (BSD, numeric link count).
LC_ALL=C tar -tvzf "$TMPDIR/bmux.tar.gz" > "$TMPDIR/types"
awk '
  substr($1, 1, 1) == "d" { if ($NF != "." && $NF != "./") exit 1; next }
  { if (substr($1, 1, 1) != "-") exit 1
    size = ($2 ~ /^[0-9]+$/) ? $5 : $3
    if (size !~ /^[0-9]+$/ || size > 1073741824 || (total += size) > 2147483648) exit 1 }
' "$TMPDIR/types" || fail 'Unsafe archive type or size'
mkdir "$TMPDIR/verified"
tar -xzf "$TMPDIR/bmux.tar.gz" -C "$TMPDIR/verified"
PREFIX="\${BMUX_INSTALL_PREFIX:-/usr/local}"
# This release-target executable is built from the repository-owned Rust xtask,
# authenticated by the archive checksum; it validates the complete inventory.
chmod 700 "$TMPDIR/verified/bmux-install"
"$TMPDIR/verified/bmux-install" install "$TMPDIR/verified" --prefix "$PREFIX" --version "$VERSION-$TARGET"
echo "Installed verified BMUX version at $PREFIX/lib/bmux/versions/$VERSION-$TARGET"
echo "Activated $PREFIX/bin/bmux; prior versions remain intact"
`;
}

async function serveStatic(pathname, env) {
  const key = pathname.replace(/^\//, "");
  const object = await env.PACKAGES_BUCKET.get(key);
  if (!object) {
    return new Response("Not found", { status: 404 });
  }

  const headers = new Headers();
  headers.set("content-type", staticContentType(pathname));
  headers.set("cache-control", "public, max-age=300");
  const etag = object.httpEtag;
  if (etag) {
    headers.set("etag", etag);
  }

  return new Response(object.body, { headers });
}

export { installScript };

export default {
  async fetch(request, env) {
    const url = new URL(request.url);
    const host = env.PACKAGES_HOST || url.host;

    if (STATIC_PATH.test(url.pathname)) {
      return serveStatic(url.pathname, env);
    }

    if (url.pathname === "/channels.json") {
      const stable = await resolveChannelMetadata(host, "stable", env);
      const nightly = await resolveChannelMetadata(host, "nightly", env);
      return Response.json(
        { stable, nightly },
        {
          headers: {
            "cache-control": "no-store",
          },
        },
      );
    }

    if (url.pathname === "/install") {
      const channel = pickChannel(url);
      const metadata = await resolveChannelMetadata(host, channel, env);
      const script = installScript(host, channel, metadata.version);
      return new Response(script, {
        headers: {
          "content-type": "text/x-shellscript; charset=utf-8",
          "cache-control": "no-store",
        },
      });
    }

    if (url.pathname === "/" || url.pathname === "") {
      const channel = pickChannel(url);
      return new Response(
        `bmux package endpoint\n\n- channel: ${channel}\n- install: https://${host}/install?channel=${channel}\n- metadata: https://${host}/channels.json\n`,
        { headers: { "content-type": "text/plain; charset=utf-8" } },
      );
    }

    return new Response("Not found", { status: 404 });
  },
};
