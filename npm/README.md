# @vyer/cli (npm)

Thin installer for **[Vyer](https://github.com/chirayuoli/vyer)** — the warm code-context
engine for AI coding agents (an MCP server).

```sh
npx @vyer/cli serve --root .     # run it (downloads the right prebuilt binary on first use)
npm install -g @vyer/cli         # or install globally
```

Wire it into your agent host (Claude Code / Cursor / Windsurf):

```jsonc
{ "mcpServers": { "vyer": { "command": "npx", "args": ["-y", "@vyer/cli", "serve", "--root", "."] } } }
```

On install, `install.js` downloads the `vyer` binary matching your platform and this
package's version from the project's GitHub Releases. If no prebuilt binary fits your
platform, install from source instead:

```sh
cargo install --git https://github.com/chirayuoli/vyer vyer-server
```

This package's version tracks the Vyer release it installs (npm `x.y.z` → git tag `vx.y.z`).
License: MIT OR Apache-2.0.
