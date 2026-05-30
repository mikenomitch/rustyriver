//! db.rs — SQLite database lifecycle: WAL-mode setup, checkpoint takeover, the
//! LTX capture loop, and clean shutdown. Ported from litestream@v0.5.11 `db.go`.
//!
//! T9 is the highest-risk module in the crate (Risk R-4: long-running read-tx
//! vs manual checkpoint across the async boundary), so it is landed in **tight,
//! independently-tested sub-steps** rather than one block.
//!
//! ## Sub-step 1 (this commit): `Db::open` — the WAL-mode foundation
//! Ported from `DB.init` (db.go:795-878): open the connection with
//! `busy_timeout` + `wal_autocheckpoint(0)`, enable `journal_mode=WAL` (verifying
//! SQLite returns `"wal"`), create the `_litestream_seq` / `_litestream_lock`
//! control tables, read the page size, and expose the meta/WAL sidecar paths.
//!
//! ## Deferred to later sub-steps (tracked in OPEN_QUESTIONS)
//! - `acquireReadLock`: the long-running read transaction that takes over
//!   checkpointing (the R-4 concurrency core; needs a dedicated DB thread since
//!   `rusqlite::Connection` is `!Sync`).
//! - `setPersistWAL` (PERSIST_WAL): likely an `unsafe` `sqlite3_file_control`
//!   FFI call — the most probable `unsafe` block in the crate.
//! - The WAL→LTX capture loop, `verify()` snapshot-on-continuity-break, and the
//!   clean-shutdown read-lock release.

use crate::error::{Error, Result};
use crate::META_DIR_SUFFIX;
use rusqlite::Connection;
use std::path::{Path, PathBuf};
use std::time::Duration;

fn sql_err(e: rusqlite::Error) -> Error {
    Error::Other(Box::new(e))
}

/// A SQLite database managed for replication.
///
/// Holds a connection with WAL mode enabled and auto-checkpoint disabled, so
/// litestream — not SQLite — decides when the WAL is checkpointed.
///
/// NOTE: `rusqlite::Connection` is `!Sync`; a later sub-step pins it to a
/// dedicated thread for the async capture loop (Risk R-4). For now `Db` is a
/// synchronous handle that establishes the WAL-mode foundation.
pub struct Db {
    path: PathBuf,
    // Held to keep the SQLite connection — and therefore the persistent WAL and
    // the future long-running read-lock — alive. The capture loop (a later T9
    // sub-step) reads from it; until then no non-test code does, hence the allow.
    #[allow(dead_code)]
    conn: Connection,
    page_size: u32,
}

impl Db {
    /// Default busy timeout, matching litestream's `DB.BusyTimeout` default.
    pub const DEFAULT_BUSY_TIMEOUT_MS: u64 = 5_000;

    /// Opens and initializes the database with litestream's WAL-mode setup.
    ///
    /// Ported from `DB.init` (db.go:795-878) — the connection setup half. The
    /// read-lock takeover and capture loop land in later sub-steps.
    pub fn open(path: impl AsRef<Path>) -> Result<Db> {
        let path = path.as_ref().to_path_buf();
        let conn = Connection::open(&path).map_err(sql_err)?;

        // DSN pragmas: busy_timeout + wal_autocheckpoint(0) (db.go:818-819).
        conn.busy_timeout(Duration::from_millis(Self::DEFAULT_BUSY_TIMEOUT_MS))
            .map_err(sql_err)?;
        conn.pragma_update(None, "wal_autocheckpoint", 0)
            .map_err(sql_err)?;

        // Enable WAL; SQLite returns the new mode on success (db.go:849-853).
        let mode: String = conn
            .query_row("PRAGMA journal_mode=WAL", [], |r| r.get(0))
            .map_err(sql_err)?;
        if mode != "wal" {
            return Err(Error::Other(
                format!("enable wal failed, mode={mode:?}").into(),
            ));
        }

        // Control tables: one forces WAL writes when empty, the other forces a
        // write lock during sync (db.go:857-864).
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS _litestream_seq (id INTEGER PRIMARY KEY, seq INTEGER);\
             CREATE TABLE IF NOT EXISTS _litestream_lock (id INTEGER);",
        )
        .map_err(sql_err)?;

        // Page size (db.go:874-878).
        let page_size: i64 = conn
            .query_row("PRAGMA page_size", [], |r| r.get(0))
            .map_err(sql_err)?;
        if page_size <= 0 {
            return Err(Error::Other(
                format!("invalid db page size: {page_size}").into(),
            ));
        }

        Ok(Db {
            path,
            conn,
            // Validated > 0 above; SQLite page sizes are <= 65536.
            page_size: page_size as u32,
        })
    }

    /// The database file path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The SQLite page size, in bytes.
    pub fn page_size(&self) -> u32 {
        self.page_size
    }

    /// Path to the litestream meta directory (`<db>-litestream`).
    /// Ported from `DB.MetaPath` (uses `MetaDirSuffix`).
    pub fn meta_path(&self) -> PathBuf {
        let mut s = self.path.clone().into_os_string();
        s.push(META_DIR_SUFFIX);
        PathBuf::from(s)
    }

    /// Path to SQLite's WAL sidecar file (`<db>-wal`).
    pub fn wal_path(&self) -> PathBuf {
        let mut s = self.path.clone().into_os_string();
        s.push("-wal");
        PathBuf::from(s)
    }

    /// Reads the current WAL sidecar bytes (empty if the file is absent). The
    /// capture sub-step will feed these to the `wal` reader (T1).
    pub fn read_wal(&self) -> Result<Vec<u8>> {
        match std::fs::read(self.wal_path()) {
            Ok(b) => Ok(b),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(e) => Err(e.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_enables_wal_and_reads_page_size() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        let db = Db::open(&path).unwrap();

        assert_eq!(db.page_size(), 4096, "default sqlite page size");

        let mode: String = db
            .conn
            .query_row("PRAGMA journal_mode", [], |r| r.get(0))
            .unwrap();
        assert_eq!(mode, "wal", "journal mode persisted as WAL");

        // wal_autocheckpoint disabled so litestream controls checkpointing.
        let autockpt: i64 = db
            .conn
            .query_row("PRAGMA wal_autocheckpoint", [], |r| r.get(0))
            .unwrap();
        assert_eq!(autockpt, 0, "auto-checkpoint disabled");

        let n: i64 = db
            .conn
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE name IN ('_litestream_seq','_litestream_lock')",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 2, "control tables created");

        assert_eq!(db.meta_path().file_name().unwrap(), "state.db-litestream");
        assert_eq!(db.wal_path().file_name().unwrap(), "state.db-wal");
    }

    #[test]
    fn writes_accumulate_in_the_wal() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        let db = Db::open(&path).unwrap();

        db.conn
            .execute_batch(
                "CREATE TABLE kv(k TEXT PRIMARY KEY, v TEXT NOT NULL);\
                 INSERT INTO kv VALUES ('a','1'),('b','2'),('c','3');",
            )
            .unwrap();

        // With auto-checkpoint disabled, the writes remain in the WAL sidecar.
        let wal = db.read_wal().unwrap();
        assert!(!wal.is_empty(), "writes are captured in the WAL");
        assert!(
            wal.len() >= crate::WAL_HEADER_SIZE,
            "WAL has at least a 32-byte header for the capture loop (T1) to parse"
        );
    }
}
