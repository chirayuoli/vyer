# Vyer — productization & deployment plan

How to take Vyer from "works on my machine" to "thousands of agents install it." Ordered by what
unblocks adoption first.

---

## 0. Positioning (decide this first — everything else follows)

- **One-liner:** *The warm, local-first code-context engine for AI coding agents — find, understand, and
  edit code through one MCP tool, always fresh.*
- **ICP (who installs it):** developers using agentic coding hosts — **Claude Code, Cursor, Windsurf,
  Cline, Zed, Claude Desktop** — who feel their agent is slow/imprecise/expensive inside real repos.
- **The wedge:** *speed + freshness + safe edits + tiny footprint*, all **100% local**. Lead with that,
  not "semantic search" (saturated, and not the moat).
- **Proof points:** ~4 ms warm, staleness 0, 14 languages, one tool, no uploads, sandboxed writes,
  217 tests, CI-enforced SLOs.

## 1. Packaging — ship a single binary, many ways to get it

Vyer is a self-contained Rust binary. The install path **must** be frictionless because MCP users abandon
fast.

1. **Prebuilt binaries (the backbone).** GitHub Actions matrix build → static binaries for
   `macos-arm64`, `macos-x64`, `linux-x64`, `linux-arm64`, `windows-x64`. Attach to **GitHub Releases**.
   Everything below downloads these.
2. **npm wrapper — the priority channel.** Most Claude Code / Cursor users wire MCP via `npx`. Publish
   `@vyer/cli` whose `postinstall` downloads the right prebuilt binary (the esbuild/turbo/biome pattern).
   Then onboarding is literally:
   ```jsonc
   { "mcpServers": { "vyer": { "command": "npx", "args": ["-y", "@vyer/cli", "serve", "--root", "."] } } }
   ```
   Add `vyer init --global` to the postinstall (or print a one-line nudge to run it).
3. **Homebrew tap.** `brew install vyer-dev/tap/vyer`. Homebrew forbids auto-editing files → use a
   `caveats` message pointing to `vyer init`.
4. **Shell installer.** `curl -fsSL https://get.vyer.dev | sh` → detects platform, drops the binary on
   PATH, optionally runs `vyer init --global` (with a confirm prompt).
5. **`cargo install vyer`** for the Rust crowd (publish to crates.io).
6. **VS Code / JetBrains extension (later).** Bundles the binary + auto-registers the MCP server +
   runs `vyer init` on activation.

**CI/CD:** one GitHub Actions workflow = `scripts/gate.sh` on every PR (already your green gate) + a
release workflow (tag → matrix build → sign → publish to Releases + npm + crates.io + Homebrew tap).

## 2. Zero-friction onboarding

- **Auto-index the cwd**, stdio by default — no config to search. ✅ already true.
- **`vyer init`** to bootstrap `CLAUDE.md` so the agent *prefers* Vyer and *batches*. ✅ shipped.
- **Copy-paste config snippets** per host (Claude Code, Cursor, Windsurf, Cline, Claude Desktop) in the
  README + docs site.
- **First-run nudge (small, worth adding):** on `vyer serve`, if `CLAUDE.md` lacks the managed block,
  print one line: *"tip: run `vyer init` so your agent prefers Vyer."* Non-intrusive, surfaces the feature.
- **The `vyer://playbook` resource** teaches optimal usage in-band. ✅ already served.

## 3. Distribution & discovery (go-to-market)

- **List on every MCP directory:** the official `modelcontextprotocol/servers` registry, **Smithery**,
  **mcp.so**, **Glama**, the **Cursor** MCP directory, **Claude's connector directory**. These are where
  agent users browse for servers — being absent = invisible.
- **Launch posts:** Show HN ("Vyer — local, warm code context for coding agents"), a dev-Twitter/X thread
  with a 30-second screen capture (agent batch-editing across a monorepo at ~4 ms), r/LocalLLaMA,
  the Claude Code / Cursor communities and Discords.
- **A demo that sells in 30 s:** side-by-side — native tools dripping one file at a time vs. Vyer doing a
  batched, freshness-verified multi-file refactor. The *feel* is the pitch.
- **Docs site** (Mkdocs/Docusaurus/Astro): Quickstart, the playbook, security posture, benchmarks,
  per-host setup. Distill `docs/design-v4.md` into a public "How it works."

## 4. Trust & security as a *marketing* asset

Your security posture is a differentiator, not just hygiene — say it loudly:

- **Local-first, no uploads** → the answer to "I can't send my proprietary code to a code-search SaaS."
- **Sandboxed writes, no shell, typed params, audit log** → enterprise-reviewable.
- Publish a short **SECURITY.md** + threat model (you already have the §9 checklist + red-team tests).
- This is what lets you sell into companies that ban cloud code indexers.

## 5. Licensing & business model

- **License:** keep **MIT OR Apache-2.0** for the core. Open-source + local-first *is* the trust story and
  the adoption engine. Don't gate the core.
- **Open-core monetization (later, optional):**
  - **Team / shared index** — encrypted, opt-in, obfuscated (already designed as the "last" feature).
  - **Hosted/managed** for orgs (still local execution; central policy/config/telemetry).
  - **Priority support / SLAs** for enterprise.
  - The premium hooks (LSP graph, neural embeddings, team index) are already on the roadmap — they're the
    natural paid tier without crippling the free core.
- **Telemetry:** opt-in only, anonymous, clearly disclosed — anything else breaks the local-first promise.

## 6. Versioning & maintenance

- **SemVer**; turn `update.md` into a public **CHANGELOG.md**.
- **The gate is the contract** — `scripts/gate.sh` must stay green on every release (tests + clippy + fmt +
  smoke + **enforced SLOs**). This is your quality moat; advertise "CI-enforced latency SLOs."
- **Compatibility:** track the MCP spec version; pin the `rmcp` SDK; test against the top 2–3 hosts.

## 7. Metrics that matter

- Installs (npm downloads, Release asset downloads, brew installs).
- **MCP connections / active repos** (a privacy-safe local counter, opt-in).
- Retention (still connected after 1 / 4 weeks).
- Qualitative: directory ratings, GitHub stars, issue sentiment.

## 8. 30/60/90 sequencing

- **First (unblock install):** GitHub Releases + matrix CI · npm wrapper + `vyer init` postinstall · README
  + per-host snippets · list on MCP directories.
- **Then (polish & reach):** Homebrew tap + shell installer · docs site · SECURITY.md · launch posts +
  demo video · CHANGELOG.
- **Later (depth & money):** LSP graph · contextual embeddings (opt-in) · encrypted team index · IDE
  extensions · enterprise support.

---

**The single highest-leverage move:** the **npm wrapper + a one-line MCP config snippet + listing on the
MCP directories.** That's how an agent user goes from "never heard of it" to "connected" in under a minute —
and a connected agent that's been told (via `vyer init`) to prefer Vyer and batch is where all the value lands.
