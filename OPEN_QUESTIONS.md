# OPEN_QUESTIONS.md — rustyriver

Two parts: **settled decisions** (treat as fact) and the **escalations log** the
agent appends to when it hits ambiguity (per `AGENTS.md` rule 9).

## Settled decisions (from `PLAN.md` §0)

| # | Topic | Decision |
|---|-------|----------|
| D-1 | Wire-compatible with upstream LTX layout | **YES** — the differential oracle depends on it. |
| D-2 | Pinned upstream version | **`v0.5.11`** (latest stable, Apr 2026) — source *and* comparison binary. |
| D-3 | CI runs real `litestream` + MinIO | **YES** — CI installs the `v0.5.11` binary and runs MinIO as a service. |
| D-4 | SQLite binding | **`rusqlite`, bundled SQLite.** |
| D-5 | Object I/O | **`object_store` crate.** |
| D-6 | LTX: hand-roll vs crate | **Default hand-roll; resolved by the T2 spike** (adopt a crate only if a correct, maintained one exists). Record the spike outcome below. |
| D-7 | One-shot scope | **KEEP set only** — LTX, **L0-only (no compaction)**, single replica, S3 + file, lease fencing. Compaction/VFS/extra-clients OUT. |
| D-8 | License | **Apache-2.0 + NOTICE** attributing Litestream. |
| D-9 | Async runtime | **Tokio.** |
| D-10 | Edition / MSRV | **Rust 2021; MSRV pinned via `rust-toolchain.toml`.** |

Non-blocking: crate/repo name `rustyriver`; no debug CLI in the one-shot.

## Known unknowns to resolve DURING the run (do not guess past these)

- **U-1 (D-6) — LTX crate vs hand-roll.** T2 spike must (a) check for a correct,
  maintained Rust `ltx` crate at the format version `v0.5.11` produces, and
  (b) record the decision + reasoning here before T2 proceeds.
  → **Outcome (2026-05-29): HAND-ROLL.** Evidence: litestream v0.5.11's `go.mod`
  pins the format to **`github.com/superfly/ltx v0.5.1`** (vendored at
  `reference/ltx-go`, commit `a08d200e…`) — that Go package (`encoder.go`,
  `decoder.go`, `checksum.go`, `file_spec.go`, `ltx.go`) is the authoritative
  byte-format spec and is what T2 ports. crates.io has no maintained Rust `ltx` at
  this format version: the only candidate, `litetx` v0.1.0, is a single unproven
  0.1 release (others — `ltx_2_5`, `ltx_3` — are unrelated stubs). For a
  correctness-critical format gated by golden vectors + differential D1/D3, the
  plan's conservative default (hand-roll) wins. Observed L0 magic: `LTX1` + page
  size `0x1000`.
- **U-2 (Risk R-3) — L0-only restore.** Confirm the real `litestream v0.5.11`
  binary can restore a replica that contains **only L0 LTX files** (no L1/L2/L3).
  If it cannot, minimal L1 compaction enters scope — **escalate, do not silently
  expand scope.** Validate via differential D1 as early as possible.
  → **Outcome (2026-05-29): CONFIRMED — L0-only restore works.** Spike: built the
  real binary from tag v0.5.11, replicated a WAL-mode DB with `replicate -once`
  producing an L0-only tree (`ltx/0/…`, snapshot at TXID 1 + 5 single-txn files
  TXIDs 2–6, **no level ≥1**), then `litestream restore` reproduced it and
  `db_equal A` passed. Risk R-3 retired: L0-only is a valid shippable architecture
  for the one-shot. Compaction stays OUT of scope. (Fixtures: `tests/fixtures/golden/replica/`.)

## Escalations log (agent appends; newest first)

> Format:
> ### YYYY-MM-DD — T<id> — <one-line title>
> **Context:** what code/path, which upstream ref.
> **Ambiguity:** what's unclear and why it's correctness-relevant.
> **Conservative choice taken (if any):** `// DECISION:` summary, or "STOPPED — needs human".
> **Needs from human:** the specific answer required to proceed.

### 2026-05-29 — T0 — Toolchain pin bumped 1.84.0 → 1.90.0 (D-10)
**Context:** `rust-toolchain.toml`, `Cargo.toml` `rust-version`.
**Ambiguity:** The seed pinned Rust 1.84.0, but the resolved dependency tree
(getrandom, hashbrown, icu_*, proptest, security-framework, …) requires Rust
1.85/1.86 (edition-2024 support). 1.84.0 cannot build it.
**Conservative choice taken:** `// DECISION:` pin to **1.90.0** (latest stable
already installed locally; D-10 explicitly allows choosing the seed-time stable).
Our crate stays **edition 2021**; only the *toolchain* is newer, to compile deps.
`Cargo.lock` is committed for CI determinism.
**Needs from human:** none — recorded for visibility. CI reads the channel via
`rustup show`, so it tracks automatically.

### 2026-05-29 — T0 — Explicit `[workspace]` to shield from a stray ancestor manifest
**Context:** `Cargo.toml`.
**Ambiguity:** This crate lives in a git worktree nested under the parent repo,
which carries its own untracked `Cargo.toml`; cargo walked up and adopted that
ancestor manifest ("no targets specified").
**Conservative choice taken:** `// DECISION:` add an empty `[workspace]` table so
this manifest is an explicit workspace root. No functional effect on the crate.
**Needs from human:** none — recorded for visibility.
