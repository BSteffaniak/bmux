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

TMPDIR="$(mktemp -d)"
trap 'rm -rf "$TMPDIR"' EXIT

echo "Downloading $URL"
curl -fsSL "$URL" -o "$TMPDIR/bmux.tar.gz"
curl -fsSL "https://$HOST/$CHANNEL/artifacts/$VERSION/SHA256SUMS" -o "$TMPDIR/SHA256SUMS"
command -v python3 >/dev/null 2>&1 || { echo 'Python 3 is required for verified installation' >&2; exit 1; }
python3 - "$TMPDIR" "$FILE" "$TARGET" <<'PY'
import hashlib, json, pathlib, re, tarfile, sys
root, filename, target = pathlib.Path(sys.argv[1]), sys.argv[2], sys.argv[3]
checksums = []
for line in (root / 'SHA256SUMS').read_text().splitlines():
    fields = line.split()
    if len(fields) == 2 and fields[1].lstrip('*') == filename:
        checksums.append(fields[0])
if len(checksums) != 1 or not re.fullmatch('[0-9a-f]{64}', checksums[0]):
    raise SystemExit('Missing or ambiguous archive checksum')
archive = root / 'bmux.tar.gz'
if hashlib.sha256(archive.read_bytes()).hexdigest() != checksums[0]:
    raise SystemExit('Archive checksum mismatch')
output = root / 'verified'
output.mkdir(mode=0o700)
with tarfile.open(archive, 'r:gz') as tar:
    members = {}
    total = 0
    for member in tar:
        name = member.name
        if name in ('.', './') and member.isdir():
            continue
        if name.startswith('./'):
            name = name[2:]
        if not re.fullmatch('[A-Za-z0-9][A-Za-z0-9_.-]*', name) or not member.isfile():
            raise SystemExit('Unsafe archive member: ' + member.name)
        if name in members or member.size < 0 or member.size > 1024 * 1024 * 1024:
            raise SystemExit('Duplicate or oversized archive member')
        total += member.size
        if total > 2 * 1024 * 1024 * 1024 or len(members) >= 128:
            raise SystemExit('Archive budget exceeded')
        members[name] = member
    if 'artifacts.json' not in members:
        raise SystemExit('Legacy inventory-less archive refused: install manually; no auxiliary support is claimed')
    if members['artifacts.json'].size > 1024 * 1024:
        raise SystemExit('Inventory too large')
    inventory = json.load(tar.extractfile(members['artifacts.json']))
    if inventory.get('schema_version') != 1 or inventory.get('target') != target:
        raise SystemExit('Unsupported inventory schema or wrong target')
    expected = {'artifacts.json'}
    for item in inventory['artifacts']:
        name = item['relative_path']
        if name in expected or name not in members:
            raise SystemExit('Duplicate or missing inventory artifact')
        expected.add(name)
        member = members[name]
        if member.size != item['bytes']:
            raise SystemExit('Artifact length mismatch')
        data = tar.extractfile(member).read()
        if hashlib.sha256(data).hexdigest() != item['sha256']:
            raise SystemExit('Artifact hash mismatch: ' + name)
        (output / name).write_bytes(data)
    if set(members) != expected or not {'bmux', 'install-artifacts.py'}.issubset(expected):
        raise SystemExit('Archive inventory incomplete or contains unlisted files')
    (output / 'artifacts.json').write_text(json.dumps(inventory))
PY
PREFIX="\${BMUX_INSTALL_PREFIX:-/usr/local}"
python3 "$TMPDIR/verified/install-artifacts.py" "$TMPDIR/verified" "$PREFIX" --activate-version "$VERSION-$TARGET"
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
