#!/usr/bin/env node
"use strict";
// postinstall: download the prebuilt `vyer` binary that matches this package's
// version and the host platform from GitHub Releases, into ./bin. The bin/vyer.js
// shim then execs it. If anything fails, we point the user at the from-source path
// instead of leaving a broken install.

const fs = require("fs");
const path = require("path");
const https = require("https");
const { execFileSync } = require("child_process");

const REPO = "chirayuoli/vyer";
const VERSION = require("./package.json").version;

// node platform+arch  ->  Rust target triple (must match release.yml's matrix)
const TARGETS = {
  "darwin arm64": "aarch64-apple-darwin",
  "darwin x64": "x86_64-apple-darwin",
  "linux x64": "x86_64-unknown-linux-gnu",
  "linux arm64": "aarch64-unknown-linux-gnu",
  "win32 x64": "x86_64-pc-windows-msvc",
};

function bail(msg) {
  console.error(`\n[vyer] ${msg}`);
  console.error(
    "[vyer] Install from source instead:\n" +
      "       cargo install --git https://github.com/chirayuoli/vyer vyer-server\n"
  );
  process.exit(1);
}

const key = `${process.platform} ${process.arch}`;
const target = TARGETS[key];
if (!target) bail(`no prebuilt binary for ${key}.`);

const isWin = process.platform === "win32";
const ext = isWin ? "zip" : "tar.gz";
const asset = `vyer-v${VERSION}-${target}.${ext}`;
const url = `https://github.com/${REPO}/releases/download/v${VERSION}/${asset}`;
const binDir = path.join(__dirname, "bin");
const binName = isWin ? "vyer.exe" : "vyer";

function download(u, dest, cb, redirects) {
  redirects = redirects || 0;
  if (redirects > 10) return cb(new Error("too many redirects"));
  https
    .get(u, { headers: { "User-Agent": "vyer-installer" } }, (res) => {
      if (res.statusCode >= 300 && res.statusCode < 400 && res.headers.location) {
        res.resume();
        return download(res.headers.location, dest, cb, redirects + 1);
      }
      if (res.statusCode !== 200) {
        res.resume();
        return cb(new Error(`HTTP ${res.statusCode} for ${u}`));
      }
      const file = fs.createWriteStream(dest);
      res.pipe(file);
      file.on("finish", () => file.close(() => cb(null)));
      file.on("error", cb);
    })
    .on("error", cb);
}

fs.mkdirSync(binDir, { recursive: true });
const archivePath = path.join(binDir, asset);
console.log(`[vyer] downloading ${asset} ...`);

download(url, archivePath, (err) => {
  if (err) bail(`download failed: ${err.message}`);
  try {
    if (isWin) {
      execFileSync(
        "powershell",
        [
          "-NoProfile",
          "-Command",
          `Expand-Archive -Force -Path "${archivePath}" -DestinationPath "${binDir}"`,
        ],
        { stdio: "inherit" }
      );
    } else {
      execFileSync("tar", ["-xzf", archivePath, "-C", binDir], { stdio: "inherit" });
    }
    fs.unlinkSync(archivePath);
    const binPath = path.join(binDir, binName);
    if (!fs.existsSync(binPath)) bail(`${binName} missing after extraction.`);
    if (!isWin) fs.chmodSync(binPath, 0o755);
    console.log(`[vyer] installed ${binName} (v${VERSION})`);
  } catch (e) {
    bail(`extraction failed: ${e.message}`);
  }
});
