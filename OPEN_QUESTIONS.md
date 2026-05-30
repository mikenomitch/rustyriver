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
  → _Outcome:_ _(agent fills in)_
- **U-2 (Risk R-3) — L0-only restore.** Confirm the real `litestream v0.5.11`
  binary can restore a replica that contains **only L0 LTX files** (no L1/L2/L3).
  If it cannot, minimal L1 compaction enters scope — **escalate, do not silently
  expand scope.** Validate via differential D1 as early as possible.
  → _Outcome:_ _(agent fills in)_

## Escalations log (agent appends; newest first)

> Format:
> ### YYYY-MM-DD — T<id> — <one-line title>
> **Context:** what code/path, which upstream ref.
> **Ambiguity:** what's unclear and why it's correctness-relevant.
> **Conservative choice taken (if any):** `// DECISION:` summary, or "STOPPED — needs human".
> **Needs from human:** the specific answer required to proceed.

_(none yet)_
