# Vyer vs Claude Code native tools — benchmark plan

> Goal: a fair, repeatable, head-to-head measurement of **vyer** (`code` / `code_apply`, the warm
> core, the CLI) against **Claude Code's native tools** (`Read`, `Edit`, `Write`, `Grep`, `Glob`)
> across the read → search → edit loop, at several repo scales.
>
> Companion to `update.md` (which lists the qualitative gaps). This file is the *quantitative* plan.

---

## 0. The core measurement problem (read this first)

The two sides are not symmetric:

- **Vyer** is a daemon with an MCP surface and a CLI (`vyer serve`, `cargo run -p vyer-server
  --example warm_bench`). It can be driven by a script → easy to time and to count tokens.
- **Native tools** (`Read`/`Edit`/`Grep`/…) are **internal to the Claude Code harness**. There is no
  public CLI to call `Edit` 1,000 times from a shell script. They only run *inside an agent turn*.

So we cannot benchmark them in the same `for` loop. We resolve this with **two complementary tracks**:

1. **Track A — Mechanism micro-bench (no model).** Compare vyer's primitives against the *underlying
   mechanisms* native tools are built on: `Read` ≈ `fs::read` + line-number formatting; `Grep` ≈
   `ripgrep`; `Edit` ≈ read-file + string-replace + write. This isolates raw engine performance
   (latency, throughput, scale) with zero LLM variance. Deterministic, cheap, repeatable thousands of
   times.

2. **Track B — Agent task-completion bench (with model).** Identical tasks run by an agent that is
   **only allowed vyer**, vs an agent **only allowed native tools** (enforced via tool-permission
   config). Measures what actually matters end-to-end: task success, tokens consumed, wall-clock, and
   error/corruption rate — including the read→edit loop and freshness.

Track A answers "is the engine faster?" Track B answers "does the agent do the job better/cheaper?"
We need both; Track B is the one the user ultimately cares about.

---

## 1. Metrics (define before measuring)

| Metric | Unit | Applies to | How captured |
|---|---|---|---|
| Latency p50 / p95 / p99 | ms | A & B | monotonic clock around each op |
| Throughput | ops/s | A | N ops / wall-clock |
| **Context tokens consumed** | tokens | B (and A by proxy) | sum of tool-result bytes → tokenizer; in B, turn token accounting |
| Task success | pass/fail | B | post-task assertion (tests pass / target string present) |
| Edit correctness | % | A & B | re-parse + `pytest` after each edit |
| **Corruption rate** | % | A & B | edits that changed unintended bytes (diff outside target) |
| Search recall@k / precision@k | % | A & B | vs a hand-labelled gold set of expected hits |
| Read-after-write staleness | count | A & B | write then immediately read; count stale reads (must be 0 for vyer) |
| Setup / cold-start | ms | A | first query after daemon start (warm-core build) |
| Failed-call / retry count | count | B | tool calls that errored and needed a retry |

The headline comparison is a 2-D plot: **tokens consumed (x)** vs **task success (y)**, per tool-set,
per task. Vyer's thesis ("compact, structured, fewer round-trips") should show up as *up and to the
left*.

---

## 2. Repo corpus (control for scale — vyer's SLOs are scale-dependent)

| Tier | Repo | ~Files | Why |
|---|---|---|---|
| XS | `playground/snake` | 11 | this session's repo; sanity / fast iteration |
| S | a ~500-file OSS project (e.g. `flask`) | ~500 | typical service |
| M | a ~5k-file project (e.g. `django`) | ~5k | lower bound of vyer's stated SLO band |
| L | a ~30–50k-file monorepo | 30–50k | vyer's headline SLO band (§7 of CLAUDE.md) |

Pin exact commits. Run every tier warm (after one priming pass) **and** cold (fresh daemon) so we see
the warm-core advantage and the cold-start cost separately.

---

## 3. Task suite (the "almost everything" matrix)

Group tasks by the capability under test. Each task has a **deterministic oracle** (an automatic
pass/fail check) so Track B needs no human grading.

### 3.1 Read
- R1 — read one whole file (small / medium / 5k-line).
- R2 — read N scattered files (10, 100).
- R3 — read just one function out of a large file (vyer's `detail=snippet` vs native Read+slice).
- Oracle: returned content contains the known target span; measure tokens + latency.

### 3.2 Search / locate
- S1 — exact identifier (`validate_token`) — lexical.
- S2 — symbol definition (`class Snake`) — structural.
- S3 — "where is X used" (cross-file refs) — graph (`detail=refs`) vs `Grep` + manual.
- S4 — fuzzy / "I don't know the name" (semantic, if enabled) vs `Grep` guesses.
- Oracle: recall@k / precision@k vs a gold hit-set; tokens + latency.

### 3.3 Edit (the loop that matters)
- E1 — add a one-line comment to a function (this session's task) ×100 symbols.
- E2 — rename a symbol's body / change a signature.
- E3 — edit a top-level constant (vyer §1.1 gap — expect native-only).
- E4 — multi-file edit (same change across 6 files).
- E5 — an *intentionally broken* edit (must be rejected) — does the tool catch it?
- Oracle: `pytest` green after E1/E2/E4; corruption diff check; E5 must be refused (vyer) and measure
  whether native silently writes garbage.

### 3.4 Read-after-write freshness
- F1 — edit a function, immediately read it back, assert fresh body. Repeat 100×.
- Oracle: staleness count (vyer target 0).

### 3.5 End-to-end mini-tasks (Track B only)
- T1 — "comment every function in this repo" (the real task; measures the whole loop + the §1.1 gap).
- T2 — "find the function that does X and add a guard clause."
- T3 — "rename Y everywhere and keep tests green."
- Oracle: tests pass + target diff present; record tokens, wall-clock, retries.

---

## 4. Track A harness (mechanism micro-bench, no model)

A small Rust or Python driver that, per repo tier, runs each op M times and records the metrics.

```
benches/
  track_a/
    read_bench.{rs|py}      # vyer code(detail=full,path) vs fs::read+format
    search_bench.{rs|py}    # vyer structural/lexical vs ripgrep
    edit_bench.{rs|py}      # vyer code_apply vs read+replace+write
    freshness_bench.rs      # write→read loop, staleness counter
    harness.py              # orchestrates, writes results.csv
```

- Vyer side: drive the MCP server over stdio (the repo already has a stdio e2e test to copy), or call
  the CLI / `warm_bench` example pattern.
- Native-mechanism side: `std::fs` + the `grep`/`ignore` crates (the very libs native `Grep` uses) so
  we compare engines, not wrappers.
- Output: one `results.csv` (tool, op, tier, warm/cold, run#, latency_ms, bytes, ok). Aggregate with a
  notebook / `ctx_execute` into p50/p95 tables + the tokens-vs-latency plot.
- Repeat each cell ≥1,000× (XS/S), ≥100× (M/L). Discard the first run (JIT/cache warm). Fixed RNG seeds.

This track is fully automatable and is where vyer's warm-core claim (p50 ≈ 1 ms on snake) gets
stress-tested at M/L scale against cold ripgrep.

## 4b. Counting tokens fairly (the metric that decides it)

The whole vyer thesis is *context efficiency*, so token accounting must be rigorous:
- For every tool result, run the bytes through the **same tokenizer** the model uses and record the
  count.
- Compare like-for-like: "read function `foo`" → vyer `detail=snippet` returns ~15 lines + a locator;
  native `Read` returns the whole file (or a chosen range). The token delta *is* the headline result.
- Include the **tool-metadata footprint**: vyer = 1–2 tools (~2k tokens of schema); native = several
  tools billed every turn. Measure both once and add to the per-task totals.

---

## 5. Track B harness (agent task-completion bench, with model)

The honest end-to-end test. Two **identical** agent configs differing only in allowed tools:

- **Config N (native):** allow `Read`, `Edit`, `Write`, `Grep`, `Glob`; **deny** `mcp__vyer__*`.
- **Config S (vyer):** allow `mcp__vyer__code`, `mcp__vyer__code_apply`; **deny** native file tools.
  (Use `.claude/settings.json` permissions / `--allowedTools` / `--disallowedTools` to enforce.)

Procedure per task × repo tier:
1. Reset the repo to the pinned commit (`git stash`/`git checkout -- .` between runs).
2. Run the task with Config N, then Config S (randomize order; alternate to cancel drift).
3. Capture from the run: total tokens (input+output), wall-clock, tool-call count, ret/ error count.
4. Run the oracle (tests / diff assertion) → success bit + corruption check.
5. Repeat **k ≥ 5** times per (task, tier, config) to get a distribution (model is stochastic; one run
   proves nothing). Report mean ± stdev and a paired test (same task, N vs S) for significance.

Controls:
- Same model, same temperature (0 where possible), same system prompt, same task wording.
- Same machine, quiesced (no other load), daemon pre-warmed for vyer runs (and a separate cold cohort).
- Log every tool call (vyer already has an audit log; mirror it for native via transcript parsing).

Threats to validity to note in the writeup:
- Model may "know" native tools better (training bias) → report tool-call efficiency separately.
- Vyer's §1.1 gap means T1 ("comment everything") can't fully succeed on vyer alone — that *is* a
  result; record it as a capability-coverage score, not just pass/fail.
- Permission enforcement must be verified (assert the denied tools were never used).

---

## 6. Deliverables

1. `benches/` harness (Track A scriptable; Track B = runner + task specs + oracles).
2. `results/results.csv` + a generated `results/report.md` with:
   - latency tables (p50/p95/p99) per op × tier × warm/cold,
   - the **tokens-vs-success** scatter (the money chart),
   - corruption + freshness-staleness counts,
   - search recall/precision,
   - a per-capability "vyer wins / native wins / tie" matrix mirroring §2 of `update.md`.
3. A short executive summary: where vyer already beats native, by how much, and which `update.md`
   features would flip the remaining cells.

---

## 7. Suggested execution order (incremental, each independently useful)

1. **XS Track A** on `playground/snake` — wire the harness, prove the plumbing, get first numbers fast.
2. **XS Track B** — 3 tasks (R1, S2, E1) × 5 reps, both configs — validate the permission-gating and
   token accounting end-to-end.
3. Scale Track A to S → M → L (this is where vyer's warm core should pull ahead; also where cold-start
   cost shows).
4. Full Track B task suite on S and M.
5. Write `results/report.md`, fold conclusions back into `update.md` priorities.

---

## 8. Open questions to settle before running

- Token counting: which exact tokenizer / accounting source for Track B turns? (harness telemetry vs
  transcript byte-count vs API usage).
- Native-tool driving in Track B: scripted agent runs (preferred) vs manual sessions — pick one and
  keep it constant.
- For L tier: licence/size of the chosen monorepo and disk budget for the warm core.
- Is semantic mode (Phase 6) in scope? If off, S4 compares vyer-lexical vs native-grep only and we say
  so.
