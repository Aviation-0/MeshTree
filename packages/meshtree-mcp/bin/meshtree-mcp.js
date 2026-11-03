#!/usr/bin/env node
"use strict";
// `npx meshtree-mcp`: runs `meshtree mcp` over this process's stdio, so an MCP
// client config needs only `{"command": "npx", "args": ["-y", "meshtree-mcp"]}`
// (SPEC §15, §19). Extra arguments are passed through.

const { spawn } = require("node:child_process");
const { resolveBinary } = require("../lib/resolve.js");
const { version } = require("../package.json");

resolveBinary({ env: process.env, platform: process.platform, arch: process.arch, version })
  .then((bin) => {
    const child = spawn(bin, ["mcp", ...process.argv.slice(2)], { stdio: "inherit" });
    for (const signal of ["SIGINT", "SIGTERM", "SIGHUP"]) {
      process.on(signal, () => child.kill(signal));
    }
    child.on("error", (err) => {
      process.stderr.write(`meshtree-mcp: cannot start ${bin}: ${err.message}\n`);
      process.exit(1);
    });
    child.on("exit", (code, signal) => {
      process.exit(code === null ? (signal ? 1 : 0) : code);
    });
  })
  .catch((err) => {
    process.stderr.write(`meshtree-mcp: ${err.message}\n`);
    process.exit(1);
  });
