//! faults_inject — T14: fault injection on the replica, then restore.
//!
//! The contract under test (PLAN.md §6.4 resilience / G4): when the replica is
//! damaged — a **truncated LTX file**, an **empty (partial) upload**, **corrupt
//! bytes**, or **missing/missed frames** (a deleted mid-chain L0 file) — a
//! restore must *always* either
//!   * yield a **valid SQLite database at a valid TXID ≤ the last durable TXID**, or
//!   * fail with a **clean error**.
//!
//! In every case it must **never panic** and never produce a corrupt output
//! database.
//!
//! Two of the cases are direct ports of upstream `TestReplica_Restore_InvalidFileSize`
//! (replica_test.go:797-858): an empty snapshot file and a sub-header-sized
//! ("truncated") snapshot file must both make `Restore` fail with an
//! "invalid ltx file" error. The rest inject byte-level corruption and chain gaps
//! against a real replicated L0 chain and assert the recovery contract, using the
//! **Oracle A** (`scripts/db_equal.sh A`) where a successful restore is expected.
//!
//! The fault-injection cases that do not need the oracle run unconditionally
//! (they assert error/typing/no-panic); the recovery cases that compare DB
//! contents self-skip (never fail) when `sqlite3` is not on PATH.

use rusqlite::Connection;
use rustyriver::client::file::FileReplicaClient;
use rustyriver::client::ReplicaClient;
use rustyriver::db::Db;
use rustyriver::ltx::{self, FileInfo};
use rustyriver::replica::{self, Replica, SNAPSHOT_LEVEL};
use rustyriver::{ltx_file_path, Error, TXID};
use std::path::{Path, PathBuf};
use std::process::Command;

// ── oracle / env helpers ────────────────────────────────────────────────────

fn db_equal_script() -> String {
    format!("{}/scripts/db_equal.sh", env!("CARGO_MANIFEST_DIR"))
}

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

fn has_bin(bin: &str) -> bool {
    Command::new(bin)
        .arg("--help")
        .output()
        .map(|o| o.status.success() || o.status.code().is_some())
        .unwrap_or(false)
}

fn open_writer(path: &Path) -> Connection {
    let c = Connection::open(path).unwrap();
    c.busy_timeout(std::time::Duration::from_secs(5)).unwrap();
    let mode: String = c
        .query_row("PRAGMA journal_mode=WAL", [], |r| r.get(0))
        .unwrap();
    assert_eq!(mode, "wal");
    c
}

/// True if the restored file at `path` is a valid SQLite database (opens and
/// passes `PRAGMA integrity_check`). Used to assert the "valid DB" half of the
/// contract directly in-process (no sqlite3 binary needed).
fn restored_db_is_valid(path: &Path) -> bool {
    let Ok(conn) = Connection::open(path) else {
        return false;
    };
    match conn.query_row("PRAGMA integrity_check", [], |r| r.get::<_, String>(0)) {
        Ok(s) => s == "ok",
        Err(_) => false,
    }
}

/// Returns the user-table row count of a restored DB (asserts a non-trivial DB).
fn count_rows(path: &Path, table: &str) -> i64 {
    let conn = Connection::open(path).unwrap();
    conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
        .unwrap()
}

// ── a real replicated L0 chain to damage ────────────────────────────────────

/// Holds a TempDir-rooted replica with a known-good L0 chain plus the
/// point-in-time `VACUUM INTO` snapshot taken at `safe_txid` (a TXID strictly
/// before the tail we will damage).
struct Fixture {
    _dir: tempfile::TempDir,
    replica_root: String,
    /// Database position (TXID) after all writes — the last durable TXID.
    last_txid: TXID,
    /// A safe earlier TXID whose tail we keep intact for point-in-time recovery.
    safe_txid: TXID,
    /// VACUUM INTO snapshot of the source exactly at `safe_txid`.
    safe_snapshot: PathBuf,
}

/// Builds a real replicated database: a `t(id,v)` table with several
/// transactions, each captured + uploaded through the file client. Captures a
/// point-in-time snapshot of the source at `safe_txid` for later comparison.
async fn build_fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let src_path = dir.path().join("source.db");
    let replica_root = dir.path().join("replica").to_string_lossy().into_owned();
    let safe_snapshot = dir.path().join("safe.db");

    let db = Db::open(&src_path).unwrap();
    let writer = open_writer(&src_path);
    let client = FileReplicaClient::new(replica_root.clone());
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
    step!("INSERT INTO t (v) VALUES ('three')");
    // Snapshot the source as-of this point — the "safe" recovery target.
    let safe_txid = replica.db_mut().unwrap().pos().unwrap().txid;
    writer
        .execute(
            "VACUUM INTO ?1",
            [safe_snapshot.to_string_lossy().to_string()],
        )
        .unwrap();

    // More transactions whose tail we will damage.
    step!("UPDATE t SET v='TWO' WHERE id=2");
    step!("INSERT INTO t (v) VALUES ('four'),('five')");
    step!("DELETE FROM t WHERE id=1");

    let last_txid = replica.db_mut().unwrap().pos().unwrap().txid;
    assert!(
        last_txid > safe_txid,
        "tail TXIDs exist past the safe point"
    );

    drop(replica);
    drop(writer);

    Fixture {
        _dir: dir,
        replica_root,
        last_txid,
        safe_txid,
        safe_snapshot,
    }
}

/// On-disk path of an L0 file in a fixture replica.
fn l0_path(root: &str, txid: TXID) -> String {
    ltx_file_path(root, 0, txid, txid)
}

// ── Ported: TestReplica_Restore_InvalidFileSize (replica_test.go:797-858) ────

/// A `ReplicaClient` whose snapshot listing reports a single file at
/// `SNAPSHOT_LEVEL` with a caller-chosen `size`, mirroring the upstream
/// `mock.ReplicaClient` used by `TestReplica_Restore_InvalidFileSize`. All other
/// levels are empty. `open_ltx_file` returns whatever bytes were stored (used
/// only by the empty-file case, which never reaches a read because the size
/// guard fires first).
struct BadSizeClient {
    size: i64,
}

#[async_trait::async_trait]
impl ReplicaClient for BadSizeClient {
    fn type_name(&self) -> &str {
        "bad-size-mock"
    }
    async fn ltx_files(
        &self,
        level: i32,
        _seek: TXID,
        _md: bool,
    ) -> rustyriver::Result<Vec<FileInfo>> {
        if level == SNAPSHOT_LEVEL {
            Ok(vec![FileInfo {
                level: SNAPSHOT_LEVEL,
                min_txid: TXID(1),
                max_txid: TXID(10),
                size: self.size,
                ..Default::default()
            }])
        } else {
            Ok(Vec::new())
        }
    }
    async fn open_ltx_file(
        &self,
        _level: i32,
        _min: TXID,
        _max: TXID,
        _off: i64,
        _size: i64,
    ) -> rustyriver::Result<Vec<u8>> {
        Ok(Vec::new())
    }
    async fn write_ltx_file(
        &self,
        level: i32,
        min_txid: TXID,
        max_txid: TXID,
        data: &[u8],
    ) -> rustyriver::Result<FileInfo> {
        Ok(FileInfo {
            level,
            min_txid,
            max_txid,
            size: data.len() as i64,
            ..Default::default()
        })
    }
    async fn delete_ltx_files(&self, _files: &[FileInfo]) -> rustyriver::Result<()> {
        Ok(())
    }
    async fn delete_all(&self) -> rustyriver::Result<()> {
        Ok(())
    }
}

/// Port of `TestReplica_Restore_InvalidFileSize/EmptyFile`: a snapshot file with
/// `Size == 0` must make restore fail with an "invalid ltx file" error.
#[tokio::test]
async fn restore_empty_snapshot_file_errors() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("restored.db");

    let client = BadSizeClient { size: 0 };
    let err = replica::restore(&client, &out, TXID(0))
        .await
        .expect_err("empty file must be rejected");
    assert!(
        err.to_string().contains("invalid ltx file"),
        "expected 'invalid ltx file', got: {err}"
    );
    assert!(
        !out.exists(),
        "no output DB is produced on a rejected restore"
    );
}

/// Port of `TestReplica_Restore_InvalidFileSize/TruncatedFile`: a snapshot file
/// smaller than the LTX header (`Size == 50 < HEADER_SIZE`) must make restore
/// fail with an "invalid ltx file" error.
#[tokio::test]
async fn restore_truncated_snapshot_file_errors() {
    // Precondition (compile-time): 50 is below the LTX header size, so the size
    // guard — not a decode — is what rejects it. Matches the upstream mock's 50.
    const _: () = assert!(50 < ltx::HEADER_SIZE);
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("restored.db");

    let client = BadSizeClient { size: 50 };
    let err = replica::restore(&client, &out, TXID(0))
        .await
        .expect_err("truncated file must be rejected");
    assert!(
        err.to_string().contains("invalid ltx file"),
        "expected 'invalid ltx file', got: {err}"
    );
    assert!(
        !out.exists(),
        "no output DB is produced on a rejected restore"
    );
}

// ── On-disk byte-level fault injection against a real chain ──────────────────

/// CORRUPT BYTES in a mid-chain L0 file: a latest-state restore must fail via a
/// checksum/corruption error (decode_file verifies every input), never panic,
/// and never write a corrupt output DB.
#[tokio::test(flavor = "multi_thread")]
async fn corrupt_midchain_ltx_fails_cleanly() {
    let fx = build_fixture().await;
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("restored.db");

    // Flip a byte in the data region of the snapshot (TXID 1) file.
    let victim = l0_path(&fx.replica_root, TXID(1));
    let mut bytes = std::fs::read(&victim).unwrap();
    let mid = ltx::HEADER_SIZE + (bytes.len() - ltx::HEADER_SIZE) / 2;
    bytes[mid] ^= 0xFF;
    std::fs::write(&victim, &bytes).unwrap();

    let client = FileReplicaClient::new(fx.replica_root.clone());
    let err = replica::restore(&client, &out, TXID(0))
        .await
        .expect_err("corrupt LTX must be rejected");
    assert!(
        matches!(
            err,
            Error::ChecksumMismatch | Error::LTXCorrupted | Error::Other(_)
        ),
        "corrupt LTX rejected with a clean error, got {err:?}"
    );
    // The atomic temp-rename means a failed restore leaves no output DB.
    assert!(!out.exists(), "no corrupt output DB is produced");
}

/// TRUNCATED final upload (partial PUT): the newest L0 file is cut to below the
/// LTX header size on disk. A latest restore must fail with "invalid ltx file",
/// while a restore targeted at the last *intact* TXID still yields a valid DB at
/// that TXID — the "valid DB at a TXID ≤ last durable" guarantee.
#[tokio::test(flavor = "multi_thread")]
async fn truncated_final_upload_recovers_to_prior_txid() {
    let fx = build_fixture().await;
    let dir = tempfile::tempdir().unwrap();

    // Truncate the LAST L0 file to 40 bytes (< HEADER_SIZE): a partial upload.
    let victim = l0_path(&fx.replica_root, fx.last_txid);
    let orig = std::fs::read(&victim).unwrap();
    assert!(orig.len() > 40);
    std::fs::write(&victim, &orig[..40]).unwrap();

    let client = FileReplicaClient::new(fx.replica_root.clone());

    // 1) A latest restore must be rejected ("invalid ltx file"), never panic.
    let out_latest = dir.path().join("latest.db");
    let err = replica::restore(&client, &out_latest, TXID(0))
        .await
        .expect_err("a sub-header tail file must be rejected");
    assert!(
        err.to_string().contains("invalid ltx file"),
        "expected 'invalid ltx file', got: {err}"
    );
    assert!(!out_latest.exists());

    // 2) Restore to the last intact TXID (one before the damaged tail) → valid DB.
    let target = TXID(fx.last_txid.0 - 1);
    let out_prior = dir.path().join("prior.db");
    replica::restore(&client, &out_prior, target)
        .await
        .expect("restore to the last intact TXID must succeed");
    assert!(
        restored_db_is_valid(&out_prior),
        "restored DB at the prior TXID is a valid SQLite database"
    );
    assert!(count_rows(&out_prior, "t") >= 1, "restored DB has data");
}

/// MISSING / MISSED FRAMES: a mid-chain L0 file is deleted entirely (a dropped
/// segment). A latest restore must fail cleanly (non-contiguous chain), and a
/// restore targeted before the gap must still reproduce the point-in-time DB
/// (Oracle A vs. the `VACUUM INTO` snapshot taken at `safe_txid`).
#[tokio::test(flavor = "multi_thread")]
async fn missing_midchain_segment_recovers_before_the_gap() {
    let fx = build_fixture().await;
    let dir = tempfile::tempdir().unwrap();

    // Delete an L0 file in the damaged tail (right after the safe point), opening
    // a gap in the chain.
    let gap_txid = TXID(fx.safe_txid.0 + 1);
    assert!(
        gap_txid < fx.last_txid,
        "the gap is strictly inside the tail"
    );
    std::fs::remove_file(l0_path(&fx.replica_root, gap_txid)).unwrap();

    let client = FileReplicaClient::new(fx.replica_root.clone());

    // 1) A latest restore must fail cleanly (the chain is non-contiguous) — never
    //    panic, never silently skip the gap.
    let out_latest = dir.path().join("latest.db");
    let res = replica::restore(&client, &out_latest, TXID(0)).await;
    assert!(
        res.is_err(),
        "a latest restore across a chain gap must fail, got Ok"
    );
    assert!(!out_latest.exists(), "no output DB on a failed restore");

    // 2) A restore targeted at the safe TXID (before the gap) still works.
    if !has_bin("sqlite3") {
        eprintln!("skipping the Oracle-A half: sqlite3 not on PATH");
        return;
    }
    let out_safe = dir.path().join("safe_restored.db");
    replica::restore(&client, &out_safe, fx.safe_txid)
        .await
        .expect("restore before the gap must succeed");
    assert!(restored_db_is_valid(&out_safe), "valid DB before the gap");
    db_equal_a(&fx.safe_snapshot, &out_safe)
        .expect("Oracle A: restore@safe_txid == the point-in-time snapshot");
}

/// MISSING SNAPSHOT ANCHOR: deleting the very first L0 file (TXID 1, the snapshot
/// anchor) must make even a latest restore fail cleanly (no anchor → no plan),
/// never panic.
#[tokio::test(flavor = "multi_thread")]
async fn missing_snapshot_anchor_fails_cleanly() {
    let fx = build_fixture().await;
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("restored.db");

    std::fs::remove_file(l0_path(&fx.replica_root, TXID(1))).unwrap();

    let client = FileReplicaClient::new(fx.replica_root.clone());
    let res = replica::restore(&client, &out, TXID(0)).await;
    assert!(
        res.is_err(),
        "restore without the snapshot anchor must fail, got Ok"
    );
    assert!(!out.exists(), "no output DB is produced");
}

/// EMPTY (zero-length) tail file on disk via the real file client: listing
/// reports `size == 0`, so the restore size guard rejects it with
/// "invalid ltx file" — the on-disk analog of the ported EmptyFile mock case.
#[tokio::test(flavor = "multi_thread")]
async fn empty_ondisk_tail_file_errors() {
    let fx = build_fixture().await;
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("restored.db");

    // Zero out the last L0 file in place.
    std::fs::write(l0_path(&fx.replica_root, fx.last_txid), b"").unwrap();

    let client = FileReplicaClient::new(fx.replica_root.clone());
    let err = replica::restore(&client, &out, TXID(0))
        .await
        .expect_err("an empty tail file must be rejected");
    assert!(
        err.to_string().contains("invalid ltx file"),
        "expected 'invalid ltx file', got: {err}"
    );
    assert!(!out.exists());
}
