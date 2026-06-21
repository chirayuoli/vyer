# Releasing Vyer

How a version goes from a green tree to the three no-toolchain install paths
(prebuilt binary, `npx vyer`, `brew install`). Most of it is automated; the manual
steps are called out.

## What's wired up

- **`.github/workflows/ci.yml`** — fmt + clippy + `cargo test --workspace` on every push/PR.
- **`.github/workflows/release.yml`** — on a `v*` tag, cross-builds the `vyer` binary for
  macOS (arm64/x64), Linux (x64/arm64), and Windows (x64), and uploads each as
  `vyer-<tag>-<target>.(tar.gz|zip)` + a `.sha256` to a GitHub Release.
- **`npm/`** — the `vyer` npm package. `postinstall` downloads the release binary that
  matches the host + the package version; `bin/vyer.js` execs it (stdio inherited for MCP).
- **`packaging/homebrew/vyer.rb`** — the Homebrew formula (lives in a tap repo).

## Cut a release

1. **Bump versions** to the same number in:
   - the crate versions (`Cargo.toml` `[workspace.package] version`, if used, or per-crate),
   - `npm/package.json` `"version"`,
   - `packaging/homebrew/vyer.rb` `version`.
2. **Land it green:** `bash scripts/gate.sh` must pass; commit.
3. **Tag and push:**
   ```sh
   git tag v0.1.0
   git push origin v0.1.0
   ```
   The release workflow builds every platform and creates the GitHub Release with the
   archives + checksums. ✅ The "download a prebuilt binary" path is now live.

## Publish to npm (manual — needs your npm auth)

The package publishes as **`@vyer/cli`** — npm blocks the bare `vyer` as too similar to
existing names, so it's scoped. One-time: create the free `vyer` org at npmjs.com/org/create
(if that org name is taken, fall back to `@chirayuoli/vyer`).

```sh
cd npm
npm publish            # first time: `npm login`; `publishConfig.access=public` ships it public
```

Now `npx @vyer/cli serve --root .` works for everyone. (Tip: a `publish` job can be added to
`release.yml` with an `NPM_TOKEN` secret to automate this on tag.)

## Publish the Homebrew tap (manual, first time)

1. Create a repo `chirayuoli/homebrew-tap`.
2. Copy `packaging/homebrew/vyer.rb` to `Formula/vyer.rb` there.
3. Fill the four `sha256` values from the release's `*.sha256` assets.
4. `brew install chirayuoli/tap/vyer` works.

(This too can be automated: a release step that opens a PR to the tap with the new version +
checksums — the standard `brew bump-formula-pr` flow.)

## Result — the install matrix

| Path | Command | Needs |
|---|---|---|
| Prebuilt binary | download from Releases, `tar xzf`, run | nothing |
| npm | `npx @vyer/cli serve --root .` | Node |
| Homebrew | `brew install chirayuoli/tap/vyer` | brew |
| From source | `cargo install --git https://github.com/chirayuoli/vyer vyer-server` | Rust toolchain |
