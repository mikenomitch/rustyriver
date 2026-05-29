# `rustyriver` — Build Spec & Agent Operating Manual

> A from-scratch Rust reimplementation ("slop-fork") of **Litestream v0.5** as an
> **embeddable library**. This document is the complete specification. It is
> written to be dropped into the new `rustyriver` repository and handed to an
> autonomous coding agent to implement end-to-end. Read §0 first — there are
> blocking decisions a human must answer before the agent is set loose.

---

## 0. Decisions (DECIDED — recorded in `OPEN_QUESTIONS.md`)

These were the blocking decisions; all are now answered. Each changes what gets
built or how it gets verified. The agent treats this table as settled fact.

| # | Topic | Answer (DECIDED) | Why it matters |
|---|-------|------------------|----------------|
| **D-1** | Wire-compatible with upstream Litestream v0.5's LTX object-store layout? | **YES** | The backbone of verification. Matching the layout lets the real `litestream` binary restore our backups and vice-versa — the *differential oracle* (§6). |
| **D-2** | Upstream version to pin | **`v0.5.11`** (latest stable, Apr 2026) | We port and differentially test against one frozen target — same tag for source *and* the comparison binary. |
| **D-3** | CI runs the pinned `litestream` binary **and** MinIO (Docker)? | **YES — CI provisions both** | Differential (§6 D1–D3) + integration tests require them. CI installs the `v0.5.11` binary and runs MinIO as a service. |
| **D-4** | SQLite binding | **`rusqlite`, bundled SQLite** | No system dependency, fixed version, supports the "single static binary, no CGo" goal. |
| **D-5** | Object I/O | **`object_store` crate** | One trait for S3/R2/file now; other clouds near-free behind features later. |
| **D-6** | LTX: hand-roll vs Rust `ltx` crate | **Default hand-roll; T2 spike decides** | If a correct, maintained Rust `ltx` crate exists, the spike may adopt it; otherwise hand-roll (control the most correctness-critical code). The spike result is logged before T2 proceeds. |
| **D-7** | One-shot scope | **KEEP set only** (§2): LTX, **L0-only (no compaction)**, single replica, S3 + file clients, lease fencing. Compaction levels, VFS read replicas, extra cloud clients **OUT** (follow-on). | Bounds the blast radius of the autonomous run. |
| **D-8** | License | **Apache-2.0 + NOTICE** attributing Litestream | Litestream is Apache-2.0; the fork carries attribution. |
| **D-9** | Async runtime | **Tokio** | Pervasive assumption in the module design. |
| **D-10** | Edition / MSRV | **Rust 2021; MSRV pinned via `rust-toolchain.toml`** (latest stable at seed time) | CI determinism. |

**Non-blocking notes:** repo/crate name is `rustyriver`; a thin debug CLI
(`src/bin/`) is OUT of one-shot scope.

> D-1 = YES is load-bearing: most of §6's verification depends on it. It is
> settled YES; do not revisit without re-opening the verification plan.

---

## 1. Purpose & Where It Will Be Used

**`rustyriver` is an embeddable Rust library for streaming replication of a
SQLite database to object storage, with point-in-time restore and lease-based
single-primary failover.** It is the in-process equivalent of running Litestream
as a sidecar, exposed as a small async API instead of a separate binary.

**Intended consumer (generic):** a long-running Rust service — a distributed
control-plane / scheduler-type system — that keeps its authoritative cluster
state in a **single SQLite database** (WAL mode, one writer, a small read pool).
That service needs three things and currently has none of them without an
external binary:

1. **Continuous disaster-recovery backup** of the state DB to S3-compatible
   object storage (R2/S3/MinIO) with a small, non-zero RPO.
2. **Fast restore on a fresh host** after the primary is lost (crash/host loss).
3. **Single-primary fencing** via an object-storage lease, so exactly one
   instance is the writer at a time (no split-brain).

**Constraints driven by that use case:** must be embeddable (no external process,
no CGo), cross-platform (Linux/macOS/Windows), driven entirely through a small
async Rust API, and quiet (emit `tracing` spans; the host owns metrics/telemetry).
The host integrates `rustyriver` behind its own persistence abstraction — the
library exposes a handful of types (`Db`, `Replica`, `Leaser`, `restore()`), not
a framework.

---

## 2. What We Are Building (and Not)

Litestream has **two incompatible architectures**. The public "how it works" page
describes the old one; current `main` is the new one. **We target only the new
(v0.5) LTX architecture** — see the prior analysis baked into the decision below.

### The model we implement (Litestream v0.5, LTX)
- **LTX (Litestream Transaction) files** are the unit of replication; each carries
  a **monotonic transaction ID (TXID)**.
- **No "generations."** On a break in continuity, re-snapshot as the next LTX
  file. TXIDs are globally ordered → "DB state at TXID/time X" is a direct lookup.
- **Compaction levels** (L1=30s, L2=5m, L3=hourly) roll small files into larger
  ones to bound restore cost. **We ship L0-only first** (see scope).
- **Checkpoint takeover:** hold a long-running read transaction so nothing else
  checkpoints/restarts the WAL; capture frames ourselves.

### Scope — KEEP / DEFER / DROP

**KEEP (the entire one-shot deliverable):**
- WAL frame parsing + SQLite checksums (byte-exact).
- LTX read/write + TXID handling — **the core**.
- DB lifecycle: checkpoint takeover + LTX capture loop.
- Single-replica sync loop + restore.
- Replica URL/config parsing.
- `ReplicaClient` trait + **S3-compatible client** + **file client**.
- Snapshot / TXID bookkeeping + retention.
- **Object-storage lease** for single-primary fencing.

**DEFER (named follow-on milestones, NOT in the one-shot):**
- **Compaction levels L1/L2/L3** — one-shot ships **L0-only** (every txn = one
  LTX file). Correct, just slower to restore on a long-lived DB. (See Risk R-3.)
- **Live read replicas (VFS + HTTP server)** — useful for read-scaling, not for
  DR/failover.

**DROP (out of scope; re-addable behind cargo features later):**
- All v0.3 shadow-WAL/generations machinery and the `ReplicaClientV3` legacy-restore shim (we are greenfield — nothing to be backward-compatible with).
- GCS / Azure / Alibaba OSS / SFTP / WebDAV / NATS clients.
- Multi-replica fan-out (the host needs exactly one replica target).
- Built-in Prometheus `/metrics` + subscriber plumbing (host owns telemetry).
- Standalone CLI as a product.

---

## 3. Rules of Engagement (NON-NEGOTIABLE GUARDRAILS)

These exist because an autonomous agent's failure modes on a byte-format-critical
library are predictable. **Violating any of these is a failed run, not a
shortcut.** These rules belong verbatim in the repo's `AGENTS.md`.

### 3.1 Correctness honesty
1. **Never weaken a test to make it pass.** Do not delete assertions, narrow
   inputs, add `#[ignore]`, gate with `--no-run`, replace a real assertion with
   `assert!(true)`, or loosen a tolerance. If a test looks wrong, **stop and log
   it** in `OPEN_QUESTIONS.md`; do not "fix" it.
2. **Never stub and claim done.** No `todo!()`, `unimplemented!()`, `panic!("not
   implemented")`, or a function that returns a canned value, in any code path a
   task's Definition of Done depends on. A task is done only when the *real*
   implementation passes its *real* tests.
3. **Golden vectors and the real `litestream` binary are the ONLY sources of
   "expected" values.** Never derive an expected output from `rustyriver`'s own
   output. A failing golden/differential test means **rustyriver is wrong** — never
   "the fixture is wrong." Golden fixtures are immutable once captured (§6.2).
4. **Read the real source before porting.** For every module, read the
   corresponding upstream Go file and the real LTX format definition. Cite the
   upstream file + line range in the module's doc-comment (`//! Ported from
   litestream@<tag> db.go:1234-1300`). **Do not reconstruct the format from
   memory.**

### 3.2 Quality bar (every task, every commit)
5. **The gate must be green before a task is marked done**, with no exceptions
   and no "fix it later":
   ```
   cargo fmt --all --check
   cargo clippy --all-targets --all-features -- -D warnings
   cargo build --all-targets
   cargo test --all
   ```
6. **`unsafe` requires written justification** in a `// SAFETY:` comment and a
   line in the commit body. Default to zero `unsafe`.
7. **No new runtime dependency** beyond those agreed in §0/§5 without logging it
   in `OPEN_QUESTIONS.md` and getting it approved.

### 3.3 Scope & ambiguity discipline
8. **Stay in the KEEP scope (§2).** Do not start compaction, VFS, or extra
   clients. If you think the design needs them, log it; don't build it.
9. **On ambiguity, prefer the most conservative (most-likely-correct, least-data-loss) interpretation, mark it with `// DECISION: <reasoning>`, and log it** in
   `OPEN_QUESTIONS.md`. **On a correctness-critical path (checksums, TXID
   ordering, durability, restore), do not guess — stop and escalate.**
10. **Small, reviewable commits**, one per task (or per task sub-step), using
    the message convention in §9.

### 3.4 Progress integrity
11. **`PROGRESS.md` must reflect true state.** Check a box only when that task's
    gate is green *in CI from a clean checkout* (§6.5), not when it passes locally.
12. **CI is the referee, not the agent's self-report.** "It passes on my machine"
    is not done. Green CI on the pinned environment is done.

---

## 4. Architecture & Module Map

Async on Tokio; object I/O via `object_store`; SQLite via `rusqlite` (bundled).
Each module names the upstream Go file it derives from.

```
rustyriver/
├── Cargo.toml
├── AGENTS.md                 # §3 rules verbatim
├── PLAN.md                   # this document
├── PROGRESS.md               # the task checklist (§5), agent-maintained
├── OPEN_QUESTIONS.md         # ambiguities/decisions log (agent-maintained)
├── reference/                # vendored upstream @ pinned tag (READ-ONLY)
│   ├── litestream-go/        # the Go source we port from
│   └── ltx-format.md         # written-up byte layout (agent produces in T2)
├── tests/
│   ├── fixtures/golden/      # immutable bytes captured from real litestream (§6.2)
│   ├── conformance.rs        # ReplicaClient conformance suite (generic)
│   ├── integration_*.rs      # replicate↔restore vs file + MinIO
│   ├── differential_*.rs     # cross-tool vs real litestream binary (§6.3)
│   ├── property_*.rs         # proptest: replicate→restore == source
│   └── faults_*.rs           # crash/partial-upload/truncation injection
├── fuzz/                     # cargo-fuzz targets for the LTX/WAL parsers
├── scripts/
│   ├── capture-golden.sh     # run ONCE w/ real litestream; commits fixtures
│   └── db_equal.rs|sh        # the equality oracle (§6.1)
└── src/
    ├── lib.rs                # public API: Db, Replica, Leaser, Config, restore()
    ├── error.rs              # thiserror error model
    ├── wal.rs                # <- wal_reader.go: frame/header parse + checksums
    ├── ltx.rs                # <- v3.go + ltx: LTX read/write, TXID  (CORE)
    ├── db.rs                 # <- db.go: checkpoint takeover, capture loop
    ├── store.rs              # <- store.go: snapshot/TXID bookkeeping, retention
    ├── replica.rs            # <- replica.go: single-replica sync loop + restore
    ├── replica_url.rs        # <- replica_url.go: s3://, file:// parsing
    ├── leaser.rs             # <- leaser.go: object-storage lease/fencing
    └── client/
        ├── mod.rs            # ReplicaClient trait <- replica_client.go
        ├── file.rs           # <- file/
        └── object_store.rs   # S3/R2/MinIO via object_store <- s3/
```

**Public API sketch (what the host calls):**
```rust
let db = rustyriver::Db::open(path, cfg).await?;   // checkpoint takeover begins
db.start_replication().await?;                      // background LTX sync loop
// failover on a fresh host:
rustyriver::restore(&replica_cfg, dest_path).await?;
// fencing:
let lease = rustyriver::Leaser::new(obj_cfg).acquire().await?; // renew/standby loop
```

---

## 5. Work Breakdown — Task DAG

Each task is one **porter → reviewer → fixer** unit (§7) with a hard Definition
of Done (DoD) and an associated gate. Tasks in the same wave run in parallel.

| ID | Task | Depends on | DoD (in addition to §3.2 gate) |
|----|------|-----------|--------------------------------|
| **T0** | Repo scaffold: Cargo, CI, `AGENTS.md`/`PLAN.md`/`PROGRESS.md`/`OPEN_QUESTIONS.md`, vendor pinned upstream into `reference/`, empty module + test files, `db_equal` oracle, golden-capture script. | — | CI runs (even if trivially green); upstream vendored at pinned tag; `db_equal` oracle works on two hand-made DBs. |
| **T1** | `wal.rs`: WAL header/frame parse + SQLite checksum (salt rotation, partial-frame). Port `wal_reader_test.go`. | T0 | Unit tests + **golden WAL vectors** pass byte-exact. |
| **T2** | `ltx.rs`: LTX read/write, TXID ordering, framing, checksums. Port `v3_test.go`. **Includes the D-6 spike** (existing crate vs hand-roll) and produces `reference/ltx-format.md`. **De-risk spike:** confirm real `litestream` can restore an **L0-only** replica (informs Risk R-3). | T0 | Unit tests + **golden LTX vectors** pass byte-exact; format write-up committed; L0-restore spike result logged. |
| **T3** | `replica_url.rs`: parse `s3://…`, `file://…`, options. Port `replica_url_test.go`. | T0 | Unit tests pass. |
| **T4** | `error.rs` + `lib.rs` public-API skeleton + position/offset helpers. Port `litestream_test.go` helpers. | T0 | Compiles; helper tests pass; public API types exist (may be unimplemented bodies *only* where a later task owns them — those are tracked, not "done"). |
| **T5** | `client/mod.rs`: `ReplicaClient` trait + **generic conformance suite** (`run_client_suite(client)`). Port `replica_client_test.go`. | T2 | Trait + suite compile; suite is exhaustive (write/read/list/delete LTX+snapshot, ordering, pagination). |
| **T6** | `client/file.rs`. Port `file/` tests. | T5 | Passes the conformance suite. |
| **T7** | `client/object_store.rs` (S3/R2). Port `s3/` tests. | T5 | Passes the conformance suite against **MinIO** in CI. |
| **T8** | `store.rs`: snapshot/TXID bookkeeping + retention selection. Port `store_test.go`. | T2 | Unit tests pass. |
| **T9** | `db.rs`: checkpoint takeover, LTX capture loop, snapshot-on-continuity-break, clean shutdown. Port `db_test.go`, `db_internal_test.go`, `db_shutdown_test.go`. | T1, T2 | Unit + internal tests pass; shutdown releases read-tx with no corruption; resume-after-restart works. |
| **T10** | `replica.rs`: single-replica sync loop + restore orchestration. Port `replica_test.go`, `replica_internal_test.go`. | T8, T9, T5 | **G2 round-trip gate** (§6.4) passes against the file client. |
| **T11** | Integration suite: replicate↔restore vs **file** and **MinIO**, incl. crash-in-the-middle + snapshot-on-continuity-break + retention GC. | T6, T7, T10 | Integration tests green in CI. |
| **T12** | Property tests (proptest): random txn sequences → replicate → restore == source (Oracle A). | T10 | Property suite green with a fixed seed budget; finds no counterexample. |
| **T13** | **Differential cross-tool tests** vs the real `litestream` binary (D1/D2/D3, §6.3). | T11 | **G3 differential gate** passes both directions; D3 byte-identical. |
| **T14** | Fault injection: truncated LTX, partial multipart upload, missed frames, clock skew. | T11 | Restore always yields a valid DB at a valid TXID ≤ last durable; never panics. |
| **T15** | `leaser.rs`: object-storage lease acquire/renew/standby + fencing. Port `leaser.go` (+ `heartbeat.go` liveness as needed). | T7 | Lease unit + integration tests (two contenders → exactly one primary; expiry → failover) green vs MinIO. |
| **T16** | `fuzz/` targets for LTX + WAL parsers; adversarial recovery sweep. | T13, T14 | Fuzz runs N minutes with zero crashes; sweep scenarios pass. |
| **T17** | Docs: `README`, embedding guide, runnable example (open→replicate→simulate loss→restore→verify vs MinIO), API stabilization, coverage report (every ported Go test → status). | all | Example runs in CI; coverage report committed; `OPEN_QUESTIONS.md` has no unresolved correctness-critical items. |

**Execution waves (parallelism):**
- **Wave 0:** T0
- **Wave 1:** T1, T2, T3, T4  → **Gate G1 (format)** before proceeding
- **Wave 2:** T5, T8, T9
- **Wave 3:** T6, T7  (then T15 can start once T7 is green)
- **Wave 4:** T10  → **Gate G2 (round-trip)**
- **Wave 5:** T11, T12, T14, T15
- **Wave 6:** T13  → **Gate G3 (differential)**
- **Wave 7:** T16
- **Wave 8:** T17  → **Gate G5 (release)**

---

## 6. Verification Strategy (the backbone)

A backup/replication library that "looks correct" but corrupts a restore is worse
than none. Verification is layered, and the layers an autonomous agent **cannot
fake** (golden bytes, the real binary, CI) are weighted most heavily.

### 6.1 The equality oracle — "is the restored DB correct?"
Raw file comparison is unreliable (freelist, page ordering can legitimately
differ). Define two oracles, both in `scripts/db_equal`:
- **Oracle A (logical equality)** — the default: both DBs pass `PRAGMA
  integrity_check`; identical `sqlite_master` schema; identical per-table content
  hash (deterministic `ORDER BY` over primary keys → hash). Robust across SQLite
  versions.
- **Oracle B (physical equality)** — after a full checkpoint, the main DB files
  are **byte-identical**. Stronger, but only valid when comparing outputs produced
  by the *same* SQLite version.

### 6.2 Golden vectors — anchor against a hallucinated format
`scripts/capture-golden.sh` runs the **real pinned `litestream`** against scripted
SQL to produce real WAL + LTX bytes and a small replica tree, committed under
`tests/fixtures/golden/`. Rules:
- Captured **once** by a human/agent that has the real binary; outputs committed.
- **Immutable.** `rustyriver` parsers/serializers must match these bytes. A
  mismatch means rustyriver is wrong (§3.1 rule 3).
- Never regenerated from `rustyriver`.

### 6.3 Differential testing vs the real binary — the strongest oracle
Requires D-1 = YES and D-3 = environment has the pinned binary + MinIO.
- **D1 (write path):** `rustyriver` replicates → **real `litestream` restores** →
  Oracle A vs source. Proves our *written* format is real-Litestream-readable.
- **D2 (restore path):** **real `litestream` replicates** → `rustyriver` restores
  → Oracle A vs source. Proves our *reader* handles real-Litestream output.
- **D3 (format cross-check):** both tools restore the **same** replica → **Oracle
  B** between the two outputs (byte-identical). Isolates format fidelity from
  SQLite-version noise because both replay identical page images.

> If D-1 = NO, D1–D3 are impossible and the strongest layer is gone. That is the
> core reason §0 pushes hard for YES.

### 6.4 Test layers and the gates they feed
| Layer | Tooling | Gate |
|-------|---------|------|
| Unit (ported Go tests) | `cargo test` | per-task DoD |
| Golden byte vectors | fixtures + `cargo test` | **G1** (format) |
| Round-trip (replicate→restore, file client) | integration | **G2** |
| Differential D1/D2/D3 | real binary + MinIO | **G3** |
| Property (random txns) | `proptest` | G4 (resilience) |
| Fault injection | custom harness | G4 |
| Fuzz (malformed input → no panic/UB) | `cargo-fuzz` | G4 |
| Concurrency (read-tx lifecycle) | `loom` where feasible | within T9 |
| End-to-end example soak | example bin vs MinIO | **G5** (release) |

**Gate definitions:**
- **G1 (format):** T1+T2 unit + golden pass. *No client/db/replica work merges
  until G1 is green* — format errors otherwise cascade into every module.
- **G2 (round-trip):** open→replicate→restore reproduces the source (Oracle A
  and, where applicable, B) via the file client.
- **G3 (differential):** D1, D2 pass (Oracle A); D3 byte-identical (Oracle B).
  **This is "M1 correct."**
- **G4 (resilience):** property + fault-injection + fuzz green.
- **G5 (release):** everything green in CI from clean checkout; example runs;
  coverage report committed; `OPEN_QUESTIONS.md` clear of correctness blockers.

### 6.5 Anti-gaming: CI is the impartial referee
- A **GitHub Actions** workflow runs the full §3.2 gate **from a clean checkout**
  on every push (services: MinIO; install the pinned `litestream` binary for
  differential jobs). The agent's local pass does not count — **green CI does**.
- A **reviewer agent re-runs the gate from a fresh clone** and spot-audits that
  tests assert real behavior (not tautologies, not `#[ignore]`d, assertions
  intact). It specifically diffs test files against the upstream Go tests to
  catch silently-weakened ports.
- CI **fails the build** if it detects `todo!`/`unimplemented!`/`#[ignore]`/`assert!(true)` in non-`reference/` code, or if any golden fixture changed in the
  same commit as source under `src/` (guards against "fix the fixture").

---

## 7. Execution via a Dynamic Workflow

This is a textbook dynamic-workflow job: a large, mostly-parallel migration where
the orchestration is worth codifying, and where independent adversarial review of
each port materially raises trust.
([Claude Code: Orchestrate subagents at scale](https://code.claude.com/docs/en/workflows))

### 7.1 Orchestration shape
The workflow walks the §5 waves. For each task it runs a 3-role loop:
1. **Porter** — reads the named upstream Go file + its test + `reference/ltx-format.md` + the relevant golden fixtures + `AGENTS.md`; writes the Rust module and ports the test; gets the local gate green.
2. **Reviewer (fresh context)** — given the Go original and the Rust port, hunts a behavioral divergence (checksum endianness, off-by-one offsets, TXID ordering, error mapping, retention boundaries, partial-frame handling). Writes a *failing* test if it finds one. Also enforces §3 (no stubs/weakened tests).
3. **Fixer** — resolves reviewer findings. Loop 2↔3 up to N iterations; if not converged, **stop and escalate** to `OPEN_QUESTIONS.md` rather than ship a guess.
A task is "done" only after the **reviewer cannot refute it AND CI is green** (§6.5).

### 7.2 Per-agent prompt template (porter)
```
You are porting ONE module of rustyriver, a Rust reimplementation of Litestream v0.5.
RULES: read and obey AGENTS.md in full. Hard rules: never weaken/skip a test;
never stub and claim done; golden vectors and the real litestream binary are the
only sources of truth; cite upstream file:line in the module doc-comment.

TASK: <task id + one-line goal>
PORT FROM: reference/litestream-go/<file>.go  and its <file>_test.go
FORMAT REF: reference/ltx-format.md ; golden fixtures: tests/fixtures/golden/<...>
SCOPE: implement only <module>. Mock dependencies behind their traits.
DONE WHEN: ported tests + relevant golden tests pass AND `cargo fmt --check &&
cargo clippy -- -D warnings && cargo test` is green. Log any ambiguity in
OPEN_QUESTIONS.md; STOP on correctness-critical ambiguity instead of guessing.
```

### 7.3 Operational notes
- Pre-add `cargo`, `git`, `docker`/MinIO, and the `litestream` binary to the **tool allowlist** before launch so subagents don't stall on permission prompts.
- Route mechanical translation to a **smaller model**; reserve the strongest model for T2 (LTX), T9 (db/concurrency), T13 (differential), T16 (adversarial sweep).
- **Worktree-per-porter isolation** so parallel tasks don't collide; the orchestrator merges only gate-green units.
- **Save the run** as `/port-rustyriver` so re-syncing with a newer upstream tag is one command.
- The runtime cap is 16 concurrent agents / 1,000 total per run — the §5 waves fit comfortably.

---

## 8. Definition of Done (the one-shot is complete when…)

1. All tasks **T0–T17** complete; **PROGRESS.md** fully checked and truthful.
2. **CI green from a clean checkout**, full §3.2 gate, on the pinned environment.
3. **G1–G5 all green**, including **differential G3 both directions** (assuming
   D-1=YES / D-3 provisioned).
4. Property, fault-injection, and fuzz suites pass (G4).
5. The runnable **example** (open → replicate → simulate host loss → restore →
   `db_equal` OK) succeeds in CI against MinIO.
6. **Coverage report** maps every upstream Go test → {ported | deferred | dropped}
   with reasons; no KEEP-scope test silently missing.
7. **OPEN_QUESTIONS.md** has zero unresolved *correctness-critical* items (design
   nits may remain, flagged).
8. Public API documented; `cargo doc` clean.

If any item can't be met, the agent **stops and reports** with the specific
blocker — it does not lower the bar to declare success.

---

## 9. Seeding the New Repo

Drop these into the fresh `rustyriver` repo before launching the agent:
- `PLAN.md` ← this document.
- `AGENTS.md` ← §3 verbatim (the guardrails), plus the §3.2 gate command block.
- `PROGRESS.md` ← the §5 task table as an unchecked checklist.
- `OPEN_QUESTIONS.md` ← seeded with the §0 decisions and their chosen answers.
- `reference/litestream-go/` ← upstream vendored at the **pinned tag** (D-2), read-only.
- A CI workflow implementing §6.5.

**Commit convention:** `T<id>: <imperative summary>` (e.g., `T2: implement LTX
reader/writer with golden-vector tests`). Body notes any `// DECISION:` made and
any `unsafe`. One task per commit (or per sub-step of a large task).

**Pre-launch human checklist:** answers to all of §0 recorded in
`OPEN_QUESTIONS.md`; pinned `litestream` binary + MinIO available to CI (D-3);
golden vectors captured and committed (`scripts/capture-golden.sh`); allowlist
configured (§7.3).

---

## 10. Risks & Mitigations

- **R-1 Checksum / endianness bugs** (the classic Litestream footgun). → Golden
  vectors (G1) + differential D3 + the adversarial sweep concentrate here.
- **R-2 LTX format fidelity / the D-6 unknown.** → T2 spike resolves crate-vs-hand-roll *first*; `reference/ltx-format.md` + golden vectors gate the whole fan-out at G1.
- **R-3 L0-only restore correctness & cost.** Must confirm the real binary restores an **L0-only** replica (T2 spike, validated by D1). If it can't, a minimal L1 compaction enters scope — **escalate before assuming.** Cost (slow restore on long-lived DBs) is acceptable for the first cut; document it.
- **R-4 SQLite interop / checkpoint takeover.** Go uses mattn/SQLite; we use bundled `rusqlite`. Read-tx hold + manual `PRAGMA wal_checkpoint` semantics must match exactly. → T9 unit tests + a `loom` concurrency test for the read-tx lifecycle.
- **R-5 Environment can't run the real binary / MinIO (D-3=NO).** → Then G3 can't run in the one-shot; this is a *named, accepted* confidence reduction, not something to paper over. Prefer to fix the environment.
- **R-6 AI translation "looks right, isn't."** → The whole point of the per-task adversarial reviewer + property/fuzz + the §6.5 anti-gaming CI checks. Never merge on porter confidence alone.
- **R-7 Upstream drift.** → Pin one tag (D-2); the saved workflow re-runs against a newer tag later.
- **R-8 Scope creep into deferred features.** → §3 rule 8; CI/reviewer reject out-of-scope modules.

---

## 11. Appendix — Slop-Fork Best Practices (vinext-derived)

1. **Compatibility over elegance, first.** Parity with the format and the test
   suite beats idiomatic beauty on pass one; refactor after green.
2. **The original test suite is the spec.** Port tests alongside the code they
   exercise; the upstream tests are the oracle.
3. **Refactor generated code into typed, linted modules.** No machine-translated
   blobs; everything passes fmt + clippy.
4. **Rich context wins (~20% quality lift in vinext).** Pin the Go source, its
   test, the format write-up, and the guardrails into every porter prompt.
5. **The last 6% is the real work.** Coverage reaches ~94% fast; the edge cases
   (checksums, partial-LTX recovery, retention races, crash consistency) are where
   the hardening lives — that's why §6 weights differential/property/fuzz/fault
   layers so heavily.

Sources:
[Slop Fork (mbleigh.dev)](https://mbleigh.dev/posts/slop-forks/) ·
[AI Slop Forks (builder.io)](https://www.builder.io/blog/ai-slop-forks) ·
[Syntax #988](https://syntax.fm/show/988/cloudflare-s-next-js-slop-fork) ·
[Litestream v0.5 (Fly)](https://fly.io/blog/litestream-v050-is-here/) ·
[Litestream VFS (Fly)](https://fly.io/blog/litestream-vfs/) ·
[Migration Guide](https://litestream.io/docs/migration/) ·
[Dynamic workflows](https://code.claude.com/docs/en/workflows)
```
