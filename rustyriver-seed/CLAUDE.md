# CLAUDE.md — rustyriver

`rustyriver` is a from-scratch Rust reimplementation of **Litestream v0.5**
(pinned `v0.5.11`) as an **embeddable library**: streaming SQLite replication to
object storage, point-in-time restore, and object-storage lease fencing. The
full spec is in **`PLAN.md`**. This file is the **non-negotiable operating
manual**. Read it fully before writing any code.

## The gate (run before marking ANY task done)
```sh
cargo fmt --all --check
cargo clippy --all-targets --all-features -- -D warnings
cargo build --all-targets
cargo test --all
```
A task is done only when its real implementation passes its real tests **and**
this gate is green **in CI from a clean checkout** — not just locally.

## Non-negotiable rules

### Correctness honesty
1. **Never weaken a test to make it pass.** No deleting assertions, narrowing
   inputs, `#[ignore]`, `--no-run`, `assert!(true)`, or loosened tolerances. If a
   test looks wrong, **STOP and log it** in `OPEN_QUESTIONS.md`. Do not "fix" it.
2. **Never stub and claim done.** No `todo!()`, `unimplemented!()`,
   `panic!("not implemented")`, or canned return values on any path a task's
   Definition of Done depends on.
3. **Golden vectors and the real `litestream` binary are the ONLY sources of
   "expected".** Never derive expected output from rustyriver itself. A failing
   golden/differential test means **rustyriver is wrong**, never that the fixture
   is wrong. Fixtures under `tests/fixtures/golden/` are **immutable**.
4. **Read the real source before porting.** For each module, read the upstream Go
   file (`reference/litestream-go/`) and the real LTX format. Cite the upstream
   `file:line` range in the module doc-comment:
   `//! Ported from litestream@v0.5.11 db.go:1234-1300`. Do **not** reconstruct
   the format from memory.

### Quality bar
5. The gate above passes — no exceptions, no "fix clippy later."
6. **`unsafe` requires a `// SAFETY:` comment and a note in the commit body.**
   Default to zero `unsafe`.
7. **No new runtime dependency** beyond those in `PLAN.md` §5 / `OPEN_QUESTIONS.md`
   without logging and approval.

### Scope & ambiguity
8. **Stay in the KEEP scope** (`PLAN.md` §2). Do **not** start compaction levels,
   the VFS/read-replica server, or extra cloud clients (gcs/abs/oss/sftp/webdav).
9. On ambiguity: prefer the **most conservative, least-data-loss** interpretation,
   mark it `// DECISION: <reasoning>`, and log it in `OPEN_QUESTIONS.md`. **On a
   correctness-critical path (checksums, TXID ordering, durability, restore) do
   NOT guess — STOP and escalate.**

### Progress integrity
10. **`PROGRESS.md` reflects true state** — check a box only when that task's gate
    is green in CI from a clean checkout.
11. **CI is the referee, not your self-report.** "Passes on my machine" ≠ done.

## How each task is executed (porter → reviewer → fixer)
- **Porter** writes the module + ports its Go test; gets the local gate green.
- **Reviewer** (fresh context) compares the Go original to the Rust port and
  hunts a behavioral divergence (checksum endianness, off-by-one offsets, TXID
  ordering, error mapping, retention boundaries, partial-frame handling); writes
  a *failing* test if found; enforces rules 1–2 and 8.
- **Fixer** resolves findings; loop reviewer↔fixer until the reviewer cannot
  refute. If it won't converge, **stop and escalate** — never ship a guess on a
  correctness-critical path.

## Commit convention
`T<id>: <imperative summary>` — e.g. `T2: implement LTX reader/writer with
golden-vector tests`. One task (or sub-step) per commit. Body notes any
`// DECISION:` made and any `unsafe`.

## Map
- `PLAN.md` — full spec (scope, architecture, task DAG, verification, gates).
- `PROGRESS.md` — the task checklist; keep it truthful.
- `OPEN_QUESTIONS.md` — settled decisions + your escalations log.
- `reference/` — vendored upstream @ `v0.5.11`, **read-only** ground truth.
- `tests/fixtures/golden/` — immutable bytes from the real binary.
