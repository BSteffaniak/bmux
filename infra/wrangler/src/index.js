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

TMPDIR="$(mktemp -d)"
trap 'rm -rf "$TMPDIR"' EXIT

echo "Downloading $URL"
curl -fsSL "$URL" -o "$TMPDIR/bmux.tar.gz"
tar -xzf "$TMPDIR/bmux.tar.gz" -C "$TMPDIR"

BIN=""
if [ -f "$TMPDIR/bmux" ]; then
  BIN="$TMPDIR/bmux"
elif [ -f "$TMPDIR/bmux.exe" ]; then
  BIN="$TMPDIR/bmux.exe"
fi

if [ -z "$BIN" ]; then
  echo "bmux binary not found in archive"
  exit 1
fi

install -m 755 "$BIN" /usr/local/bin/bmux
echo "Installed bmux to /usr/local/bin/bmux"
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
