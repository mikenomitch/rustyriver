# PROGRESS.md — rustyriver

Single source of truth for task status. **Check a box only when that task's gate
is green in CI from a clean checkout** (see `AGENTS.md`). Tasks within a wave run
in parallel; gates between waves are hard stops.

> **CI substitution note (this environment):** there is no GitHub remote/Actions
> runner here, so the faithful proxy for "green CI from a clean checkout" is a
> gate run against a **fresh `git` clone of the repo** on this machine, which has
> the real `litestream` binary (built from tag v0.5.11), `sqlite3`, `go`, and
> Docker/MinIO. A box is checked only after that clean-checkout gate passes.

## Wave 0 — scaffold
- [x] **T0** Repo scaffold: Cargo, CI (§6.5), `AGENTS.md`/`PLAN.md`/`PROGRESS.md`/`OPEN_QUESTIONS.md`, vendor upstream @ `v0.5.11` into `reference/` (read-only), empty module + test files, `db_equal` oracle, `scripts/capture-golden.sh`. — *Done 2026-05-29: gate green (fmt/clippy/build/test); litestream v0.5.11 + ltx v0.5.1 vendored; `db_equal` A/B oracle self-tested; golden fixtures captured (L0 replica + WAL); real binary on PATH; U-1 (hand-roll) & U-2 (L0 restore) resolved.*

## Wave 1 — format foundation  → **GATE G1 (format) before Wave 2**
- [x] **T1** `wal.rs` — WAL header/frame parse + SQLite checksums. Port `wal_reader_test.go`. *(dep: T0)* — *Done 2026-05-29 (workflow porter→reviewer→fixer): 14/14 ported `wal_reader_test.go` cases + the golden `sample.wal` decodes with every frame's SQLite checksum (salt-rotation) byte-exact.*
- [x] **T2** `ltx.rs` — LTX read/write, TXID, framing, checksums. Port `v3_test.go`. **D-6 spike** (crate vs hand-roll) + produce `reference/ltx-format.md` + **L0-restore spike** (Risk R-3). *(dep: T0)* — *Done 2026-05-30 (inline): hand-rolled CRC64-ISO + `lz4_flex` frame codec; **all 6 golden L0 `.ltx` files decode and their file checksums verify byte-exact**; encode↔decode round-trip; `reference/ltx-format.md` written; D-6=hand-roll & U-2=L0-restore resolved. Dep `lz4_flex` logged in OPEN_QUESTIONS (rule 7).*
- [ ] **T3** `replica_url.rs` — parse `s3://`, `file://`. Port `replica_url_test.go`. *(dep: T0)* — **NEXT (only remaining Wave 1 task).**
- [x] **T4** `error.rs` + `lib.rs` public-API skeleton + helpers. Port `litestream_test.go` helpers. *(dep: T0)* — *Done 2026-05-29 (workflow): thiserror model + `TXID`/`Pos`/`Checksum`/`CHECKSUM_FLAG`/WAL+SQLite constants + Go `path.Join`-faithful `ltx_dir`/`path_clean` (incl. `..` backtracking); 24 ported `litestream_test.go` helper tests pass.*
- [x] **G1 (format)** wal + ltx unit **and golden** vectors pass byte-exact. — *Met 2026-05-30, re-verified from a fresh `git clone` @ `50bb1ec`: `cargo test --all` = 46 pass / 0 fail (incl. byte-exact WAL + LTX golden); fmt + clippy (`--all-targets --all-features -D warnings`) + guards all green. NOTE: T3 (`replica_url.rs`) is still pending; it does not feed G1's wal/ltx criterion but must land before Wave 1 is fully complete.*

## Wave 2 — core
- [ ] **T5** `client/mod.rs` — `ReplicaClient` trait + generic conformance suite. Port `replica_client_test.go`. *(dep: T2)*
- [ ] **T8** `store.rs` — snapshot/TXID bookkeeping + retention. Port `store_test.go`. *(dep: T2)*
- [ ] **T9** `db.rs` — checkpoint takeover, LTX capture loop, snapshot-on-continuity-break, clean shutdown. Port `db_test.go`, `db_internal_test.go`, `db_shutdown_test.go`. *(dep: T1, T2)*

## Wave 3 — clients
- [ ] **T6** `client/file.rs` — passes conformance suite. Port `file/` tests. *(dep: T5)*
- [ ] **T7** `client/object_store.rs` (S3/R2) — passes conformance suite vs MinIO. Port `s3/` tests. *(dep: T5)*

## Wave 4 — replica  → **GATE G2 (round-trip)**
- [ ] **T10** `replica.rs` — single-replica sync loop + restore. Port `replica_test.go`, `replica_internal_test.go`. *(dep: T8, T9, T5)*
- [ ] **G2** open→replicate→restore reproduces source (Oracle A; B where applicable) via file client.

## Wave 5 — resilience + failover
- [ ] **T11** Integration suite: replicate↔restore vs file + MinIO; crash-in-the-middle; snapshot-on-continuity-break; retention GC. *(dep: T6, T7, T10)*
- [ ] **T12** Property tests (proptest): random txns → replicate → restore == source. *(dep: T10)*
- [ ] **T14** Fault injection: truncated LTX, partial multipart upload, missed frames, clock skew. *(dep: T11)*
- [ ] **T15** `leaser.rs` — object-storage lease acquire/renew/standby + fencing. Port `leaser.go` (+ `heartbeat.go`). *(dep: T7)*

## Wave 6 — differential  → **GATE G3 (= "M1 correct")**
- [ ] **T13** Differential vs real `litestream` v0.5.11: D1 (write→litestream restore), D2 (litestream write→our restore), D3 (two restorers, byte-identical). *(dep: T11)*
- [ ] **G3** D1, D2 pass (Oracle A); D3 byte-identical (Oracle B), both directions.

## Wave 7 — hardening  → **GATE G4 (resilience)**
- [ ] **T16** `fuzz/` targets for LTX + WAL parsers; adversarial recovery sweep. *(dep: T13, T14)*
- [ ] **G4** property + fault-injection + fuzz green.

## Wave 8 — release  → **GATE G5 (release)**
- [ ] **T17** Docs: README, embedding guide, runnable example (open→replicate→simulate loss→restore→verify vs MinIO), API stabilization, coverage report (every Go test → ported/deferred/dropped). *(dep: all)*
- [ ] **G5** all green in CI from clean checkout; example runs; coverage report committed; `OPEN_QUESTIONS.md` clear of correctness blockers.

---
**Done = all boxes checked, G1–G5 green, and the `PLAN.md` §8 Definition of Done satisfied.**
