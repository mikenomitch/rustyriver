//! integration_resilience — T11: the resilience half of the round-trip gate.
//!
//! Where `integration_file.rs` proves the happy-path G2 round-trip, this file
//! proves replicate↔restore survives the messy cases the real world throws at a
//! replication system, using the file `ReplicaClient` + the **Oracle A**
//! (`scripts/db_equal.sh A`) logical-equality check:
//!
//! * **crash-in-the-middle** — the managed `Db` is dropped without a clean
//!   shutdown (simulating a process kill), a fresh `Db` reopens the same file and
//!   continues capturing, and a restore still reproduces the final source.
//! * **restore-and-replicate-after-data-loss** — the issue #781 scenario, ported
//!   from `replica_test.go:111-255 TestReplica_RestoreAndReplicateAfterDataLoss`:
//!   replicate, hard-recover from backup to an *earlier* state, write new data,
//!   replicate again, restore again, and verify the new data survived. This is
//!   the canonical snapshot-on-continuity-break integration test and exercises
//!   `Replica::check_database_behind_replica`.
//! * **snapshot-on-continuity-break via a TRUNCATE checkpoint** — drive a real
//!   `PRAGMA wal_checkpoint(TRUNCATE)` between writes (the salt-reset path through
//!   `Db::verify`) and confirm every captured TXID still restores cleanly.
//!
//! Each test self-skips (never fails) when `sqlite3` is not on PATH, since the
//! Oracle needs it; the capture/replicate/restore code is exercised regardless.

use rusqlite::Connection;
use rustyriver::client::file::FileReplicaClient;
use rustyriver::client::ReplicaClient;
use rustyriver::db::{CheckpointMode, Db};
use rustyriver::replica::{self, Replica};
use rustyriver::TXID;
use std::path::Path;
use std::process::Command;

/// Absolute path to the `db_equal.sh` oracle.
fn db_equal_script() -> String {
    format!("{}/scripts/db_equal.sh", env!("CARGO_MANIFEST_DIR"))
}

/// Runs `db_equal.sh <mode> <a> <b>`; Ok on exit 0, else the captured output.
fn db_equal(mode: &str, a: &Path, b: &Path) -> Result<(), String> {
    let out = Command::new("bash")
        .arg(db_equal_script())
        .arg(mode)
        .arg(a)
        .arg(b)
        .output()
        .map_err(|e| format!("spawn db_equal.sh: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(format!(
            "db_equal {mode} failed:\nstdout: {}\nstderr: {}",
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

/// Opens an application writer connection in WAL mode (the host's own handle).
fn open_writer(path: &Path) -> Connection {
    let c = Connection::open(path).unwrap();
    c.busy_timeout(std::time::Duration::from_secs(5)).unwrap();
    let mode: String = c
        .query_row("PRAGMA journal_mode=WAL", [], |r| r.get(0))
        .unwrap();
    assert_eq!(mode, "wal");
    c
}

/// CRASH IN THE MIDDLE: drop the managed `Db` with no clean shutdown partway
/// through replication, reopen a fresh `Db` on the same file, keep writing &
/// replicating, then restore — the result must still equal the final source.
///
/// This mirrors a process kill: the read lock and connections vanish, a new
/// process re-`open`s the database, and `Db::open`'s `ensure_wal_exists` +
/// `verify`'s continuity-break detection recover the capture stream. The first
/// `sync()` on the reopened `Db` writes a fresh snapshot (no local LTX state
/// survives a meta-dir-less reopen view of pos), so the chain stays restorable.
#[tokio::test(flavor = "multi_thread")]
async fn crash_in_the_middle_then_reopen_and_restore() {
    if !has_bin("sqlite3") {
        eprintln!("skipping: sqlite3 not on PATH (required for Oracle A)");
        return;
    }

    let dir = tempfile::tempdir().unwrap();
    let src_path = dir.path().join("source.db");
    let replica_root = dir.path().join("replica");
    let restored_path = dir.path().join("restored.db");

    let writer = open_writer(&src_path);

    // ── Phase 1: open a managed Db, capture & replicate a few txns. ──
    {
        let db = Db::open(&src_path).unwrap();
        let client = FileReplicaClient::new(replica_root.to_string_lossy().into_owned());
        let mut replica = Replica::new(db, client);

        writer
            .execute_batch("CREATE TABLE kv (k TEXT PRIMARY KEY, v TEXT NOT NULL)")
            .unwrap();
        replica.db_mut().unwrap().sync().unwrap();
        replica.sync().await.unwrap();

        writer
            .execute_batch("INSERT INTO kv (k,v) VALUES ('a','1'),('b','2'),('c','3')")
            .unwrap();
        replica.db_mut().unwrap().sync().unwrap();
        replica.sync().await.unwrap();

        // ── CRASH: drop the replica (and its Db) WITHOUT calling close(). ──
        // No clean shutdown: the read lock + connections are abandoned, exactly
        // as if the process were killed here.
        drop(replica);
    }

    // ── Phase 2: a fresh process reopens the same database file. ──
    // The application connection survived (a separate handle); reopen the
    // managed Db and continue. The replica position is recomputed from the
    // remote on the next sync (Replica::sync clears+recomputes pos).
    {
        let db = Db::open(&src_path).unwrap();
        let client = FileReplicaClient::new(replica_root.to_string_lossy().into_owned());
        let mut replica = Replica::new(db, client);

        // Heal any continuity break left by the crash before resuming uploads
        // (the same call a host makes after open; issue #781 path).
        replica.check_database_behind_replica().await.unwrap();

        writer
            .execute_batch("UPDATE kv SET v='updated' WHERE k='a'")
            .unwrap();
        replica.db_mut().unwrap().sync().unwrap();
        replica.sync().await.unwrap();

        writer
            .execute_batch(
                "INSERT INTO kv (k,v) VALUES ('d','4'),('e','5'); DELETE FROM kv WHERE k='b';",
            )
            .unwrap();
        replica.db_mut().unwrap().sync().unwrap();
        replica.sync().await.unwrap();

        // Restore from the replica into a brand-new path.
        replica.restore(&restored_path).await.expect("restore");
        assert!(restored_path.exists(), "restored DB file created");

        drop(replica);
    }
    drop(writer);

    // Oracle A: the restored DB is logically identical to the (final) source.
    db_equal("A", &src_path, &restored_path)
        .expect("Oracle A: restored == source after crash-in-the-middle");
}

/// ISSUE #781 — restore to an earlier state, write new data, replicate, restore
/// again: the new data must survive. Faithful port of
/// `TestReplica_RestoreAndReplicateAfterDataLoss` (replica_test.go:111-255).
///
/// Without `check_database_behind_replica`, step 3's new write is silently
/// dropped: the fresh post-restore DB snapshots at TXID 1, but the replica's
/// recomputed position sits at the remote's higher MaxTXID, so the upload loop
/// (`pos+1 ..= db.pos`) never runs. The fix seeds the remote baseline so the
/// next sync snapshots forward and uploads.
#[tokio::test(flavor = "multi_thread")]
async fn restore_and_replicate_after_data_loss() {
    if !has_bin("sqlite3") {
        eprintln!("skipping: sqlite3 not on PATH");
        return;
    }

    let replica_dir = tempfile::tempdir().unwrap();
    let replica_root = replica_dir.path().to_string_lossy().into_owned();

    let db_dir = tempfile::tempdir().unwrap();
    let db_path = db_dir.path().join("db.sqlite");

    // ── Step 1: create initial data and replicate. ──
    {
        // The application writes the initial rows (mirrors the Go test opening a
        // plain *sql.DB first), then litestream takes over and snapshots them.
        let writer = open_writer(&db_path);
        writer
            .execute_batch("CREATE TABLE test(col1 INTEGER); INSERT INTO test VALUES (1);")
            .unwrap();

        let db = Db::open(&db_path).unwrap();
        let client = FileReplicaClient::new(replica_root.clone());
        let mut replica = Replica::new(db, client);

        replica.db_mut().unwrap().sync().unwrap();
        replica.sync().await.unwrap();

        // Clean shutdown of the managed Db + drop the writer handle.
        let db = replica.into_db().unwrap();
        db.close().unwrap();
        drop(writer);
    }

    // ── Step 2: hard recovery — wipe the db + meta dir, restore from backup. ──
    let meta_path = Db::meta_path_for_path(&db_path);
    std::fs::remove_file(&db_path).ok();
    std::fs::remove_file(format!("{}-wal", db_path.display())).ok();
    std::fs::remove_file(format!("{}-shm", db_path.display())).ok();
    std::fs::remove_dir_all(&meta_path).ok();

    {
        let client = FileReplicaClient::new(replica_root.clone());
        replica::restore(&client, &db_path, TXID(0))
            .await
            .expect("restore from backup");
    }

    // ── Step 3: reopen, insert new data (value=2), replicate. ──
    {
        let db = Db::open(&db_path).unwrap();
        let client = FileReplicaClient::new(replica_root.clone());
        let mut replica = Replica::new(db, client);

        // Issue #781 detection: the DB is behind the replica (fresh local L0 vs.
        // the remote's TXID). Seed the baseline so the next sync snapshots
        // forward.
        replica.check_database_behind_replica().await.unwrap();

        let writer = open_writer(&db_path);
        writer
            .execute_batch("INSERT INTO test VALUES (2);")
            .unwrap();

        replica.db_mut().unwrap().sync().unwrap();
        replica.sync().await.unwrap();

        let db = replica.into_db().unwrap();
        db.close().unwrap();
        drop(writer);
    }

    // ── Step 4: second hard recovery, restore to a path whose parent is absent. ──
    std::fs::remove_file(&db_path).ok();
    std::fs::remove_file(format!("{}-wal", db_path.display())).ok();
    std::fs::remove_file(format!("{}-shm", db_path.display())).ok();
    std::fs::remove_dir_all(&meta_path).ok();

    let restored_path = db_dir.path().join("restored").join("db.sqlite");
    {
        let client = FileReplicaClient::new(replica_root.clone());
        replica::restore(&client, &restored_path, TXID(0))
            .await
            .expect("second restore (parent dir must be created)");
    }

    // ── Step 5: verify the new data (value=2) survived. ──
    let conn = Connection::open(&restored_path).unwrap();
    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM test", [], |r| r.get(0))
        .unwrap();
    assert_eq!(count, 2, "expected 2 rows (1 and 2) in restored database");
    let exists: bool = conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM test WHERE col1 = 2)",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(
        exists,
        "new data (value=2) was not replicated — this is the bug in issue #781"
    );
}

/// SNAPSHOT-ON-CONTINUITY-BREAK: force a real `PRAGMA wal_checkpoint(TRUNCATE)`
/// between writes (which resets the WAL salts, the salt-reset branch of
/// `Db::verify`) and confirm the full chain still restores to the final source
/// under Oracle A. The capture loop must either continue incrementally or
/// re-snapshot — either way the restore stays correct.
#[tokio::test(flavor = "multi_thread")]
async fn checkpoint_truncate_continuity_break_still_restores() {
    if !has_bin("sqlite3") {
        eprintln!("skipping: sqlite3 not on PATH");
        return;
    }

    let dir = tempfile::tempdir().unwrap();
    let src_path = dir.path().join("source.db");
    let replica_root = dir.path().join("replica");
    let restored_path = dir.path().join("restored.db");

    let db = Db::open(&src_path).unwrap();
    let writer = open_writer(&src_path);
    let client = FileReplicaClient::new(replica_root.to_string_lossy().into_owned());
    let mut replica = Replica::new(db, client);

    macro_rules! step {
        ($sql:expr) => {{
            writer.execute_batch($sql).unwrap();
            replica.db_mut().unwrap().sync().unwrap();
            replica.sync().await.unwrap();
        }};
    }

    step!("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)");
    step!("INSERT INTO t (v) VALUES ('one'),('two')");

    // Force a TRUNCATE checkpoint: this restarts the WAL and rotates the salts,
    // a continuity break the next verify() must detect.
    replica
        .db_mut()
        .unwrap()
        .checkpoint(CheckpointMode::Truncate)
        .unwrap();

    step!("INSERT INTO t (v) VALUES ('three')");
    step!("UPDATE t SET v='TWO' WHERE id=2");
    // A second checkpoint mid-stream for good measure.
    replica
        .db_mut()
        .unwrap()
        .checkpoint(CheckpointMode::Truncate)
        .unwrap();
    step!("INSERT INTO t (v) VALUES ('four'),('five')");

    let dpos = replica.db_mut().unwrap().pos().unwrap();
    assert!(dpos.txid.0 >= 5, "at least 5 transactions captured");

    // Every captured TXID is on the replica.
    let files = replica.client.ltx_files(0, TXID(0), false).await.unwrap();
    assert!(
        files.len() as u64 >= dpos.txid.0,
        "every captured TXID is on the replica (got {} files, txid={})",
        files.len(),
        dpos.txid
    );

    replica.restore(&restored_path).await.expect("restore");
    drop(replica);
    drop(writer);

    db_equal("A", &src_path, &restored_path)
        .expect("Oracle A: restored == source across TRUNCATE checkpoints");
}
