//! property_roundtrip — T12: a property test (proptest) that replicate→restore
//! reproduces the source database for *random* transaction sequences.
//!
//! This is **Oracle A** (`scripts/db_equal.sh A`, PLAN.md §6.1) lifted to a
//! generative setting: for each randomly-generated sequence of SQL transactions,
//! we open a managed [`Db`], capture every transaction into L0 LTX files, upload
//! them to a file replica, restore into a fresh path, and assert the restored DB
//! is logically identical to the source. The invariant under test — *a restore
//! of a fully-synced replica reproduces the source* — is the heart of the whole
//! system, and random operation mixes (insert / update / delete / DDL / occasional
//! checkpoints) exercise WAL shapes the hand-written integration tests don't.
//!
//! ## Determinism / speed
//! proptest needs deterministic, reasonably-fast cases. The pipeline is async and
//! the oracle is a subprocess, so each case builds a small current-thread Tokio
//! runtime and `block_on`s the upload/restore. The seed budget is bounded
//! (`PROPTEST_CASES` cases, ≤ `MAX_OPS` operations each) to keep `cargo test`
//! fast while still covering a wide operation space. Values are drawn from the
//! proptest RNG only (no `randomblob`/time), so a failing case is reproducible
//! from its seed.
//!
//! The whole suite self-skips (never fails) if `sqlite3` is not on PATH, since
//! the oracle requires it.

use proptest::prelude::*;
use rusqlite::Connection;
use rustyriver::client::file::FileReplicaClient;
use rustyriver::db::{CheckpointMode, Db};
use rustyriver::replica::Replica;
use std::path::Path;
use std::process::Command;

/// Number of random sequences to try. Bounded for a fast suite (each case opens a
/// real SQLite DB, runs the full capture→upload→restore pipeline, and shells out
/// to the oracle twice).
const PROPTEST_CASES: u32 = 24;
/// Maximum number of transactions per generated sequence.
const MAX_OPS: usize = 14;

/// Absolute path to the `db_equal.sh` oracle.
fn db_equal_script() -> String {
    format!("{}/scripts/db_equal.sh", env!("CARGO_MANIFEST_DIR"))
}

/// Runs `db_equal.sh A <a> <b>`; Ok on exit 0, else the captured output.
fn db_equal_a(a: &Path, b: &Path) -> Result<(), String> {
    let out = Command::new("bash")
        .arg(db_equal_script())
        .arg("A")
        .arg(a)
        .arg(b)
        .output()
        .map_err(|e| format!("spawn db_equal.sh: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(format!(
            "db_equal A failed:\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
        ))
    }
}

/// True if `bin` resolves on PATH.
fn has_bin(bin: &str) -> bool {
    Command::new(bin)
        .arg("--help")
        .output()
        .map(|o| o.status.success() || o.status.code().is_some())
        .unwrap_or(false)
}

/// Opens an application writer connection in WAL mode.
fn open_writer(path: &Path) -> Connection {
    let c = Connection::open(path).unwrap();
    c.busy_timeout(std::time::Duration::from_secs(5)).unwrap();
    let mode: String = c
        .query_row("PRAGMA journal_mode=WAL", [], |r| r.get(0))
        .unwrap();
    assert_eq!(mode, "wal");
    c
}

/// One generated transaction against a fixed `kv(k INTEGER PRIMARY KEY, v TEXT)`
/// schema plus occasional structural events. Keeping the schema fixed makes value
/// generation simple while the *operation mix* (and thus the WAL frame shapes,
/// page growth, and checkpoint interleaving) stays random.
#[derive(Debug, Clone)]
enum Op {
    /// `INSERT OR REPLACE` a key with a text value.
    Upsert { key: i64, val: String },
    /// `DELETE` a key (may match nothing — still a valid, empty-ish txn).
    Delete { key: i64 },
    /// `UPDATE` all rows whose key is below a bound (range write).
    UpdateBelow { bound: i64, val: String },
    /// Insert `n` rows in a single transaction (multi-page growth).
    BulkInsert { start: i64, n: i64, val: String },
    /// Create an extra table and seed one row (DDL + schema change).
    CreateAux { idx: u8 },
    /// Force a `PRAGMA wal_checkpoint(TRUNCATE)` (continuity break / salt reset).
    CheckpointTruncate,
}

/// proptest strategy for a single `Op`. Weighted toward data mutations.
fn op_strategy() -> impl Strategy<Value = Op> {
    let val = "[a-zA-Z0-9]{0,24}";
    prop_oneof![
        6 => (0i64..40, val).prop_map(|(key, v)| Op::Upsert { key, val: v }),
        3 => (0i64..40).prop_map(|key| Op::Delete { key }),
        3 => (0i64..40, val).prop_map(|(bound, v)| Op::UpdateBelow { bound, val: v }),
        3 => (0i64..30, 1i64..8, val).prop_map(|(start, n, v)| Op::BulkInsert { start, n, val: v }),
        1 => (0u8..3).prop_map(|idx| Op::CreateAux { idx }),
        2 => Just(Op::CheckpointTruncate),
    ]
}

/// Applies one `Op`. DDL/data ops run on the application `writer` connection;
/// `CheckpointTruncate` drives the managed `Db`.
fn apply_op(op: &Op, writer: &Connection, db: &mut Db) {
    match op {
        Op::Upsert { key, val } => {
            writer
                .execute(
                    "INSERT OR REPLACE INTO kv (k, v) VALUES (?1, ?2)",
                    (key, val),
                )
                .unwrap();
        }
        Op::Delete { key } => {
            writer
                .execute("DELETE FROM kv WHERE k = ?1", [key])
                .unwrap();
        }
        Op::UpdateBelow { bound, val } => {
            writer
                .execute("UPDATE kv SET v = ?2 WHERE k < ?1", (bound, val))
                .unwrap();
        }
        Op::BulkInsert { start, n, val } => {
            let tx_sql: String = (0..*n)
                .map(|i| {
                    format!(
                        "INSERT OR REPLACE INTO kv (k, v) VALUES ({}, '{}');",
                        start + i,
                        val
                    )
                })
                .collect();
            // Wrap as a single transaction so it is one captured TXID.
            writer
                .execute_batch(&format!("BEGIN; {tx_sql} COMMIT;"))
                .unwrap();
        }
        Op::CreateAux { idx } => {
            let i = (*idx % 3) as usize;
            writer
                .execute_batch(&format!(
                    "CREATE TABLE IF NOT EXISTS aux{i} (id INTEGER PRIMARY KEY, label TEXT);\
                     INSERT INTO aux{i} (id, label) VALUES (1, 'seed-{i}') \
                       ON CONFLICT(id) DO UPDATE SET label='seed-{i}';"
                ))
                .unwrap();
        }
        Op::CheckpointTruncate => {
            db.checkpoint(CheckpointMode::Truncate).unwrap();
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: PROPTEST_CASES,
        // Shrinking re-runs the (heavy) pipeline; cap it so a failure still
        // reports quickly. The seed alone reproduces any case.
        max_shrink_iters: 24,
        failure_persistence: None,
        ..ProptestConfig::default()
    })]

    /// For a random sequence of transactions, a fully-synced replica restores to
    /// a database logically identical to the source (Oracle A).
    #[test]
    fn random_txns_roundtrip_restores_source(ops in prop::collection::vec(op_strategy(), 1..=MAX_OPS)) {
        // Skip (don't fail) when the oracle's sqlite3 is unavailable.
        if !has_bin("sqlite3") {
            return Ok(());
        }

        let dir = tempfile::tempdir().unwrap();
        let src_path = dir.path().join("source.db");
        let replica_root = dir.path().join("replica");
        let restored_path = dir.path().join("restored.db");

        // A current-thread runtime: proptest bodies are synchronous, and the
        // upload/restore halves are async. block_on keeps each case deterministic.
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        let db = Db::open(&src_path).unwrap();
        let writer = open_writer(&src_path);
        let client = FileReplicaClient::new(replica_root.to_string_lossy().into_owned());
        let mut replica = Replica::new(db, client);

        // Base schema (one captured TXID).
        writer
            .execute_batch("CREATE TABLE kv (k INTEGER PRIMARY KEY, v TEXT)")
            .unwrap();
        replica.db_mut().unwrap().sync().unwrap();
        rt.block_on(replica.sync()).unwrap();

        for op in &ops {
            apply_op(op, &writer, replica.db_mut().unwrap());
            // Capture then upload after every op (SyncAndWait ordering). Syncing
            // an idle transaction is a valid no-op (db.rs covers it).
            replica.db_mut().unwrap().sync().unwrap();
            rt.block_on(replica.sync()).unwrap();
        }

        // The replica must have caught up to the database position.
        let dpos = replica.db_mut().unwrap().pos().unwrap();
        prop_assert_eq!(
            replica.pos().txid, dpos.txid,
            "replica caught up to the DB (txid {})", dpos.txid.0
        );

        // Restore into a fresh path.
        rt.block_on(replica.restore(&restored_path)).expect("restore");

        // Release the source WAL before the oracle checkpoints it.
        drop(replica);
        drop(writer);

        // Oracle A: restored == source.
        if let Err(e) = db_equal_a(&src_path, &restored_path) {
            return Err(TestCaseError::fail(format!(
                "Oracle A mismatch for ops {ops:?}: {e}"
            )));
        }
    }
}
