#!/usr/bin/env node
// Renders the Scoop and winget manifest templates in dist/ for one release
// (SPEC §19, T-035):
//
//   node dist/render-manifests.mjs <version> <sha256 of the Windows zip> <out-dir>
//
// writes <out-dir>/scoop/meshtree.json and <out-dir>/winget/*.yaml. The
// release-publish workflow pushes the Scoop manifest; the winget files are for
// the first manual `wingetcreate submit` (later releases use `wingetcreate update`).
import { mkdirSync, readFileSync, readdirSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const [version, sha256, outDir] = process.argv.slice(2);
if (!version || !sha256 || !outDir) {
  console.error("usage: render-manifests.mjs <version> <sha256> <out-dir>");
  process.exit(2);
}
if (!/^\d+\.\d+\.\d+([-+][0-9A-Za-z.-]+)?$/.test(version)) {
  console.error(`not a semver version: ${version}`);
  process.exit(2);
}
if (!/^[0-9a-fA-F]{64}$/.test(sha256)) {
  console.error(`not a SHA-256 hex digest: ${sha256}`);
  process.exit(2);
}

const here = dirname(fileURLToPath(import.meta.url));
const render = (text) =>
  text
    .replaceAll("{{VERSION}}", version)
    .replaceAll("{{SHA256_UPPER}}", sha256.toUpperCase())
    .replaceAll("{{SHA256}}", sha256.toLowerCase());

for (const kind of ["scoop", "winget"]) {
  mkdirSync(join(outDir, kind), { recursive: true });
  for (const name of readdirSync(join(here, kind)).sort()) {
    const out = render(readFileSync(join(here, kind, name), "utf8"));
    if (out.includes("{{")) {
      console.error(`unrendered placeholder in ${kind}/${name}`);
      process.exit(1);
    }
    writeFileSync(join(outDir, kind, name), out);
  }
}
