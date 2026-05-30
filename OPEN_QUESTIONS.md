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

### 2026-05-30 — T7 — ObjectStore (S3/R2/MinIO) client: DECISIONs + deferrals
**Context:** `src/client/object_store.rs` (the S3/R2/MinIO `ReplicaClient`),
`tests/integration_minio.rs`. Behavior ported from `s3/replica_client.go`; the
transport is the **`object_store` crate** (feature `s3`), not the Go AWS SDK
(D-5 / brief T7). No new runtime dependency — `object_store` is already in
`Cargo.toml`.

**DECISION — offset-past-EOF returns an empty slice (file-client parity, not Go
parity).** A range request with `offset >= object_size` returns HTTP 416 from
S3/MinIO. Go's `OpenLTXFile` would propagate that 416 as an error; our file
client (T6) returns an empty `Vec` when `offset >= len`. T7 mirrors the **file
client** so both backends are interchangeable under the shared
`run_client_suite` (the T7 Definition of Done). No real read path ever requests a
past-EOF offset — restore reads the in-bounds page-index tail — so this only
affects the degenerate case; any non-416 read error still propagates.
(`src/client/object_store.rs`, in `open_ltx_file` + `is_range_not_satisfiable`.)
The 416 is detected by matching the rendered error string ("416" / "Range Not
Satisfiable"), because `object_store 0.11` surfaces it as a `Generic` error with
no dedicated variant; verified against live MinIO.

**DECISION — `litestream-timestamp` stored as an `object_store` metadata
attribute; round-tripped via a hand-rolled RFC3339Nano (no date crate).** Go
writes `time.UnixMilli(hdr.Timestamp).UTC().Format(time.RFC3339Nano)` into S3
object metadata and parses it back with `time.Parse(time.RFC3339Nano, …)`
(s3/replica_client.go:671-681, 1435). We store it as
`Attribute::Metadata("litestream-timestamp")` and (de)serialise the exact shape
we write — `YYYY-MM-DDTHH:MM:SS[.fffffffff]Z` — with two small civil-date helpers
(Howard Hinnant's algorithm), to avoid pulling in `chrono`/`time` (AGENTS.md rule
7). This is sufficient because the `use_metadata` restore path only ever reads
timestamps that this same client wrote. Round-trip + malformed-input rejection
are unit-tested; the end-to-end metadata read is tested against live MinIO.

**DECISION — multipart threshold implemented explicitly at 5 MiB.**
`object_store 0.11` has **no** `with_multipart_threshold`; `put`/`put_opts` is a
single PUT and `put_multipart_opts` is caller-driven. To match the Go uploader's
5 MiB `PartSize` default (s3/replica_client.go:99, brief §5.1) we branch in
`write_ltx_file`: `< 5 MiB` → single `put_opts`; `>= 5 MiB` → `put_multipart_opts`
with fixed-size parts (each `>= 5 MiB` except the last, per object_store's part
rule). Verified against live MinIO with a ~6 MiB incompressible LTX file
(byte-exact round-trip + a byte-range read on the multipart object).

**DECISION — `skip_verify` maps to `allow_http`.** `object_store` gates plaintext
behind `with_allow_http`, with no TLS-verify toggle. We enable `allow_http` for
`http://` endpoints, local endpoints, or when `skip_verify` is set. (Full TLS
`SkipVerify` against an https self-signed endpoint is not reachable through
object_store 0.11's public builder; not needed for the MinIO/file KEEP scope.)

**DEFERRED (not on the KEEP functional path; logged, not built):**
SSE-C / SSE-KMS request headers; payload-signing / `RequireContentMD5` /
aws-chunked middleware assertions; Tigris consistent-read header; the
`LITESTREAM_S3_DEBUG` env knob; access-point ARN *endpoint* wiring (ARN parsing
into bucket/region IS ported). These are AWS-SDK-middleware concerns the
`object_store` backend either handles internally or that only matter on native
AWS; none affects MinIO conformance or the round-trip/restore correctness this
task gates. The corresponding Go unit tests (httptest/smithy-mock) assert SDK
wire details we don't control and were therefore not ported; the five
`ReplicaClient` operations are fully real and exercised end-to-end against MinIO.

**MinIO gate:** `tests/integration_minio.rs` runs the generic `run_client_suite`
(+ the `use_metadata` timestamp path and the 5 MiB multipart boundary) against a
real MinIO. It **auto-skips with a logged note** (TCP-reachability probe) when
Docker/MinIO is unavailable, so it never blocks the gate; the pure-unit ports
(ParseHost, R2 concurrency, URL query aliases, endpoint env var, key scheme,
NotFound mapping, RFC3339Nano) cover the client regardless. In this run MinIO
was up and all three live tests passed.

### 2026-05-30 — T10 — Replica sync+restore: compactor reimplementation (DECISION) + L0-only restore-plan + deferrals
**Context:** `src/replica.rs` (the single-replica sync loop + restore). Ported from `replica.go`
(+ ltx@v0.5.1 `compactor.go`/`decoder.go`). Drives G2.
**DECISION — reimplement the LTX compactor as an in-memory page-merge (not a streaming
`io.Pipe`+`Compactor`+`DecodeDatabaseTo`):** Go's `Replica.Restore` (replica.go:667-682) wires
`ltx.NewCompactor(pw, rdrs)` into a goroutine writing an `io.Pipe`, then `Decoder.DecodeDatabaseTo`
reads the compacted snapshot off the other end into the DB file. The buffered-I/O DECISION already
taken for the client (T5) makes a streaming pipe pointless here, so `build_database_image` does the
compactor's job directly: decode+**verify** every input (`ltx::decode_file`, so a corrupt download
is caught), then for pages `1..=commit` (commit = the LAST input's `Commit`, the truncation bound,
compactor.go:108-118,217) take the **latest** input carrying each page (forward-iterate, last write
wins == compactor's newest-first selection compactor.go:203-224), with the lock page zero-filled
(decoder.go:236-254). Proven equivalent to the real tool three ways: (a) a single-snapshot merge is
byte-identical to `ltx::decode_database_image`; (b) the G2 round-trip reproduces the source under
Oracle A; (c) **our restore of the committed golden replica is Oracle-A-identical to the real
`litestream restore` of the same tree** (`restore_golden_replica_matches_real_litestream`).
**Finding (not an ambiguity) — L0-only stores snapshots at level 0, not `SnapshotLevel(9)`:** the
real binary's L0-only tree (golden fixture, captured from `replicate -once`) holds the snapshot
(MinTXID==1) and every incremental under `ltx/0/`; level 9 is empty without compaction.
`calc_restore_plan` is still a faithful port of `CalcRestorePlan` (snapshot anchor probed at
`SnapshotLevel` + per-level `restoreLevelCursor`s + `restoreCandidateBetter`), so it works unchanged
once compaction lands; in this scope the plan is the contiguous L0 chain. Verified the
continuity-break shape (a re-emitted wider snapshot 1-N among per-txn files) selects the wider
snapshot (`calc_restore_plan_prefers_wider_reemitted_snapshot`).
**Discovery — `Replica` owns an OPTIONAL `Db`:** Go's `NewReplicaWithClient(nil, client)` (used by the
restore-only tests) means the db pointer is nilable; mirrored as `db: Option<Db>`. `restore()` is a
free function over a bare `&C: ReplicaClient` (no DB needed), matching `Replica.Restore`'s
client-only data flow. Added `Error::TxNotAvailable` (store.go:27) for the empty-replica /
unreachable-TXID cases, and made `Db::ltx_path` `pub` (Go's exported `DB.LTXPath`, read by
`uploadLTXFile`).
**Deferrals (logged, NOT on the G2 path; land with the background runtime / later waves):**
1. **Follow mode** (`Replica.follow`/`applyNewLTXFiles`/`applyLTXFile`/`fillFollowGap`,
   replica.go:730-987 + the `TestReplica_ApplyNewLTXFiles_*`/`ApplyLTXFile_VerifiesChecksumOnClose`
   internal tests): the continuous tail-restore loop + its gap-bridging across compaction levels.
   The one-shot G2 gate is a single restore; follow needs the higher compaction levels (out of L0
   scope) to be meaningful and a long-running poll loop. The page-apply primitive it shares
   (`build_database_image`'s merge) is implemented + tested.
2. **Background monitor goroutine + exponential backoff** (`Replica.monitor`/`Start`/`Stop`,
   replica.go:92-128,326-441, footgun F-13) + `AutoRecoverEnabled` reset path: needs a Tokio task
   owning the `Db`; the synchronous `sync()` primitive it would call on a ticker is implemented and
   tested here. The `auto_recover_enabled` field is kept on `Replica` so the surface matches upstream.
3. **V3 (v0.3.x generation) restore** (`RestoreV3` and the whole `ReplicaClientV3` shim,
   replica.go:990-1418): **DROPPED** (PLAN.md §2 — greenfield, nothing to be backward-compatible
   with). Recorded in the T17 coverage report.
4. **Timestamp / point-in-time restore through the public API + `RestoreOptions`/integrity-check
   modes** (`CalcRestoreTarget`/`checkIntegrity`, replica.go:443-520,1238-1272): `calc_restore_plan`
   already honors a target **TXID** (tested), and the integration test runs the equivalent of a
   post-restore integrity check via Oracle A's `PRAGMA integrity_check`. The timestamp path +
   structured options surface stay minimal for the one-shot; revisit at T11 (integration) /
   public-API stabilization (T17).
5. **`EnforceRetention`** (replica.go:252-284) is `panic!("TODO")` upstream pending multi-level
   compaction; the L0/snapshot retention **selection** already lives in `store.rs` (T8). Not ported
   (out of scope), not on any tested path.
**Needs from human:** none — conservative, tested, golden-anchored choices throughout.

### 2026-05-30 — T9 — Async/blocking shape (DECISION: synchronous `Db`) + two-connection read-lock + deferrals
**Context:** `src/db.rs` (the WAL→LTX capture loop + checkpoint takeover). Ported from `db.go`.
**DECISION — synchronous `Db` (footgun F-1 / brief §2 shape choice):** The brief offered
(A) a dedicated DB thread or (B) `spawn_blocking`+`Mutex<Connection>` for the `!Sync`
`rusqlite::Connection` + the borrowing read-tx. I took the third option the **task brief
explicitly sanctions**: keep the capture API **synchronous** and let T10 drive it. `Db`
owns the connection(s) directly; the long-running read transaction is held with raw
`BEGIN`/`ROLLBACK` SQL plus a `read_lock_held` flag (exactly Go's `acquireReadLock`/
`releaseReadLock`, not a borrowing `rusqlite::Transaction`). This sidesteps the
!Sync/borrow problem entirely, makes the idempotent double-release (issue #934) a trivial
flag check, and keeps `verify`/`sync`/`checkpoint` as a faithful branch-for-branch port.
`&mut self` serializes sync/checkpoint/snapshot, so the upstream `chkMu` snapshot-vs-
checkpoint gate (footgun F-3) is unnecessary; T10 preserves this by giving the `Db` a
single owner (one blocking thread / `spawn_blocking`).
**Discovery — two connections, mirroring Go's `*sql.DB` pool (load-bearing):** Go's `db.db`
is a **connection pool**: the read lock (`db.rtx`) lives on one pooled connection while
every other `db.db.Exec` (the `_litestream_seq` write after a checkpoint, the
`_litestream_lock` write-lock grab) runs on a *different* pooled connection. A single
`rusqlite::Connection` cannot replicate that — an `INSERT` on the same connection that
holds the read transaction is buffered in that transaction and is **not flushed to the
WAL** (so it would not write a fresh WAL page after a TRUNCATE checkpoint, breaking the
two-phase restart detection in `checkpoint`, db.go:1828-1842). Fix: `Db` holds **two**
connections — `conn` (writes/PRAGMAs/checkpoints) and a dedicated `rtx_conn` (the read
lock). Verified by spike + the `checkpoint_does_not_trigger_snapshot{,_passive,_truncate}`
and `crc64` tests. Not a new dependency; just the faithful pool model.
**Finding (not an ambiguity) — SQLite checkpoint pragmas don't error on busy:** a
`PRAGMA wal_checkpoint(<mode>)` blocked by a reader/writer returns `Ok((1, …))` (busy flag
in column 0), **not** an `Err`. So `isSQLiteBusyError` (footgun F-6) matters for the
*surrounding* ops (the post-checkpoint `INSERT`/`BEGIN`), which is exactly what
`checkpoint_passive_swallowing_busy` wraps — faithful to db.go:1117-1125 wrapping the whole
`db.checkpoint` call. Real rusqlite busy errors carry "database is locked", which the
classifier matches.
**Deferrals (logged, not on the functional capture path; land with T10's `Replica`):**
1. **PERSIST_WAL** (`setPersistWAL`, the `unsafe` `sqlite3_file_control(SQLITE_FCNTL_PERSIST_WAL)`
   FFI, footgun F-10): only matters when *all* connections close and SQLite would delete
   the WAL on last close. The capture path keeps `conn`+`rtx_conn` open, so the WAL persists
   for the process lifetime. Revisit in T10 (process-restart durability) — likely the
   crate's one `unsafe` block; justify per AGENTS.md rule 6 or find a safe rusqlite path.
2. **Background monitor loop + backoff** (footgun F-13) and **`Replica` integration**
   (`ensure_exists`/`sync_status`/`sync_and_wait`/`syncReplicaWithRetry`/`Done`/the
   `EnforceL0RetentionByTime`+`EnforceSnapshotRetention` driver, the 8 shutdown subtests,
   `checkDatabaseBehindReplica` #781): all need T10's `Replica` handle / `ReplicaClient`
   upload. The *selection* logic already exists (T8 `store.rs`); T9 exposes the local-half
   primitives (`sync`/`checkpoint`/`snapshot_to_writer`/`crc64`/`pos`/`reset_local_state`)
   the `Replica` will call. `Db::close` here releases the read lock + closes the conn; the
   replica final-sync + retry wrap it in T10.
3. **`loom` model of the lock protocol** + the real-SQLite concurrent-checkpoint page-gap
   race (`TestDB_CheckpointPageGapWithConcurrentWrites`, `TestDB_ConcurrentMapWrite`):
   the synchronous `&mut self` design has no intra-`Db` data race to model; the
   cross-process WAL race is a T11 integration concern once T10 wires the monitor.
4. **Prometheus metric assertions** in the upstream white-box tests: DROPPED (host owns
   telemetry, PLAN.md §2) — ported the *behavior* (sync/checkpoint succeed/err), not the
   counter asserts. **L1+ compaction tests** (`TestDB_Compact*`): DEFER (out of L0 scope).
   Record both in the T17 coverage report.
**Needs from human:** none — conservative, tested choices throughout; recorded for
visibility and to route the PERSIST_WAL `unsafe` + Replica-integration tests to T10.


> Format:
> ### YYYY-MM-DD — T<id> — <one-line title>
> **Context:** what code/path, which upstream ref.
> **Ambiguity:** what's unclear and why it's correctness-relevant.
> **Conservative choice taken (if any):** `// DECISION:` summary, or "STOPPED — needs human".
> **Needs from human:** the specific answer required to proceed.

### 2026-05-30 — T5/T6 — ReplicaClient I/O shape (DECISION) + two deferrals
**Context:** `src/client/mod.rs`, `src/client/file.rs`. Ported from `replica_client.go` + `file/replica_client.go`.
**DECISION — buffered I/O:** Go's `ReplicaClient` uses `io.Reader`/`io.ReadCloser`;
the Rust trait takes `&[u8]` and returns `Vec<u8>`. The `(offset, size)` params on
`open_ltx_file` keep the partial-read fast-path (page-index tail) the restore uses,
so this is buffering, not lost capability. KEEP-scope L0 files are bounded; streaming
large snapshots is a noted follow-on (revisit at T10/T11 if a large-DB path needs it).
**Deferral 1 — timestamp preservation:** Go's file client `Chtimes` the file to the
LTX header timestamp so listings return accurate `CreatedAt`. We compute `created_at`
from the header in `write_ltx_file` but do **not** persist it as the file mtime
(needs the `filetime` crate or a libc call). Affects timestamp-based PITR only; the
TXID-based path is unaffected. Pick up with T7/T10 (log the dep then, rule 7).
**Deferral 2 — `LTXError` not-found wrapping:** `open_ltx_file` currently returns the
raw `Io(NotFound)` rather than `LTXError{op:"open",…}`. NotFound is preserved (so
`is_auto_recoverable` still works); the structured wrap lands when the full upstream
conformance suite (which asserts the `LTXError` type) is ported.
**Needs from human:** none — recorded for visibility; revisit before G3 (differential).

### 2026-05-30 — T2 — New runtime dependency `lz4_flex` (rule 7) + the NoChecksum finding
**Context:** `src/ltx.rs`, `Cargo.toml`. Ported from ltx@v0.5.1 `encoder.go`/`decoder.go`/`checksum.go`.
**Dependency (AGENTS.md rule 7):** LTX page blocks are LZ4-**frame** compressed
(upstream uses `github.com/pierrec/lz4/v4`). Added **`lz4_flex` 0.11**
(`features=["frame"]`) — pure-Rust, no CGo, preserving the single-static-binary
goal (D-4 rationale). Verified interoperable with upstream frames: all six golden
L0 files decode and their CRC64-ISO file checksums verify **byte-exact**. CRC64 is
hand-rolled to match Go `crc64.MakeTable(crc64.ISO)` (poly `0xD800…`), proven by
both a known check vector and the golden files.
**Finding (not an ambiguity):** real litestream L0 WAL-segment LTX files set
`HeaderFlagNoChecksum`, so the rolling post-apply checksum is **not** tracked at
the LTX layer (it lives at the DB layer). `decode_file` always verifies the file
checksum and verifies the post-apply checksum only when tracking is on. Asserted
in the golden test so a future format change is caught.
**Conservative choice / status:** none needed — byte-exact golden verification is
the proof (rule 3). **Needs from human:** none; recorded for dep visibility (rule 7).

### 2026-05-29 — T1 — Pre-existing T4 test failure blocks the repo-wide `cargo test --all` gate (NOT a T1 issue)
**Context:** `tests/litestream_helpers.rs:170` `test_ltx_dir_normalizes_like_path_join`
asserts `ltx_dir("foo/") == "foo/ltx"` (and the doubled-separator / level-dir
variants). The current `ltx_dir` / `ltx_level_dir` in `src/lib.rs:299-308` use
naive `format!("{}/ltx", root)`, so `ltx_dir("foo/")` yields `"foo//ltx"`. This
is a faithful-port divergence from Go's `path.Join` (litestream.go:184-197), which
CLEANS the result. It is a **T4** concern (`lib.rs` path helpers), not T1 (`wal.rs`).
**Confirmed pre-existing:** the test fails on the baseline commit `b1218b0` with my
`src/wal.rs` change stashed, so T1 neither introduced nor can resolve it.
**Impact on T1:** the T1 module gate is green in isolation — `wal.rs` is 14/14
(ported `wal_reader_test.go` cases + byte-exact golden `sample.wal`), and
fmt/clippy/build/guards all pass. But the *repo-wide* `cargo test --all` is RED
solely because of this T4 test, so a strict reading of "all five gate commands
green" is not met by the whole tree.
**Conservative choice taken:** STOPPED — did **not** touch the failing test or the
`ltx_dir`/`ltx_level_dir` implementation (out of T1 scope; AGENTS.md rules 1 & 8
forbid weakening a test or straying out of KEEP scope). Logged here so the T4
fixer makes `ltx_dir`/`ltx_level_dir`/`ltx_file_path` normalize like Go
`path.Join` (collapse repeated separators, resolve `.`/`..`, strip trailing slash).
**Needs from human:** route the fix to T4 (path helpers in `src/lib.rs`). No T1
action required; this entry only explains why the aggregate `cargo test --all`
boolean is reported `false` in the T1 result.

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
