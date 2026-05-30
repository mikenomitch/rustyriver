//! differential_xtool — **GATE G3 ("M1 correct"), PLAN.md §6.3**: cross-tool
//! differential tests against the REAL `litestream` binary (pinned v0.5.11).
//!
//! This is the strongest correctness oracle in the project. It does not compare
//! against rustyriver's own output (forbidden by AGENTS.md rule 3): the expected
//! value is whatever the real Litestream binary produces. Three directions, each
//! exactly as specified in §6.3:
//!
//! * **D1 (write path):** rustyriver replicates a DB into a file replica, then the
//!   **real `litestream restore`** reproduces it → **Oracle A** vs the source.
//!   Proves our *written* LTX format is real-Litestream-readable. If this passes,
//!   our serializer is wire-compatible with upstream.
//! * **D2 (restore path):** the **real `litestream replicate`** writes a replica,
//!   then **rustyriver restores** it → **Oracle A** vs the source. Proves our
//!   *reader* handles real-Litestream output.
//! * **D3 (format cross-check):** both tools restore the **same** replica → the two
//!   output DB files are **byte-identical** (**Oracle B**, after a TRUNCATE
//!   checkpoint). Isolates format fidelity from SQLite-version noise because both
//!   replay identical page images. Run in both directions (over a
//!   rustyriver-written replica AND a real-Litestream-written replica).
//!
//! ## Skip policy
//! Every test self-skips (logs + `return`, never a silent pass and never a
//! failure) when the `litestream` binary or `sqlite3` is absent from PATH — but
//! in the pinned CI/dev environment (PLAN.md D-3) both ARE present, so the gate
//! actually runs. The skip is a runtime guard, not a compile-time ignore
//! attribute, and no assertion is weakened (AGENTS.md rules 1-2).
//!
//! ## Why a file replica (not MinIO)
//! D1–D3 exercise the *format* and the *restore algorithm*, which are transport
//! agnostic. The file `ReplicaClient` is byte-for-byte the same object layout the
//! S3 client uses (`<root>/ltx/<level>/<min>-<max>.ltx`), and the real binary's
//! `file://` backend reads/writes that identical tree — so a file replica is the
//! cleanest, hermetic way to put both tools on the *same* bytes. The S3/MinIO
//! transport is separately proven end-to-end by T7 (`integration_minio.rs`).

use rusqlite::Connection;
use rustyriver::client::file::FileReplicaClient;
use rustyriver::db::Db;
use rustyriver::replica::{self, Replica};
use rustyriver::TXID;
use std::path::Path;
use std::process::Command;

/// Absolute path to the `db_equal.sh` oracle (PLAN.md §6.1).
fn db_equal_script() -> String {
    format!("{}/scripts/db_equal.sh", env!("CARGO_MANIFEST_DIR"))
}

/// Runs `db_equal.sh <mode> <a> <b>`; `Ok(())` on exit 0, else the captured
/// stdout+stderr so a failure pinpoints the mismatch. Requires `sqlite3`.
///
/// `mode` is `"A"` (logical equality) or `"B"` (byte-identical main file).
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

/// True if `bin` resolves on PATH (exits with any status code when run with a
/// help-ish flag). Mirrors the probe used by the other integration suites.
fn has_bin(bin: &str) -> bool {
    Command::new(bin)
        .arg("--help")
        .output()
        .map(|o| o.status.success() || o.status.code().is_some())
        .unwrap_or(false)
}

/// Returns `true` and logs a skip note if either required tool is missing. The
/// differential gate is meaningless without BOTH the real binary and `sqlite3`.
fn skip_if_tools_missing(test: &str) -> bool {
    if !has_bin("litestream") {
        eprintln!("skipping {test}: `litestream` binary not on PATH (PLAN.md D-3 env required)");
        return true;
    }
    if !has_bin("sqlite3") {
        eprintln!("skipping {test}: `sqlite3` not on PATH (required for the db_equal oracle)");
        return true;
    }
    false
}

/// A `file://<abs-path>` URL for the real binary's `file:` backend. The path must
/// be absolute (litestream rejects a relative `file://` host segment).
fn file_url(root: &Path) -> String {
    let abs = root
        .canonicalize()
        .unwrap_or_else(|_| root.to_path_buf())
        .to_string_lossy()
        .into_owned();
    format!("file://{abs}")
}

/// Opens an application writer connection in WAL mode — the host's own SQLite
/// handle writing alongside the managed `Db`, exactly as a library embedder does.
fn open_writer(path: &Path) -> Connection {
    let c = Connection::open(path).unwrap();
    c.busy_timeout(std::time::Duration::from_secs(5)).unwrap();
    let mode: String = c
        .query_row("PRAGMA journal_mode=WAL", [], |r| r.get(0))
        .unwrap();
    assert_eq!(mode, "wal", "test DB must be in WAL mode");
    c
}

/// The deterministic multi-transaction workload shared by the directions, so each
/// case exercises a snapshot + several incremental L0 files (mixed
/// insert/update/delete/DDL + a multi-page transaction). `apply` receives each
/// SQL statement-group in order; the caller decides what to do after each
/// (rustyriver capture+upload, or a real `litestream replicate -once`).
fn workload() -> &'static [&'static str] {
    &[
        "CREATE TABLE kv (k TEXT PRIMARY KEY, v TEXT NOT NULL)",
        "INSERT INTO kv (k, v) VALUES ('a','1'),('b','2'),('c','3')",
        "UPDATE kv SET v='updated' WHERE k='a'",
        "INSERT INTO kv (k, v) VALUES ('d','4'),('e','5')",
        "DELETE FROM kv WHERE k='b'",
        // A larger multi-page transaction to exercise multi-frame WAL capture.
        "CREATE TABLE big (id INTEGER PRIMARY KEY, blob TEXT);\
         INSERT INTO big (id, blob) SELECT value, hex(randomblob(200)) \
           FROM (WITH RECURSIVE c(value) AS (SELECT 1 UNION ALL SELECT value+1 \
                 FROM c WHERE value<500) SELECT value FROM c);",
    ]
}

/// Drives rustyriver's write path over `workload()`: opens a managed `Db`, an app
/// writer, applies each statement-group, then captures (`db.sync`) and uploads
/// (`replica.sync`) it. Leaves a replica tree at `replica_root` and the source DB
/// at `src_path`. Returns the final captured TXID.
async fn rustyriver_replicate(src_path: &Path, replica_root: &Path) -> TXID {
    let db = Db::open(src_path).unwrap();
    let writer = open_writer(src_path);
    let client = FileReplicaClient::new(replica_root.to_string_lossy().into_owned());
    let mut replica = Replica::new(db, client);

    for sql in workload() {
        writer.execute_batch(sql).unwrap();
        replica.db_mut().unwrap().sync().unwrap();
        replica.sync().await.unwrap();
    }

    let pos = replica.db_mut().unwrap().pos().unwrap();
    assert!(
        pos.txid.0 >= workload().len() as u64,
        "expected at least {} captured TXIDs, got {}",
        workload().len(),
        pos.txid
    );

    // Clean shutdown so the source WAL is released before the real binary or the
    // oracle reads the source file.
    let db = replica.into_db().unwrap();
    db.close().unwrap();
    drop(writer);
    pos.txid
}

/// Drives the REAL binary's write path over `workload()`: seeds WAL mode, then for
/// each statement-group writes via plain `sqlite3` and runs `litestream replicate
/// -once` to flush exactly one sync (no background timing races — the same
/// deterministic method `capture-golden.sh` uses). Leaves a replica tree at
/// `replica_root` and the source DB at `src_path`.
fn litestream_replicate(src_path: &Path, replica_root: &Path) {
    // Seed WAL mode + the first statement-group on a plain connection, then take
    // the initial snapshot.
    let url = file_url(replica_root);
    {
        let c = open_writer(src_path);
        c.execute_batch(workload()[0]).unwrap();
    }
    run_litestream(&["replicate", "-once", &src_path.to_string_lossy(), &url]);

    for sql in &workload()[1..] {
        {
            let c = open_writer(src_path);
            c.execute_batch(sql).unwrap();
        }
        run_litestream(&["replicate", "-once", &src_path.to_string_lossy(), &url]);
    }
}

/// Runs `litestream <args...>`, asserting success and surfacing stderr on failure.
fn run_litestream(args: &[&str]) {
    let out = Command::new("litestream")
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("spawn litestream {args:?}: {e}"));
    assert!(
        out.status.success(),
        "litestream {args:?} failed (status {:?}):\nstdout: {}\nstderr: {}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
}

/// Runs the real `litestream restore -o <out> <file-url>` for a replica tree.
fn litestream_restore(replica_root: &Path, out: &Path) {
    let url = file_url(replica_root);
    run_litestream(&["restore", "-o", &out.to_string_lossy(), &url]);
    assert!(out.exists(), "litestream restore did not produce {out:?}");
}

// ───────────────────────────── D1 (write path) ─────────────────────────────

/// **D1 — our write → real `litestream restore` → Oracle A vs source.**
///
/// rustyriver replicates the workload into a file replica; the *real* binary
/// restores that tree; the restored DB must be logically identical to the source.
/// This is the load-bearing proof that rustyriver's LTX serializer is byte-format
/// compatible with upstream Litestream — if the real tool can read what we wrote,
/// our format is correct (AGENTS.md rule 3: the binary, not us, is the oracle).
#[tokio::test(flavor = "multi_thread")]
async fn d1_our_write_real_restore_oracle_a() {
    if skip_if_tools_missing("d1_our_write_real_restore_oracle_a") {
        return;
    }

    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("source.db");
    let replica_root = dir.path().join("replica");
    let restored = dir.path().join("restored-by-litestream.db");

    // 1. rustyriver writes the replica.
    rustyriver_replicate(&src, &replica_root).await;

    // 2. The REAL binary restores our tree.
    litestream_restore(&replica_root, &restored);

    // 3. Oracle A: real-restored == source.
    db_equal("A", &src, &restored)
        .expect("D1: real `litestream restore` of our replica must equal the source (Oracle A)");
}

// ──────────────────────────── D2 (restore path) ────────────────────────────

/// **D2 — real `litestream replicate` → our restore → Oracle A vs source.**
///
/// The real binary replicates the workload; rustyriver restores that tree; the
/// restored DB must be logically identical to the source. Proves our LTX *reader*
/// + restore algorithm correctly consume real-Litestream-produced bytes.
#[tokio::test(flavor = "multi_thread")]
async fn d2_real_write_our_restore_oracle_a() {
    if skip_if_tools_missing("d2_real_write_our_restore_oracle_a") {
        return;
    }

    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("source.db");
    let replica_root = dir.path().join("replica");
    let restored = dir.path().join("restored-by-rustyriver.db");

    // 1. The REAL binary writes the replica.
    litestream_replicate(&src, &replica_root);

    // 2. rustyriver restores it (client-only restore; no Db needed).
    let client = FileReplicaClient::new(replica_root.to_string_lossy().into_owned());
    replica::restore(&client, &restored, TXID(0))
        .await
        .expect("D2: rustyriver restore of a real-Litestream replica");

    // 3. Oracle A: our-restored == source.
    db_equal("A", &src, &restored)
        .expect("D2: our restore of a real `litestream` replica must equal the source (Oracle A)");
}

// ─────────────────────────── D3 (format cross-check) ───────────────────────

/// **D3 (over a rustyriver-written replica) — both tools restore the SAME tree →
/// byte-identical (Oracle B).**
///
/// rustyriver writes the replica, then BOTH rustyriver and the real binary restore
/// it; after a TRUNCATE checkpoint the two output main-DB files must be
/// byte-for-byte identical. Because both restorers replay the *same* page images,
/// any byte difference is pure format/restore-algorithm divergence — the most
/// sensitive format-fidelity check (Risk R-1/R-2). This direction additionally
/// proves our *writer* emits page images the real decoder reconstructs bit-exactly.
#[tokio::test(flavor = "multi_thread")]
async fn d3_byte_identical_over_our_replica() {
    if skip_if_tools_missing("d3_byte_identical_over_our_replica") {
        return;
    }

    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("source.db");
    let replica_root = dir.path().join("replica");
    let ours = dir.path().join("ours.db");
    let theirs = dir.path().join("theirs.db");

    // rustyriver writes the replica.
    rustyriver_replicate(&src, &replica_root).await;

    // Our restore.
    let client = FileReplicaClient::new(replica_root.to_string_lossy().into_owned());
    replica::restore(&client, &ours, TXID(0))
        .await
        .expect("D3: our restore of our replica");

    // The real binary's restore of the SAME tree.
    litestream_restore(&replica_root, &theirs);

    // Oracle B: byte-identical main DB files after a TRUNCATE checkpoint.
    db_equal("B", &theirs, &ours).expect(
        "D3: our restore and `litestream restore` of our replica must be byte-identical (Oracle B)",
    );
}

/// **D3 (over a real-Litestream-written replica) — both tools restore the SAME
/// tree → byte-identical (Oracle B).**
///
/// The mirror of `d3_byte_identical_over_our_replica`: the real binary writes the
/// replica, then both tools restore it, and the outputs must be byte-identical.
/// Running D3 in both write-directions catches an asymmetry where one tool's
/// *writer* and the other's *reader* happen to agree only on self-produced bytes.
#[tokio::test(flavor = "multi_thread")]
async fn d3_byte_identical_over_real_replica() {
    if skip_if_tools_missing("d3_byte_identical_over_real_replica") {
        return;
    }

    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("source.db");
    let replica_root = dir.path().join("replica");
    let ours = dir.path().join("ours.db");
    let theirs = dir.path().join("theirs.db");

    // The real binary writes the replica.
    litestream_replicate(&src, &replica_root);

    // Our restore.
    let client = FileReplicaClient::new(replica_root.to_string_lossy().into_owned());
    replica::restore(&client, &ours, TXID(0))
        .await
        .expect("D3: our restore of a real replica");

    // The real binary's restore of the SAME tree.
    litestream_restore(&replica_root, &theirs);

    // Oracle B: byte-identical main DB files after a TRUNCATE checkpoint.
    db_equal("B", &theirs, &ours).expect(
        "D3: our restore and `litestream restore` of a real replica must be byte-identical (Oracle B)",
    );
}

/// **D2 at a TARGET TXID — real write → our point-in-time restore → Oracle A.**
///
/// Exercises the `-txid`-equivalent restore path against real-Litestream bytes:
/// the real binary replicates the full workload, rustyriver restores up to an
/// intermediate TXID, and the result must equal what the *real* binary restores at
/// that same `-txid`. Both restorers consume the identical real replica, so this
/// confirms our `calc_restore_plan` selects the same chain the real tool does for
/// a point-in-time target. (Uses Oracle A because a partial restore stops at a
/// mid-stream TXID; the comparison is real-vs-ours at the same target.)
#[tokio::test(flavor = "multi_thread")]
async fn d2_real_write_our_restore_at_target_txid() {
    if skip_if_tools_missing("d2_real_write_our_restore_at_target_txid") {
        return;
    }

    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("source.db");
    let replica_root = dir.path().join("replica");
    let ours = dir.path().join("ours-at-txid.db");
    let theirs = dir.path().join("theirs-at-txid.db");

    // The real binary writes the full workload.
    litestream_replicate(&src, &replica_root);

    // Pick an intermediate target: the count of statement-groups gives the max
    // TXID; restore to roughly the middle of the chain.
    let client = FileReplicaClient::new(replica_root.to_string_lossy().into_owned());
    let files = {
        use rustyriver::client::ReplicaClient;
        client.ltx_files(0, TXID(0), false).await.unwrap()
    };
    let max_txid = files.iter().map(|f| f.max_txid.0).max().unwrap();
    assert!(
        max_txid >= 4,
        "need a multi-file chain (got max {max_txid})"
    );
    let target = TXID(max_txid - 2); // a mid-stream point-in-time target

    // Our restore up to `target`.
    replica::restore(&client, &ours, target)
        .await
        .expect("D2(txid): our restore to target");

    // The real binary's restore up to the same `-txid`.
    let url = file_url(&replica_root);
    let txid_hex = format!("{:016x}", target.0);
    run_litestream(&[
        "restore",
        "-txid",
        &txid_hex,
        "-o",
        &theirs.to_string_lossy(),
        &url,
    ]);
    assert!(theirs.exists(), "litestream restore -txid produced no file");

    // Oracle A: our point-in-time restore == the real tool's at the same TXID.
    db_equal("A", &theirs, &ours).expect(
        "D2(txid): our restore@target must equal real `litestream restore -txid` at the same TXID (Oracle A)",
    );
}
