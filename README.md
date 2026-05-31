# rustyriver

An embeddable Rust library for **streaming replication of a SQLite database to
object storage**, with point-in-time restore and object-storage lease fencing.

This is an experimental port of [Litestream v0.5](https://github.com/benbjohnson/litestream)
to Rust but uses an an in-process async API instead of a sidecar binary.
Credit to benbjohnson and [Litestream](https://github.com/benbjohnson/litestream) for the original implementation.

## Embedding guide

### Core API

```rust
// 1. Open a SQLite database for managed replication.
let mut db = rustyriver::Db::open("events.db")?;

// 2. Connect it to a replica destination (local file or S3/R2/MinIO).
let client = rustyriver::FileReplicaClient::new("/mnt/replica");
let mut replica = rustyriver::Replica::new(db, client);

// 3. After each application write: capture the WAL segment, then upload.
db_ref.sync()?;               // db.sync() captures WAL → local LTX file
replica.sync().await?;        // uploads new LTX files to the replica

// 4. On disaster: restore from the replica to a fresh path.
let client = rustyriver::FileReplicaClient::new("/mnt/replica");
rustyriver::restore(&client, "events-restored.db", rustyriver::TXID(0)).await?;

// 5. Acquire an object-storage lease for single-primary fencing.
// let leaser = rustyriver::S3Leaser::new(store, "lock.json".into(), ttl);
// let lease = leaser.acquire_lease().await?;
```

### Walkthrough (mirrors `examples/embed.rs`)

1. **Open** — `Db::open(path)` enables WAL mode, disables auto-checkpointing,
   acquires a long-running read lock to prevent external checkpoints, and
   creates the litestream metadata directory next to the file.

2. **Replicate** — After your application commits a transaction, call
   `db.sync()` (the local capture half: WAL frames → LTX file in the metadata
   directory), then `replica.sync().await` (the upload half: new LTX files →
   `FileReplicaClient` or `ObjectStoreClient`).  Both steps are idempotent.

3. **Simulate host loss** — Call `replica.into_db()` to recover the `Db`, then
   `db.close()` to release the read lock cleanly.  Deleting the database file
   simulates complete data loss.

4. **Restore** — `rustyriver::restore(&client, output_path, TXID(0))` downloads
   the LTX chain from the replica, merges it (compactor semantics: last-write-
   wins per page, final `Commit` bounds the image), reconstructs the SQLite
   database file, and atomically renames it into place.  `TXID(0)` means the
   most-recent state; pass a specific `TXID(n)` for point-in-time restore.

5. **Verify** — Open the restored file with any SQLite connection and assert
   your rows are present.

Run the full example with no external services:

```sh
cargo run --example embed
```

### KEEP scope (L0-only, single replica)

| What is in scope | What is NOT |
|---|---|
| L0-only replication (no compaction) | L1+ compaction levels |
| Single replica per `Db` | Multi-replica fan-out |
| File client (`FileReplicaClient`) | GCS / Azure / SFTP / WebDAV |
| S3/R2/MinIO client (`ObjectStoreClient`, `s3` feature) | — |
| Object-storage lease fencing (`S3Leaser`) | — |
| Point-in-time restore by TXID | Restore by timestamp (planned) |

rustyriver is a **Rust reimplementation of Litestream v0.5** (Apache-2.0).
See [NOTICE](NOTICE) for attribution.

## Repository map

| Path | Role |
|------|------|
| [`PLAN.md`](PLAN.md) | The full build spec: scope (§2), architecture (§4), task DAG (§5), verification (§6), gates G1–G5. |
| [`AGENTS.md`](AGENTS.md) | Non-negotiable operating rules + the gate command. Read before any work. |
| [`PROGRESS.md`](PROGRESS.md) | Task checklist T0–T17, kept truthful. |
| [`OPEN_QUESTIONS.md`](OPEN_QUESTIONS.md) | Settled decisions (D-1…D-10), resolved unknowns (U-1, U-2), escalation log. |
| `reference/` | Vendored upstream, **read-only** ground truth: litestream @ `v0.5.11`, ltx @ `v0.5.1`. |
| `tests/fixtures/golden/` | Immutable byte fixtures captured from the real binary + sqlite3. |
| `scripts/` | `db_equal` equality oracle (§6.1), `capture-golden.sh`, anti-gaming `guards.sh`. |
| `examples/embed.rs` | End-to-end disaster-recovery example (no external services). |
| `src/` | The library (`wal`, `ltx`, `db`, `store`, `replica`, `replica_url`, `leaser`, `client/`). |

## The gate (must be green before any task is done)

```sh
cargo fmt --all --check
cargo clippy --all-targets --all-features -- -D warnings
cargo build --all-targets
cargo test --all
```

## License

Apache-2.0. See [`LICENSE`](LICENSE) and [`NOTICE`](NOTICE) (attributes Litestream
and the LTX format).
