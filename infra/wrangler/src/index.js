const CHANNELS = new Set(["stable", "nightly"]);

function pickChannel(url) {
  const channel = url.searchParams.get("channel") || "stable";
  return CHANNELS.has(channel) ? channel : "stable";
}

function versionFor(channel, env) {
  return channel === "nightly" ? env.NIGHTLY_VERSION : env.STABLE_VERSION;
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

BIN="$(find "$TMPDIR" -type f -name bmux | head -n 1)"
if [ -z "$BIN" ]; then
  echo "bmux binary not found in archive"
  exit 1
fi

install -m 755 "$BIN" /usr/local/bin/bmux
echo "Installed bmux to /usr/local/bin/bmux"
`;
}

function channelsManifest(host, env) {
  return {
    stable: {
      version: env.STABLE_VERSION,
      artifacts_base: `https://${host}/stable/artifacts/${env.STABLE_VERSION}`,
      apt_base: `https://${host}/stable/apt`,
      rpm_base: `https://${host}/stable/rpm`,
    },
    nightly: {
      version: env.NIGHTLY_VERSION,
      artifacts_base: `https://${host}/nightly/artifacts/${env.NIGHTLY_VERSION}`,
      apt_base: `https://${host}/nightly/apt`,
      rpm_base: `https://${host}/nightly/rpm`,
    },
  };
}

export default {
  async fetch(request, env) {
    const url = new URL(request.url);
    const host = env.PACKAGES_HOST || url.host;

    if (url.pathname === "/channels.json") {
      return Response.json(channelsManifest(host, env));
    }

    if (url.pathname === "/install") {
      const channel = pickChannel(url);
      const version = versionFor(channel, env);
      const script = installScript(host, channel, version);
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
