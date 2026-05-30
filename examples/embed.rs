//! Disaster-recovery story — end to end.
//!
//! Demonstrates the rustyriver embedding API in five acts:
//!
//!   1. Open a SQLite database, create a table, and write several transactions.
//!   2. Replicate each WAL segment to a local `FileReplicaClient` via `Db::sync`
//!      + `Replica::sync`.
//!   3. SIMULATE HOST LOSS — drop the `Db` handle and delete the database file.
//!   4. Restore the database from the file replica to a fresh path.
//!   5. VERIFY the restored database contains the original rows.
//!
//! Run with:
//!   cargo run --example embed
//!
//! No external services are required; everything runs in a temporary directory.

use rusqlite::Connection;
use rustyriver::{Db, FileReplicaClient, Replica};
use std::path::Path;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // ── 1. Set up temp directories ────────────────────────────────────────────
    let workdir = tempfile::tempdir()?;
    let db_path = workdir.path().join("events.db");
    let replica_dir = workdir.path().join("replica");
    let restored_path = workdir.path().join("events-restored.db");

    println!("=== rustyriver embed example ===");
    println!("  DB:       {}", db_path.display());
    println!("  Replica:  {}", replica_dir.display());
    println!("  Restored: {}", restored_path.display());
    println!();

    // ── 2. Open the managed Db and write several transactions ─────────────────
    println!("--- PHASE 1: writing transactions ---");
    let mut db = Db::open(&db_path)?;

    // Use a plain rusqlite connection to insert data (the managed Db drives
    // replication; application code uses its own connection for writes, just as
    // a host would alongside the embedded library).
    {
        let conn = Connection::open(&db_path)?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS events (
               id   INTEGER PRIMARY KEY AUTOINCREMENT,
               name TEXT NOT NULL
             );",
        )?;
        conn.execute("INSERT INTO events (name) VALUES (?1)", ["event-alpha"])?;
        println!("  wrote: event-alpha");
    }

    // Sync the WAL segment → local LTX file (the capture half).
    db.sync()?;

    {
        let conn = Connection::open(&db_path)?;
        conn.execute("INSERT INTO events (name) VALUES (?1)", ["event-beta"])?;
        println!("  wrote: event-beta");
    }
    db.sync()?;

    {
        let conn = Connection::open(&db_path)?;
        conn.execute("INSERT INTO events (name) VALUES (?1)", ["event-gamma"])?;
        println!("  wrote: event-gamma");
    }
    db.sync()?;

    println!("  three transactions captured to local LTX files");

    // ── 3. Upload local LTX files to the file replica ─────────────────────────
    println!();
    println!("--- PHASE 2: replicating to FileReplicaClient ---");
    let client = FileReplicaClient::new(replica_dir.to_string_lossy().into_owned());
    let mut replica = Replica::new(db, client);

    // sync() reads the local LTX files written by Db::sync() and uploads them
    // to the replica client.
    replica.sync().await?;
    println!("  upload complete");

    // ── 4. SIMULATE HOST LOSS ─────────────────────────────────────────────────
    println!();
    println!("--- PHASE 3: simulating host loss ---");

    // Consume the Replica to get the Db back, then close it cleanly (releases
    // the WAL read lock so SQLite can checkpoint).
    let db = replica.into_db().expect("db is attached");
    db.close()?;
    println!("  Db handle dropped (read lock released)");

    // Delete the database file and its WAL/SHM sidecars to simulate data loss.
    remove_if_exists(&db_path)?;
    remove_if_exists(&db_path.with_extension("db-wal"))?;
    remove_if_exists(&db_path.with_extension("db-shm"))?;
    // Also delete the wal/shm files with the exact sqlite naming convention
    let wal = db_path.with_extension("").with_file_name(format!(
        "{}-wal",
        db_path.file_stem().unwrap().to_string_lossy()
    ));
    let shm = db_path.with_extension("").with_file_name(format!(
        "{}-shm",
        db_path.file_stem().unwrap().to_string_lossy()
    ));
    remove_if_exists(&wal)?;
    remove_if_exists(&shm)?;
    println!("  database files deleted — host loss simulated");

    // ── 5. Restore from the replica ───────────────────────────────────────────
    println!();
    println!("--- PHASE 4: restoring from replica ---");
    // restore() downloads the LTX chain from the client, merges it (compactor
    // semantics), and writes the reconstructed SQLite database atomically.
    let restore_client = FileReplicaClient::new(replica_dir.to_string_lossy().into_owned());
    rustyriver::restore(&restore_client, &restored_path, rustyriver::TXID(0)).await?;
    println!("  restore complete: {}", restored_path.display());

    // ── 6. VERIFY the restored data ───────────────────────────────────────────
    println!();
    println!("--- PHASE 5: verifying restored data ---");
    let conn = Connection::open(&restored_path)?;
    let mut stmt = conn.prepare("SELECT name FROM events ORDER BY id")?;
    let names: Vec<String> = stmt
        .query_map([], |row| row.get(0))?
        .collect::<Result<Vec<_>, _>>()?;

    println!("  rows in restored DB: {:?}", names);
    assert_eq!(
        names,
        vec!["event-alpha", "event-beta", "event-gamma"],
        "restored DB must contain all three original rows in insertion order"
    );

    println!();
    println!("=== SUCCESS: all three rows recovered after simulated host loss ===");
    Ok(())
}

fn remove_if_exists(p: &Path) -> std::io::Result<()> {
    match std::fs::remove_file(p) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}
