# Golden fixtures — provenance (IMMUTABLE)

Captured from the **real** upstream tooling. DO NOT edit or regenerate from
rustyriver. A mismatch means rustyriver is wrong (AGENTS.md rule 3 / PLAN.md §6.2).

| Source tool | Version | Provenance |
|-------------|---------|-----------|
| `litestream` | built from tag **v0.5.11** (commit `016c3687…`) | `reference/litestream-go` |
| `sqlite3` | 3.51.0 | system |

## Files

- **`replica/`** — a full file-replica tree produced by `litestream replicate -once`
  (deterministic, no background timing). Layout: `ltx/<level>/<minTXID>-<maxTXID>.ltx`.
  Contents are **L0-only** (level `0`): a snapshot at TXID 1 plus 5 single-txn L0
  files (TXIDs 2–6). The real binary restores this tree and the result equals the
  source under Oracle A — see the U-2 resolution in `OPEN_QUESTIONS.md`.
  - SQL sequence: `PRAGMA journal_mode=WAL`; `CREATE TABLE kv(k TEXT PRIMARY KEY,
    v TEXT NOT NULL)`; seed `('a','1'),('b','2'),('c','3')`; then 5 iterations each
    `INSERT ('k$i','v$i')` + `UPDATE kv SET v='upd$i' WHERE k='a'`, each flushed
    with `replicate -once`.

- **`sample.wal`** — a raw **SQLite** WAL file (16,512 bytes) for the `wal.rs`
  parser (T1). Produced by sqlite3 3.51.0 in WAL mode with `wal_autocheckpoint=0`,
  20 inserts + an update across 2 commits, copied while a second connection held a
  read transaction (so the close-time checkpoint could not truncate it). Header
  magic `0x377f0682` (big-endian salt variant), page size 4096.

## How rustyriver uses these
Reader tests **decode** these bytes and assert structural fields (TXIDs, page
numbers, checksums, frame counts). Writer byte-fidelity is **not** asserted by
comparison against these files (LTX headers embed timestamps and are not
bit-reproducible) — it is proven by differential test D1 against the real binary
(PLAN.md §6.3).

## Reproduce
`scripts/capture-golden.sh` regenerates an equivalent tree from a `litestream`
binary built at tag v0.5.11. The bytes are not identical run-to-run (timestamps),
but the structure and the round-trip property are.
