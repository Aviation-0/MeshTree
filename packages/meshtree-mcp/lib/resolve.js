"use strict";
// Locates (or downloads) the `meshtree` binary for the npx shim (SPEC §15,
// §19, T-035). Order:
//
// 1. `MESHTREE_BIN`, if set.
// 2. `meshtree` on PATH.
// 3. The GitHub Release asset for this package's version, verified against its
//    published SHA-256 and cached under `MESHTREE_MCP_CACHE` (default
//    `~/.cache/meshtree-mcp/<version>/`).
//
// Nothing here writes to stdout: stdout is the MCP channel.

const { createHash } = require("node:crypto");
const fs = require("node:fs");
const https = require("node:https");
const os = require("node:os");
const path = require("node:path");
const { execFileSync } = require("node:child_process");

const REPO = "Aviation-0/MeshTree";

/** The release target triple and archive extension for a platform. */
function assetFor(platform, arch) {
  const key = `${platform}-${arch}`;
  const table = {
    "darwin-arm64": ["aarch64-apple-darwin", "tar.xz"],
    "linux-x64": ["x86_64-unknown-linux-gnu", "tar.xz"],
    "win32-x64": ["x86_64-pc-windows-msvc", "zip"],
  };
  const hit = table[key];
  if (!hit) {
    return null;
  }
  const [target, ext] = hit;
  return { target, file: `meshtree-${target}.${ext}` };
}

/** Download URL of a release asset. */
function assetUrl(version, file) {
  return `https://github.com/${REPO}/releases/download/v${version}/${file}`;
}

function binaryName(platform) {
  return platform === "win32" ? "meshtree.exe" : "meshtree";
}

/** `meshtree` on PATH, or null. */
function onPath(platform, envPath) {
  const exts = platform === "win32" ? [".exe", ".cmd", ""] : [""];
  for (const dir of (envPath || "").split(path.delimiter)) {
    if (!dir) continue;
    for (const ext of exts) {
      const candidate = path.join(dir, `meshtree${ext}`);
      try {
        if (fs.statSync(candidate).isFile()) return candidate;
      } catch (_) {
        // not here
      }
    }
  }
  return null;
}

function get(url, redirects = 5) {
  return new Promise((resolve, reject) => {
    https
      .get(url, { headers: { "User-Agent": "meshtree-mcp" } }, (res) => {
        if (res.statusCode >= 300 && res.statusCode < 400 && res.headers.location) {
          res.resume();
          if (redirects === 0) return reject(new Error(`too many redirects: ${url}`));
          return resolve(get(res.headers.location, redirects - 1));
        }
        if (res.statusCode !== 200) {
          res.resume();
          return reject(new Error(`GET ${url}: HTTP ${res.statusCode}`));
        }
        const chunks = [];
        res.on("data", (c) => chunks.push(c));
        res.on("end", () => resolve(Buffer.concat(chunks)));
        res.on("error", reject);
      })
      .on("error", reject);
  });
}

function findFile(dir, name) {
  for (const entry of fs.readdirSync(dir, { withFileTypes: true })) {
    const full = path.join(dir, entry.name);
    if (entry.isFile() && entry.name === name) return full;
    if (entry.isDirectory()) {
      const hit = findFile(full, name);
      if (hit) return hit;
    }
  }
  return null;
}

async function download(version, platform, arch, cacheRoot) {
  const asset = assetFor(platform, arch);
  if (!asset) {
    throw new Error(
      `no meshtree release for ${platform}-${arch}; install meshtree and put it on PATH, or set MESHTREE_BIN`
    );
  }
  const dir = path.join(cacheRoot, version);
  const cached = fs.existsSync(dir) ? findFile(dir, binaryName(platform)) : null;
  if (cached) return cached;

  const url = assetUrl(version, asset.file);
  process.stderr.write(`meshtree-mcp: downloading ${url}\n`);
  const [archive, checksum] = await Promise.all([get(url), get(`${url}.sha256`)]);
  const expected = checksum.toString("utf8").trim().split(/\s+/)[0].toLowerCase();
  const actual = createHash("sha256").update(archive).digest("hex");
  if (actual !== expected) {
    throw new Error(`checksum mismatch for ${asset.file}: expected ${expected}, got ${actual}`);
  }
  const staging = fs.mkdtempSync(path.join(os.tmpdir(), "meshtree-mcp-"));
  const archivePath = path.join(staging, asset.file);
  fs.writeFileSync(archivePath, archive);
  // bsdtar on Windows 10+ and macOS, GNU tar on Linux: all read .tar.xz, and
  // bsdtar reads .zip.
  execFileSync("tar", ["-xf", archivePath, "-C", staging], { stdio: "ignore" });
  fs.rmSync(archivePath);
  fs.mkdirSync(path.dirname(dir), { recursive: true });
  fs.renameSync(staging, dir);
  const bin = findFile(dir, binaryName(platform));
  if (!bin) throw new Error(`${asset.file} does not contain ${binaryName(platform)}`);
  if (platform !== "win32") fs.chmodSync(bin, 0o755);
  return bin;
}

/** Resolves the binary to launch. */
async function resolveBinary({ env, platform, arch, version }) {
  if (env.MESHTREE_BIN) return env.MESHTREE_BIN;
  const found = onPath(platform, env.PATH || env.Path);
  if (found) return found;
  const cacheRoot = env.MESHTREE_MCP_CACHE || path.join(os.homedir(), ".cache", "meshtree-mcp");
  return download(version, platform, arch, cacheRoot);
}

module.exports = { assetFor, assetUrl, onPath, resolveBinary };
