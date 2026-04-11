"use strict";

const { platform, arch } = process;
const { execSync, spawn } = require("child_process");

function isMusl() {
  if (platform !== "linux") return false;
  try {
    const output = execSync("ldd --version 2>&1", {
      stdio: ["pipe", "pipe", "pipe"],
    }).toString();
    return output.includes("musl");
  } catch (err) {
    return Boolean(err.stderr && err.stderr.toString().includes("musl"));
  }
}

const PLATFORM_PACKAGES = {
  win32: { x64: "@bmux/win32-x64/bmux.exe" },
  darwin: {
    x64: "@bmux/darwin-x64/bmux",
    arm64: "@bmux/darwin-arm64/bmux",
  },
  linux: {
    x64: "@bmux/linux-x64/bmux",
    arm64: "@bmux/linux-arm64/bmux",
  },
  "linux-musl": { x64: "@bmux/linux-x64-musl/bmux" },
};

function getBinaryPath() {
  if (process.env.BMUX_BINARY_PATH) {
    return process.env.BMUX_BINARY_PATH;
  }

  const platformKey = platform === "linux" && isMusl() ? "linux-musl" : platform;
  const packagePath = PLATFORM_PACKAGES?.[platformKey]?.[arch];
  if (packagePath) {
    try {
      return require.resolve(packagePath);
    } catch {
      // continue
    }
  }

  try {
    const which = platform === "win32" ? "where" : "which";
    const resolved = execSync(`${which} bmux`, {
      stdio: ["pipe", "pipe", "pipe"],
    })
      .toString()
      .trim()
      .split("\n")[0];
    if (resolved) return resolved;
  } catch {
    // continue
  }

  throw new Error(`No prebuilt bmux binary available for ${platform}/${arch}.`);
}

const binaryPath = getBinaryPath();

function run(args, options) {
  return spawn(binaryPath, args, { stdio: "inherit", ...options });
}

module.exports = { binaryPath, run };
