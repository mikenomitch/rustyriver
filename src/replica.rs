//! replica.rs — single-replica sync loop + restore orchestration.
//! Ported from litestream@v0.5.11 `replica.go`.
//!
//! A [`Replica`] connects a managed [`crate::db::Db`] to a replication
//! destination via a [`ReplicaClient`]. It drives two halves of replication:
//!
//! * **sync** (`replica.go:132-180`): copy each new L0 LTX file the capture loop
//!   wrote locally up to the remote replica, advancing the replicated position
//!   one TXID at a time. `calc_pos` (`replica.go:208-214`) recovers the starting
//!   position from the newest file already on the replica.
//! * **restore** (`replica.go:533-725`): download the LTX files from the replica
//!   in TXID order, merge them (the compactor in `ltx.NewCompactor`,
//!   ltx@v0.5.1 compactor.go), and reconstruct the SQLite database file the way
//!   `Decoder.DecodeDatabaseTo` (ltx@v0.5.1 decoder.go:223-268) does — pages
//!   `1..=commit`, lock page zero-filled, written verbatim — to a temp file that
//!   is fsync'd and atomically renamed into place.
//!
//! ## Scope (KEEP set, PLAN.md §2 / D-7): L0-only, single replica
//! The real `litestream v0.5.11` L0-only architecture stores **everything at
//! level 0** — the snapshot (MinTXID==1) and every incremental — under
//! `ltx/0/` (verified against `tests/fixtures/golden/replica`, captured from
//! `litestream replicate -once`). The snapshot level (`SnapshotLevel = 9`,
//! compaction_level.go:9) is empty without compaction. [`calc_restore_plan`] is
//! ported faithfully (snapshot anchor at `SnapshotLevel` + per-level cursors so
//! adding compaction later "just works"), but in this scope the plan is the
//! contiguous L0 chain `1..=N`.
//!
//! ## Deferred (logged in OPEN_QUESTIONS.md — need the background runtime / extra
//! scope, NOT on the G2 round-trip path):
//! * **Follow mode** (`replica.go:730-987`, `applyLTXFile`/`fillFollowGap`): the
//!   continuous tail-restore loop. The one-shot G2 gate is a single restore.
//! * **The background monitor goroutine + backoff** (`replica.go:326-441`,
//!   footgun F-13) and `Start`/`Stop`: need a Tokio task owning the `Db`; the
//!   synchronous `sync()` primitive it would call is implemented and tested here.
//! * **V3 (v0.3.x generation) restore** (`RestoreV3`, replica.go:990-1096):
//!   DROPPED (PLAN.md §2 — greenfield, nothing to be backward-compatible with).
//! * **Timestamp / `-txid` targeted restore plumbing through the public API**:
//!   [`calc_restore_plan`] honors a target TXID (used by tests), but the
//!   timestamp path and `RestoreOptions` surface stay minimal for the one-shot.

use crate::client::ReplicaClient;
use crate::db::Db;
use crate::error::{new_ltx_error, Error, Result};
use crate::ltx::{self, FileInfo};
use crate::{Pos, TXID};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// The compaction level which full snapshots are held at.
///
/// Ported from `SnapshotLevel` (compaction_level.go:9). In the L0-only one-shot
/// scope no files land here (snapshots are written at L0), but
/// [`calc_restore_plan`] still probes it first so the algorithm stays a faithful
/// port that works the moment compaction is added.
pub const SNAPSHOT_LEVEL: i32 = 9;

/// Connects a database to a replication destination via a [`ReplicaClient`].
///
/// Ported from `Replica` (replica.go:30-59). The Go type also owns the
/// background-monitor machinery (`wg`/`cancel`/`f`) and tunables
/// (`SyncInterval`/`MonitorEnabled`); those drive the deferred monitor loop and
/// are intentionally omitted here (see module docs). `auto_recover_enabled`
/// mirrors `AutoRecoverEnabled` (replica.go:58) for the monitor's
/// reset-on-corruption path.
pub struct Replica<C: ReplicaClient> {
    /// The database being replicated. `None` for a restore-only replica (the Go
    /// `NewReplicaWithClient(nil, client)` shape used by the restore tests).
    db: Option<Db>,

    /// Client used to connect to the remote replica.
    pub client: C,

    /// Current replicated position (`replica.go:33-34` `pos`).
    pos: Pos,

    /// If true, automatically reset local state when LTX errors are detected
    /// (`replica.go:54-58` `AutoRecoverEnabled`). Consulted by the deferred
    /// monitor loop; the field is kept so the public surface matches upstream.
    pub auto_recover_enabled: bool,
}

impl<C: ReplicaClient> Replica<C> {
    /// Creates a replica that owns `db` and replicates through `client`.
    ///
    /// Ported from `NewReplicaWithClient` (replica.go:73-77).
    pub fn new(db: Db, client: C) -> Self {
        Replica {
            db: Some(db),
            client,
            pos: Pos::ZERO,
            auto_recover_enabled: false,
        }
    }

    /// Creates a restore-only replica with no attached database (Go's
    /// `NewReplicaWithClient(nil, client)`).
    pub fn new_client_only(client: C) -> Self {
        Replica {
            db: None,
            client,
            pos: Pos::ZERO,
            auto_recover_enabled: false,
        }
    }

    /// Returns a reference to the attached database, if any.
    /// Ported from `Replica.DB` (replica.go:89).
    pub fn db(&self) -> Option<&Db> {
        self.db.as_ref()
    }

    /// Returns a mutable reference to the attached database, if any. Lets a
    /// caller drive `db.sync()` (the local capture half) before `replica.sync()`
    /// (the upload half) — the `SyncAndWait` ordering (db.go:500-512).
    pub fn db_mut(&mut self) -> Option<&mut Db> {
        self.db.as_mut()
    }

    /// The current replicated position. Ported from `Replica.Pos`
    /// (replica.go:237-241).
    pub fn pos(&self) -> Pos {
        self.pos
    }

    /// Sets the current replicated position. Ported from `Replica.SetPos`
    /// (replica.go:244-248).
    fn set_pos(&mut self, pos: Pos) {
        self.pos = pos;
    }

    /// Copies new L0 LTX files from the local capture directory to the replica.
    ///
    /// Ported from `Replica.Sync` (replica.go:132-180). On any error the cached
    /// position is cleared so the next sync recomputes it from the replica
    /// (replica.go:137-143). Requires an attached database.
    pub async fn sync(&mut self) -> Result<()> {
        match self.sync_inner().await {
            Ok(()) => Ok(()),
            Err(e) => {
                // Clear last position if an error occurs during sync
                // (replica.go:137-143).
                self.pos = Pos::ZERO;
                Err(e)
            }
        }
    }

    async fn sync_inner(&mut self) -> Result<()> {
        // Calculate current replica position, if unknown (replica.go:146-152).
        if self.pos().is_zero() {
            let pos = self
                .calc_pos()
                .await
                .map_err(|e| Error::Other(format!("calc pos: {e}").into()))?;
            self.set_pos(pos);
        }

        // Find current position of the database (replica.go:155-160).
        let dpos = {
            let db = self
                .db
                .as_mut()
                .ok_or_else(|| Error::Other("no database attached to replica".into()))?;
            db.pos().map_err(|e| {
                Error::Other(format!("cannot determine current position: {e}").into())
            })?
        };
        if dpos.is_zero() {
            return Err(Error::Other("no position, waiting for data".into()));
        }

        // Replicate all L0 LTX files since the last replica position
        // (replica.go:169-174). Each successful upload advances pos by one TXID;
        // re-reading `self.pos()` each iteration mirrors the Go loop exactly.
        loop {
            let tx_id = TXID(self.pos().txid.0 + 1);
            if tx_id > dpos.txid {
                break;
            }
            self.upload_ltx_file(0, tx_id, tx_id).await?;
            self.set_pos(Pos::new(tx_id, 0));
        }

        Ok(())
    }

    /// Uploads a single local LTX file to the replica.
    ///
    /// Ported from `Replica.uploadLTXFile` (replica.go:182-205). A failure to
    /// open the local file is wrapped as an `LTXError{op:"open"}` so the monitor
    /// can classify auto-recoverable corruption (replica.go:186). The write
    /// itself is delegated to the client.
    async fn upload_ltx_file(&mut self, level: i32, min_txid: TXID, max_txid: TXID) -> Result<()> {
        let db = self
            .db
            .as_ref()
            .ok_or_else(|| Error::Other("no database attached to replica".into()))?;
        let filename = db.ltx_path(level as u32, min_txid, max_txid);

        let data = match std::fs::read(&filename) {
            Ok(b) => b,
            Err(e) => {
                return Err(Error::Ltx(Box::new(new_ltx_error(
                    "open",
                    &filename,
                    level,
                    min_txid.0,
                    max_txid.0,
                    e.into(),
                ))));
            }
        };

        self.client
            .write_ltx_file(level, min_txid, max_txid, &data)
            .await
            .map_err(|e| Error::Other(format!("write ltx file: {e}").into()))?;

        Ok(())
    }

    /// Returns the last position saved to the replica for level 0.
    ///
    /// Ported from `Replica.calcPos` (replica.go:208-214) + `MaxLTXFileInfo`
    /// (replica.go:218-233): scans the L0 listing for the highest `max_txid`.
    async fn calc_pos(&self) -> Result<Pos> {
        let info = self
            .max_ltx_file_info(0)
            .await
            .map_err(|e| Error::Other(format!("max ltx file: {e}").into()))?;
        Ok(Pos::new(info.max_txid, info.post_apply_checksum))
    }

    /// Metadata about the last LTX file for a given level (highest `max_txid`),
    /// or a zero `FileInfo` if none exist. Ported from `Replica.MaxLTXFileInfo`
    /// (replica.go:218-233).
    async fn max_ltx_file_info(&self, level: i32) -> Result<FileInfo> {
        let files = self.client.ltx_files(level, TXID(0), false).await?;
        let mut info = FileInfo::default();
        for item in files {
            if item.max_txid > info.max_txid {
                info = item;
            }
        }
        Ok(info)
    }

    /// Restores the database from this replica's client into `output_path`.
    ///
    /// Convenience wrapper over [`restore`] using this replica's client and the
    /// most-recent state (no target TXID). Mirrors the common
    /// `Replica.Restore(ctx, opt)` call with `OutputPath` set and no TXID
    /// (replica.go:533).
    pub async fn restore(&self, output_path: impl AsRef<Path>) -> Result<()> {
        restore(&self.client, output_path, TXID(0)).await
    }
}

/// Restores a database from `client` into `output_path`, optionally up to a
/// target `txid` (`TXID(0)` = most recent state).
///
/// Ported from `Replica.Restore` (replica.go:533-725), LTX path only. Steps:
///   1. refuse to overwrite an existing output (replica.go:591-595);
///   2. [`calc_restore_plan`] → the ordered snapshot+incremental file list;
///   3. download + merge them (compactor semantics) and reconstruct the database
///      image the way `Decoder.DecodeDatabaseTo` does;
///   4. write to `<output>.tmp`, fsync, and atomically rename (replica.go:657-694).
///
/// The V3 generation path and follow mode are out of scope (see module docs).
pub async fn restore<C: ReplicaClient>(
    client: &C,
    output_path: impl AsRef<Path>,
    txid: TXID,
) -> Result<()> {
    let output_path = output_path.as_ref();

    // Ensure output path does not already exist (replica.go:591-595).
    match std::fs::metadata(output_path) {
        Ok(_) => {
            return Err(Error::Other(
                format!(
                    "cannot restore, output path already exists: {}",
                    output_path.display()
                )
                .into(),
            ));
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }

    // Build the restore plan (replica.go:611).
    let infos = calc_restore_plan(client, txid).await?;

    // Download every planned file's bytes, in plan order (replica.go:627-642).
    // Validate each is at least header-sized first (replica.go:628-632).
    let mut files: Vec<Vec<u8>> = Vec::with_capacity(infos.len());
    for info in &infos {
        if info.size < ltx::HEADER_SIZE as i64 {
            return Err(Error::Other(
                format!(
                    "invalid ltx file: level={} min={} max={} has size {} bytes (minimum {})",
                    info.level,
                    info.min_txid,
                    info.max_txid,
                    info.size,
                    ltx::HEADER_SIZE
                )
                .into(),
            ));
        }
        let data = client
            .open_ltx_file(info.level, info.min_txid, info.max_txid, 0, 0)
            .await
            .map_err(|e| Error::Other(format!("open ltx file: {e}").into()))?;
        files.push(data);
    }

    if files.is_empty() {
        return Err(Error::Other("no matching backup files available".into()));
    }

    // Merge the files (compactor) and reconstruct the SQLite database image.
    let image = build_database_image(&files)?;

    // Create the parent directory if needed (replica.go:649-655).
    if let Some(parent) = output_path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }

    // Output to a temp file & atomically rename (replica.go:657-694).
    let tmp_output_path = append_ext(output_path, "tmp");
    write_file_atomic(&tmp_output_path, output_path, &image)?;

    Ok(())
}

/// Returns the ordered list of LTX files needed to restore the database at
/// `txid` (`TXID(0)` = latest).
///
/// Ported from `CalcRestorePlan` (replica.go:1419-1536). Anchors on the latest
/// snapshot at or before the target, then walks compaction levels
/// `SnapshotLevel-1 ..= 0` with per-level cursors, repeatedly choosing the file
/// that extends the longest contiguous TXID range. In the L0-only scope the
/// snapshot anchor lives at L0 (MinTXID==1) and the only cursor with files is
/// level 0, so the result is the contiguous chain `1..=N` (capped at `txid`).
pub async fn calc_restore_plan<C: ReplicaClient>(client: &C, txid: TXID) -> Result<Vec<FileInfo>> {
    let mut infos: Vec<FileInfo> = Vec::new();

    // Start with the latest snapshot before the target TXID (replica.go:1430-1452).
    let snapshot_files = client.ltx_files(SNAPSHOT_LEVEL, TXID(0), false).await?;
    let mut snapshot: Option<FileInfo> = None;
    for info in snapshot_files {
        if txid != TXID(0) && info.max_txid > txid {
            continue;
        }
        snapshot = Some(info);
    }
    if let Some(s) = snapshot {
        infos.push(s);
    }

    // Collect candidates across all compaction levels and pick the next file
    // from any level that extends the longest contiguous TXID range
    // (replica.go:1454-1515).
    let max_level = SNAPSHOT_LEVEL - 1;
    let start_txid = slice_max_txid(&infos);
    let mut current_max = start_txid;
    if txid != TXID(0) && current_max >= txid {
        return Ok(infos);
    }

    // Build a cursor per level, highest level first (replica.go:1463-1473).
    let mut cursors: Vec<RestoreLevelCursor> = Vec::with_capacity((max_level + 1) as usize);
    for level in (0..=max_level).rev() {
        let files = client.ltx_files(level, TXID(0), false).await?;
        cursors.push(RestoreLevelCursor::new(files));
    }

    loop {
        // Choose the best candidate across all level cursors (replica.go:1483-1494).
        let mut next_idx: Option<usize> = None;
        for i in 0..cursors.len() {
            cursors[i].refresh(current_max, txid);
            if cursors[i].candidate.is_none() {
                continue;
            }
            match next_idx {
                None => next_idx = Some(i),
                Some(ni) => {
                    let cand = cursors[i].candidate.as_ref().unwrap();
                    let best = cursors[ni].candidate.as_ref().unwrap();
                    if restore_candidate_better(best, cand) {
                        next_idx = Some(i);
                    }
                }
            }
        }

        let ni = match next_idx {
            Some(ni) => ni,
            None => break,
        };

        // Take the chosen candidate (replica.go:1500-1510).
        let cand = cursors[ni].candidate.take().unwrap();
        if cand.max_txid <= current_max {
            continue;
        }
        current_max = cand.max_txid;
        infos.push(cand);

        if txid != TXID(0) && current_max >= txid {
            break;
        }
    }

    // For a latest/most-recent restore, verify the tail is contiguous
    // (replica.go:1517-1526).
    if !infos.is_empty() && txid == TXID(0) {
        for cursor in cursors.iter_mut() {
            cursor.ensure_current();
            if let Some(cur) = &cursor.current {
                if cur.min_txid.0 > current_max.0 + 1 {
                    return Err(Error::Other(
                        format!(
                            "non-contiguous ltx files: have up to {} but next file starts at {}",
                            current_max, cur.min_txid
                        )
                        .into(),
                    ));
                }
            }
        }
    }

    if infos.is_empty() {
        return Err(Error::TxNotAvailable);
    }
    if txid != TXID(0) && slice_max_txid(&infos) < txid {
        return Err(Error::TxNotAvailable);
    }

    Ok(infos)
}

/// A single level's streaming view during restore planning.
///
/// Ported from `restoreLevelCursor` (replica.go:1538-1603). The Go version
/// streams from a `FileIterator`; here the client already returns a sorted
/// `Vec<FileInfo>`, so the "iterator" is the slice plus a read index.
struct RestoreLevelCursor {
    /// Files for this level, sorted ascending (the `LTXFiles` contract).
    files: Vec<FileInfo>,
    /// Read index into `files` (the iterator cursor).
    idx: usize,
    /// Last item read but not yet evaluated (`current`, replica.go:1542).
    current: Option<FileInfo>,
    /// Best eligible file at this level for the current `current_max`
    /// (`candidate`, replica.go:1544).
    candidate: Option<FileInfo>,
    /// True once the iterator is exhausted (`done`, replica.go:1546).
    done: bool,
}

impl RestoreLevelCursor {
    fn new(files: Vec<FileInfo>) -> Self {
        RestoreLevelCursor {
            files,
            idx: 0,
            current: None,
            candidate: None,
            done: false,
        }
    }

    /// Advances the iterator while files could be contiguous with `current_max`,
    /// keeping the best eligible candidate. Ported from `refresh`
    /// (replica.go:1549-1587).
    fn refresh(&mut self, current_max: TXID, txid: TXID) {
        if self.done {
            return;
        }
        if let Some(c) = &self.candidate {
            if c.max_txid <= current_max {
                self.candidate = None;
            }
        }

        loop {
            self.ensure_current();
            if self.done {
                return;
            }

            let info = self.current.clone().unwrap();
            if info.min_txid.0 > current_max.0 + 1 {
                return;
            }
            self.current = None;

            if info.max_txid <= current_max {
                continue;
            }
            if txid != TXID(0) && info.max_txid > txid {
                continue;
            }

            match &self.candidate {
                None => self.candidate = Some(info),
                Some(c) => {
                    if restore_candidate_better(c, &info) {
                        self.candidate = Some(info);
                    }
                }
            }
        }
    }

    /// Populates `current` with the next item, or marks `done`. Ported from
    /// `ensureCurrent` (replica.go:1589-1603).
    fn ensure_current(&mut self) {
        if self.done || self.current.is_some() {
            return;
        }
        if self.idx >= self.files.len() {
            self.done = true;
            return;
        }
        self.current = Some(self.files[self.idx].clone());
        self.idx += 1;
    }
}

/// True if `next` is a strictly better restore candidate than `curr`: longer
/// reach first (`MaxTXID`), then a smaller `MinTXID` (more coverage), then a
/// higher level (more compacted), then an earlier `created_at`.
///
/// Ported from `restoreCandidateBetter` (replica.go:1605-1616).
fn restore_candidate_better(curr: &FileInfo, next: &FileInfo) -> bool {
    if next.max_txid != curr.max_txid {
        return next.max_txid > curr.max_txid;
    }
    if next.min_txid != curr.min_txid {
        return next.min_txid < curr.min_txid;
    }
    if next.level != curr.level {
        return next.level > curr.level;
    }
    // CreatedAt.Before(curr): an unknown (None) timestamp is treated as not
    // earlier, matching Go's zero-time comparison being false here.
    match (next.created_at, curr.created_at) {
        (Some(n), Some(c)) => n < c,
        _ => false,
    }
}

/// Maximum `max_txid` across a slice, `TXID(0)` if empty. Mirrors
/// `FileInfoSlice.MaxTXID` (ltx.go:612-619); the slice here is already in plan
/// (ascending) order, but we scan defensively rather than assume.
fn slice_max_txid(infos: &[FileInfo]) -> TXID {
    infos.iter().map(|f| f.max_txid).max().unwrap_or(TXID(0))
}

/// Merges the LTX `files` (a snapshot followed by incrementals, in plan order)
/// and reconstructs the full SQLite database image.
///
/// This replaces Go's `ltx.NewCompactor(pw, rdrs)` + `Decoder.DecodeDatabaseTo`
/// (replica.go:667-682). The compactor's contract (ltx@v0.5.1 compactor.go):
///   * page sizes must match across inputs;
///   * the **last** input's `Commit` is the final database size;
///   * for each page number, the **latest** input that carries it wins
///     (compactor.go:198-228 iterates inputs newest-first);
///   * pages numbered beyond the final `Commit` are dropped (truncation).
///
/// `DecodeDatabaseTo` then writes pages `1..=Commit`, zero-filling the lock page
/// (decoder.go:236-254). Every input is fully decoded + checksum-verified first
/// (`decode_file`), so a corrupt download is caught here.
fn build_database_image(files: &[Vec<u8>]) -> Result<Vec<u8>> {
    // Decode + verify each input, gathering its header and pages.
    let mut headers: Vec<ltx::Header> = Vec::with_capacity(files.len());
    let mut page_sets: Vec<Vec<(u32, Vec<u8>)>> = Vec::with_capacity(files.len());
    for data in files {
        let decoded = ltx::decode_file(data)?;
        let pages = ltx::decode_file_pages(data)?;
        headers.push(decoded.header);
        page_sets.push(pages);
    }

    // Validate page sizes match across inputs (compactor.go:95-97).
    let page_size = headers[0].page_size;
    for h in &headers {
        if h.page_size != page_size {
            return Err(Error::Other(
                format!(
                    "input files have mismatched page sizes: {} != {}",
                    page_size, h.page_size
                )
                .into(),
            ));
        }
    }

    // The final database size is the last input's Commit (compactor.go:108-118).
    let commit = headers[headers.len() - 1].commit;
    let page_size_usize = page_size as usize;
    let lock = ltx::lock_pgno(page_size);

    // Merge: for each page number, the latest input that carries it wins. Iterate
    // inputs in order, overwriting — the last write for a pgno is the newest
    // input's, matching the compactor's newest-first selection (compactor.go:203-224).
    // Pages beyond the final Commit are skipped (compactor.go:217).
    let mut merged: HashMap<u32, Vec<u8>> = HashMap::new();
    for pages in &page_sets {
        for (pgno, data) in pages {
            if *pgno > commit {
                continue; // out of range of the final database size
            }
            merged.insert(*pgno, data.clone());
        }
    }

    // Reconstruct the database image: pages 1..=commit, lock page zero-filled
    // (decoder.go:236-254). A non-lock page missing from the merge means the
    // backup chain is incomplete.
    let mut image = Vec::with_capacity(commit as usize * page_size_usize);
    let zero_page = vec![0u8; page_size_usize];
    for pgno in 1..=commit {
        if pgno == lock {
            image.extend_from_slice(&zero_page);
            continue;
        }
        match merged.get(&pgno) {
            Some(data) => image.extend_from_slice(data),
            None => {
                return Err(Error::Other(
                    format!("missing page {pgno} in restore plan (incomplete backup)").into(),
                ));
            }
        }
    }

    Ok(image)
}

/// Appends `ext` to `path` as a literal suffix (`p` → `p.tmp`), the way Go's
/// `opt.OutputPath + ".tmp"` does (NOT `Path::set_extension`, which would replace
/// an existing extension).
fn append_ext(path: &Path, ext: &str) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(".");
    s.push(ext);
    PathBuf::from(s)
}

/// Writes `data` to `tmp_path`, fsyncs, then renames onto `final_path` — the
/// crash-consistent atomic-write idiom (replica.go:661-694). On any failure the
/// temp file is removed.
fn write_file_atomic(tmp_path: &Path, final_path: &Path, data: &[u8]) -> Result<()> {
    use std::io::Write;
    let result = (|| -> Result<()> {
        let mut f = std::fs::File::create(tmp_path)?;
        f.write_all(data)?;
        f.sync_all()?;
        drop(f);
        std::fs::rename(tmp_path, final_path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(tmp_path);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::file::FileReplicaClient;
    use crate::ltx::{Header, HEADER_FLAG_NO_CHECKSUM, VERSION};

    /// Builds a single-page incremental LTX file (`NoChecksum`), mirroring the Go
    /// `mustBuildIncrementalLTX` helper (replica_internal_test.go:285-315): one
    /// page `pgno` filled with `fill`, `commit = pgno`.
    fn build_incremental_ltx(
        min_txid: TXID,
        max_txid: TXID,
        page_size: u32,
        pgno: u32,
        fill: u8,
    ) -> Vec<u8> {
        let header = Header {
            version: VERSION,
            flags: HEADER_FLAG_NO_CHECKSUM,
            page_size,
            commit: pgno,
            min_txid,
            max_txid,
            timestamp: 1_000,
            pre_apply_checksum: 0,
            wal_offset: 0,
            wal_size: 0,
            wal_salt1: 0,
            wal_salt2: 0,
            node_id: 0,
        };
        let pages = vec![(pgno, vec![fill; page_size as usize])];
        ltx::encode_file(&header, &pages, 0).expect("encode incremental ltx")
    }

    /// Builds a snapshot LTX file (MinTXID==1, tracked checksum) covering pages
    /// `1..=commit`, each page filled with a per-page byte. The lock page is
    /// skipped, exactly as the capture loop does.
    fn build_snapshot_ltx(max_txid: TXID, page_size: u32, commit: u32) -> Vec<u8> {
        let lock = ltx::lock_pgno(page_size);
        let mut pages: Vec<(u32, Vec<u8>)> = Vec::new();
        let mut rolling = crate::CHECKSUM_FLAG;
        for pgno in 1..=commit {
            if pgno == lock {
                continue;
            }
            let data = vec![(pgno & 0xff) as u8; page_size as usize];
            rolling = crate::CHECKSUM_FLAG | (rolling ^ ltx::checksum_page(pgno, &data));
            pages.push((pgno, data));
        }
        let header = Header {
            version: VERSION,
            flags: 0,
            page_size,
            commit,
            min_txid: TXID(1),
            max_txid,
            timestamp: 1_000,
            pre_apply_checksum: 0,
            wal_offset: 0,
            wal_size: 0,
            wal_salt1: 0,
            wal_salt2: 0,
            node_id: 0,
        };
        ltx::encode_file(&header, &pages, rolling).expect("encode snapshot ltx")
    }

    // Port of TestReplica_UploadLTXFile_OpenErrorReturnsLTXError/MissingFile
    // (replica_internal_test.go:143-160): a missing local LTX file makes
    // uploadLTXFile return an LTXError{op:"open"}.
    #[tokio::test]
    async fn upload_ltx_file_missing_returns_ltx_error() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(dir.path().join("test.db")).unwrap();
        let client =
            FileReplicaClient::new(dir.path().join("replica").to_string_lossy().into_owned());
        let mut r = Replica::new(db, client);

        // No local LTX file for TXID 1 exists yet (we never synced the Db).
        let err = r
            .upload_ltx_file(0, TXID(1), TXID(1))
            .await
            .expect_err("expected error for missing LTX file");
        match err {
            Error::Ltx(e) => assert_eq!(e.op, "open", "op must be open"),
            other => panic!("expected LTXError, got {other:?}"),
        }
    }

    // build_database_image merges a snapshot + incrementals like the compactor:
    // the latest input wins per page, and the final Commit bounds the size.
    #[test]
    fn build_image_latest_page_wins_and_commit_bounds() {
        let page_size = 512u32;
        // Snapshot: 3 pages (commit=3), page bytes 1,2,3.
        let snap = build_snapshot_ltx(TXID(1), page_size, 3);
        // Incremental at TXID 2 rewrites page 2 to 0xEE (commit stays 2 in the
        // helper, but the final image size is the LAST input's commit).
        let inc2 = build_incremental_ltx(TXID(2), TXID(2), page_size, 2, 0xEE);
        // Incremental at TXID 3 grows to page 4 (commit=4), page 4 = 0x44.
        let inc3 = build_incremental_ltx(TXID(3), TXID(3), page_size, 4, 0x44);

        let image = build_database_image(&[snap, inc2, inc3]).expect("merge");
        assert_eq!(
            image.len(),
            4 * page_size as usize,
            "final commit=4 bounds size"
        );

        let page =
            |n: u32| &image[(n - 1) as usize * page_size as usize..n as usize * page_size as usize];
        assert_eq!(page(1)[0], 1, "page 1 from snapshot");
        assert_eq!(
            page(2)[0],
            0xEE,
            "page 2 overwritten by the latest input (TXID 2)"
        );
        assert_eq!(page(3)[0], 3, "page 3 from snapshot, untouched");
        assert_eq!(page(4)[0], 0x44, "page 4 added by TXID 3");
    }

    // A page beyond the final commit is dropped (compactor truncation,
    // compactor.go:217).
    #[test]
    fn build_image_drops_pages_beyond_final_commit() {
        let page_size = 512u32;
        // Snapshot has 5 pages; the final incremental shrinks commit to 2.
        let snap = build_snapshot_ltx(TXID(1), page_size, 5);
        let inc = build_incremental_ltx(TXID(2), TXID(2), page_size, 2, 0xAB);
        let image = build_database_image(&[snap, inc]).expect("merge");
        assert_eq!(
            image.len(),
            2 * page_size as usize,
            "shrunk to commit=2; pages 3-5 dropped"
        );
    }

    // restore() reconstructs a DB byte-image equal to decoding the snapshot alone
    // when there are no incrementals (the single-file plan).
    #[test]
    fn build_image_single_snapshot_matches_decode_database_image() {
        let page_size = 1024u32;
        let snap = build_snapshot_ltx(TXID(1), page_size, 4);
        let via_merge = build_database_image(std::slice::from_ref(&snap)).expect("merge");
        let via_decoder = ltx::decode_database_image(&snap).expect("decode image");
        assert_eq!(
            via_merge, via_decoder,
            "single-file merge == DecodeDatabaseTo"
        );
    }

    // A corrupt input is rejected by build_database_image (decode_file verifies
    // the file checksum before any page is used).
    #[test]
    fn build_image_rejects_corrupt_input() {
        let page_size = 512u32;
        let mut snap = build_snapshot_ltx(TXID(1), page_size, 2);
        let mid = ltx::HEADER_SIZE + (snap.len() - ltx::HEADER_SIZE) / 2;
        snap[mid] ^= 0x01;
        let err = build_database_image(&[snap]).expect_err("corruption must be caught");
        assert!(
            matches!(err, Error::ChecksumMismatch | Error::LTXCorrupted),
            "corrupt LTX rejected, got {err:?}"
        );
    }

    // calc_restore_plan over a file client with an L0-only chain returns the
    // contiguous snapshot+incrementals in TXID order, and refusing to restore
    // when nothing is present returns TxNotAvailable.
    #[tokio::test]
    async fn calc_restore_plan_l0_chain_and_empty() {
        let dir = tempfile::tempdir().unwrap();
        let client = FileReplicaClient::new(dir.path().to_string_lossy().into_owned());

        // Empty replica -> TxNotAvailable.
        let err = calc_restore_plan(&client, TXID(0))
            .await
            .expect_err("empty replica");
        assert!(
            matches!(err, Error::TxNotAvailable),
            "empty -> TxNotAvailable"
        );

        // Write a snapshot at TXID 1 (L0) + two incrementals.
        let page_size = 512u32;
        let snap = build_snapshot_ltx(TXID(1), page_size, 2);
        client
            .write_ltx_file(0, TXID(1), TXID(1), &snap)
            .await
            .unwrap();
        let inc2 = build_incremental_ltx(TXID(2), TXID(2), page_size, 2, 0x22);
        client
            .write_ltx_file(0, TXID(2), TXID(2), &inc2)
            .await
            .unwrap();
        let inc3 = build_incremental_ltx(TXID(3), TXID(3), page_size, 2, 0x33);
        client
            .write_ltx_file(0, TXID(3), TXID(3), &inc3)
            .await
            .unwrap();

        let plan = calc_restore_plan(&client, TXID(0)).await.expect("plan");
        let txids: Vec<u64> = plan.iter().map(|f| f.max_txid.0).collect();
        assert_eq!(txids, vec![1, 2, 3], "contiguous L0 chain in TXID order");

        // Targeted restore to TXID 2 stops the plan at 2.
        let plan2 = calc_restore_plan(&client, TXID(2)).await.expect("plan@2");
        let txids2: Vec<u64> = plan2.iter().map(|f| f.max_txid.0).collect();
        assert_eq!(txids2, vec![1, 2], "plan capped at the target TXID");
    }

    // After a continuity break, the L0-only capture loop re-emits a FULL snapshot
    // at a later TXID (MinTXID==1, MaxTXID==N) alongside the earlier per-txn
    // files. calc_restore_plan must select that wider snapshot (it extends the
    // longest contiguous range), exactly as the compactor needs — and the merged
    // image must reconstruct correctly from snapshot 1-3 + incremental 4.
    #[tokio::test]
    async fn calc_restore_plan_prefers_wider_reemitted_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let client = FileReplicaClient::new(dir.path().to_string_lossy().into_owned());
        let page_size = 512u32;

        // Original chain: snapshot 1-1 + incrementals 2,3 (each rewrites page 2).
        client
            .write_ltx_file(
                0,
                TXID(1),
                TXID(1),
                &build_snapshot_ltx(TXID(1), page_size, 2),
            )
            .await
            .unwrap();
        client
            .write_ltx_file(
                0,
                TXID(2),
                TXID(2),
                &build_incremental_ltx(TXID(2), TXID(2), page_size, 2, 0x22),
            )
            .await
            .unwrap();
        client
            .write_ltx_file(
                0,
                TXID(3),
                TXID(3),
                &build_incremental_ltx(TXID(3), TXID(3), page_size, 2, 0x33),
            )
            .await
            .unwrap();
        // Continuity break: a re-emitted snapshot 1-3 (covers the whole DB at TXID 3).
        client
            .write_ltx_file(
                0,
                TXID(1),
                TXID(3),
                &build_snapshot_ltx(TXID(3), page_size, 2),
            )
            .await
            .unwrap();
        // A new incremental on top of the re-emitted snapshot.
        client
            .write_ltx_file(
                0,
                TXID(4),
                TXID(4),
                &build_incremental_ltx(TXID(4), TXID(4), page_size, 2, 0x44),
            )
            .await
            .unwrap();

        let plan = calc_restore_plan(&client, TXID(0)).await.expect("plan");
        // The wider snapshot (1-3) is chosen over snapshot 1-1, then incremental 4
        // extends it — a contiguous, minimal chain reaching TXID 4.
        let pairs: Vec<(u64, u64)> = plan.iter().map(|f| (f.min_txid.0, f.max_txid.0)).collect();
        assert_eq!(
            pairs,
            vec![(1, 3), (4, 4)],
            "wider re-emitted snapshot anchors the plan; incremental 4 extends it"
        );

        // And the merge reconstructs: page 2 takes incremental 4's 0x44.
        let mut datas = Vec::new();
        for info in &plan {
            datas.push(
                client
                    .open_ltx_file(info.level, info.min_txid, info.max_txid, 0, 0)
                    .await
                    .unwrap(),
            );
        }
        let image = build_database_image(&datas).expect("merge re-emitted chain");
        assert_eq!(image.len(), 2 * page_size as usize, "commit=2");
        assert_eq!(
            image[page_size as usize], 0x44,
            "page 2 reflects the newest incremental"
        );
    }
}
