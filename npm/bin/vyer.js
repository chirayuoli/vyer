#!/usr/bin/env node
"use strict";
// Thin launcher: exec the real `vyer` binary downloaded by install.js, forwarding
// argv and stdio (MCP speaks over stdio, so `inherit` is essential) and the exit code.

const path = require("path");
const { spawnSync } = require("child_process");

const binName = process.platform === "win32" ? "vyer.exe" : "vyer";
const bin = path.join(__dirname, binName);

const res = spawnSync(bin, process.argv.slice(2), { stdio: "inherit" });
if (res.error) {
  console.error(
    `[vyer] failed to launch the binary (${res.error.message}). ` +
      "Try reinstalling, or build from source: cargo install --git https://github.com/chirayuoli/vyer vyer-server"
  );
  process.exit(1);
}
process.exit(res.status === null ? 1 : res.status);
