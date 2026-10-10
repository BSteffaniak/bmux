import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { mkdtempSync, readFileSync, writeFileSync, mkdirSync, rmSync, existsSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { spawnSync } from "node:child_process";
import { gzipSync } from "node:zlib";
import test from "node:test";

// Import the Worker without adding package metadata or requiring a deployment.
const source = readFileSync(new URL("../src/index.js", import.meta.url), "utf8");
const { installScript } = await import(`data:text/javascript;base64,${Buffer.from(source).toString("base64")}`);
const target = "aarch64-apple-darwin";
const filename = `bmux-1.2.3-${target}.tar.gz`;

function archive(entries) {
  const blocks = [];
  for (const { name, data = "", type = "0", link = "" } of entries) {
    const body = Buffer.from(data);
    const header = Buffer.alloc(512);
    header.write(name, 0, 100, "ascii");
    for (const [offset, length, value] of [[100, 8, 0o755], [108, 8, 0], [116, 8, 0], [124, 12, body.length], [136, 12, 0]]) {
      header.write(value.toString(8).padStart(length - 1, "0") + "\0", offset, length, "ascii");
    }
    header.fill(32, 148, 156);
    header.write(type, 156, 1, "ascii");
    header.write(link, 157, 100, "ascii");
    header.write("ustar\0", 257, 6, "ascii");
    header.write("00", 263, 2, "ascii");
    const sum = header.reduce((a, b) => a + b, 0);
    header.write(sum.toString(8).padStart(6, "0") + "\0 ", 148, 8, "ascii");
    blocks.push(header, body, Buffer.alloc((512 - body.length % 512) % 512));
  }
  return gzipSync(Buffer.concat([...blocks, Buffer.alloc(1024)]));
}

const payload = [
  { name: "./", type: "5" },
  { name: "./artifacts.json", data: "{}" },
  { name: "./bmux", data: "fixture CLI" },
  // Test double only: production must execute the repository-owned Rust binary.
  { name: "./bmux-install", data: '#!/bin/sh\nprintf "%s\\n" "$@" > "$FIXTURE/invoked"\nexit "${INSTALL_EXIT:-0}"\n' },
];

function run(entries = payload, { checksum = "valid", tar = "tar", exit = "0" } = {}) {
  const root = mkdtempSync(join(tmpdir(), "bmux-install-test-"));
  try {
    mkdirSync(join(root, "bin"));
    const bytes = archive(entries);
    writeFileSync(join(root, "archive"), bytes);
    const hash = createHash("sha256").update(bytes).digest("hex");
    const line = `${checksum === "wrong" ? "0".repeat(64) : hash}  ${filename}\n`;
    writeFileSync(join(root, "checksums"), checksum === "duplicate" ? line + line : checksum === "missing" ? "" : line);
    writeFileSync(join(root, "bin", "curl"), `#!/bin/sh\nfor arg do case "$arg" in https:*/SHA256SUMS) input=checksums ;; https:*tar.gz) input=archive ;; esac; done\nwhile [ "$1" != "-o" ]; do shift; done\ncp "$FIXTURE/$input" "$2"\n`, { mode: 0o755 });
    writeFileSync(join(root, "bin", "uname"), '#!/bin/sh\ncase "$1" in -s) echo Darwin ;; -m) echo arm64 ;; esac\n', { mode: 0o755 });
    const tarPath = spawnSync("sh", ["-c", `command -v ${tar}`], { encoding: "utf8" }).stdout.trim();
    writeFileSync(join(root, "bin", "tar"), `#!/bin/sh\necho "$*" >> "$FIXTURE/tar-log"\nexec "${tarPath}" "$@"\n`, { mode: 0o755 });
    writeFileSync(join(root, "install.sh"), installScript("packages.bmux.dev", "stable", "1.2.3"));
    const result = spawnSync("sh", [join(root, "install.sh")], {
      encoding: "utf8",
      env: { ...process.env, PATH: `${join(root, "bin")}:${process.env.PATH}`, FIXTURE: root, BMUX_INSTALL_PREFIX: join(root, "prefix"), INSTALL_EXIT: exit },
      timeout: 10000,
    });
    return { ...result, invoked: existsSync(join(root, "invoked")) ? readFileSync(join(root, "invoked"), "utf8") : null, tarLog: existsSync(join(root, "tar-log")) ? readFileSync(join(root, "tar-log"), "utf8") : "" };
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
}

test("metadata rejects shell interpolation", () => {
  for (const args of [["bad;host", "stable", "1"], ["host", "bad", "1"], ["host", "stable", "$(id)"]]) {
    assert.throws(() => installScript(...args), /Unsafe/);
  }
  assert.doesNotMatch(installScript("host", "stable", "1"), /python|jq/);
});

test("authenticated flat archive invokes installer with explicit immutable activation", () => {
  const result = run();
  assert.equal(result.status, 0, result.stderr);
  assert.match(result.invoked, /^install\n.*\/verified\n--prefix\n.*\/prefix\n--version\n1\.2\.3-aarch64-apple-darwin\n$/);
  assert.match(result.tarLog, /-xzf/);
});

for (const checksum of ["wrong", "duplicate", "missing"]) {
  test(`rejects ${checksum} checksum before even inspecting tar`, () => {
    const result = run(payload, { checksum });
    assert.notEqual(result.status, 0);
    assert.equal(result.invoked, null);
    assert.equal(result.tarLog, "");
  });
}

for (const entry of [
  { name: "../escape" }, { name: "/absolute" }, { name: "nested/file" },
  { name: "bmux", type: "2", link: "/etc/passwd" },
  { name: "hard", type: "1", link: "bmux" }, { name: "fifo", type: "6" },
  { name: "./bmux" }, { name: "space name" },
]) {
  test(`rejects unsafe member ${entry.name} type ${entry.type ?? "0"} before extraction`, () => {
    const result = run([...payload, entry]);
    assert.notEqual(result.status, 0, result.stderr);
    assert.equal(result.invoked, null);
    assert.doesNotMatch(result.tarLog, /-xzf/);
  });
}

test("rejects oversized member inventories before extraction", () => {
  const result = run([...payload, ...Array.from({ length: 129 }, (_, i) => ({ name: `artifact-${i}` }))]);
  assert.notEqual(result.status, 0);
  assert.equal(result.invoked, null);
  assert.doesNotMatch(result.tarLog, /-xzf/);
});

test("rejects a linked installer before extraction", () => {
  const result = run(payload.map((entry) => entry.name === "./bmux-install" ? { name: entry.name, type: "2", link: "/usr/bin/false" } : entry));
  assert.notEqual(result.status, 0);
  assert.equal(result.invoked, null);
  assert.doesNotMatch(result.tarLog, /-xzf/);
});

test("legacy archives and installer failures never report success", () => {
  const legacy = run(payload.filter(({ name }) => name !== "./artifacts.json"));
  assert.notEqual(legacy.status, 0);
  assert.equal(legacy.invoked, null);
  const failed = run(payload, { exit: "7" });
  assert.equal(failed.status, 7);
  assert.doesNotMatch(failed.stdout, /Installed verified/);
});

if (process.platform === "darwin") {
  test("BSD tar listing is accepted on macOS", () => {
    const result = run(payload, { tar: "/usr/bin/tar" });
    assert.equal(result.status, 0, result.stderr);
  });
}
