//! store.rs — snapshot/TXID bookkeeping + retention selection.
//!
//! Ported from the `Enforce*Retention*` algorithms in litestream@v0.5.11
//! `compactor.go` (`EnforceRetentionByTXID`) and `db.go`
//! (`EnforceSnapshotRetention`, `EnforceL0RetentionByTime`).
//!
//! # Scope (KEEP set, PLAN.md §2)
//! The upstream `Store` is mostly multi-DB orchestration, **compaction** level
//! monitors, heartbeats, and validation — all OUT of the one-shot scope
//! (L0-only, single replica, no compaction). What remains and matters here is
//! the **retention selection** logic: which LTX files to delete, with the exact
//! boundary semantics that are easy to get subtly wrong (strict `<`, keep the
//! newest file, stop early to preserve a contiguous tail). These are extracted
//! as pure, testable functions operating on a sorted `&[FileInfo]`.

use crate::ltx::FileInfo;
use crate::TXID;
use std::time::SystemTime;

/// Returns the maximum `max_txid` across a sorted file slice, or `TXID(0)` if
/// empty. Ported from `FileInfoSlice.MaxTXID` (ltx.go:612-619).
pub fn max_txid(files: &[FileInfo]) -> TXID {
    files.last().map(|f| f.max_txid).unwrap_or(TXID(0))
}

/// The keep-newest invariant shared by every retention path: if the last
/// file marked for deletion is the newest file overall, un-mark it so at least
/// one file always survives. Mirrors the Go idiom
/// `if deleted[len-1] == lastInfo { deleted = deleted[:len-1] }`.
fn keep_newest(deleted: &mut Vec<FileInfo>, files: &[FileInfo]) {
    if let (Some(last_del), Some(last)) = (deleted.last(), files.last()) {
        if last_del == last {
            deleted.pop();
        }
    }
}

/// Selects files at a level whose `max_txid` is **strictly below** `txid` for
/// deletion, always keeping at least the newest file.
///
/// Ported from `Compactor.EnforceRetentionByTXID` (compactor.go:288-313).
/// `files` must be sorted ascending by txid (the `LTXFiles` iterator contract).
/// NOTE the inequality is strict `<`, not `<=` — a file whose `max_txid == txid`
/// is retained (it is still needed to reach `txid`).
pub fn select_retention_by_txid(files: &[FileInfo], txid: TXID) -> Vec<FileInfo> {
    let mut deleted: Vec<FileInfo> = files
        .iter()
        .filter(|f| f.max_txid < txid)
        .cloned()
        .collect();
    keep_newest(&mut deleted, files);
    deleted
}

/// Selects snapshot files created strictly **before** `before` for deletion and
/// returns `(deleted, min_retained_snapshot_txid)`. Always keeps the newest
/// snapshot. The returned TXID is the lowest `max_txid` among *retained*
/// snapshots (used to cascade retention into lower levels); `TXID(0)` if none.
///
/// Ported from `DB.EnforceSnapshotRetention` (db.go:2063-2116). A file with an
/// unknown (`None`) timestamp is conservatively retained (least-data-loss).
pub fn select_snapshot_retention(files: &[FileInfo], before: SystemTime) -> (Vec<FileInfo>, TXID) {
    let mut deleted = Vec::new();
    let mut min_snapshot_txid = TXID(0);
    for f in files {
        // CreatedAt.Before(timestamp)
        if f.created_at.is_some_and(|t| t < before) {
            deleted.push(f.clone());
            continue;
        }
        // Track the lowest retained snapshot txid.
        if min_snapshot_txid == TXID(0) || f.max_txid < min_snapshot_txid {
            min_snapshot_txid = f.max_txid;
        }
    }
    keep_newest(&mut deleted, files);
    (deleted, min_snapshot_txid)
}

/// Selects L0 files eligible for time-based retention: files older than
/// `threshold` that have already been compacted into L1 (`max_txid <=
/// max_l1_txid`). Stops at the first file newer than `threshold` to preserve a
/// contiguous retained tail, and only applies the keep-newest rule when the
/// whole level was processed.
///
/// Ported from `DB.EnforceL0RetentionByTime` (db.go:2118-2205).
///
/// In the KEEP scope there is **no compaction**, so `max_l1_txid` is always
/// `TXID(0)` and this returns empty — L0 files accumulate (Risk R-3, accepted:
/// correct, just a slower restore on a long-lived DB). The full algorithm is
/// ported anyway so the behavior is correct the moment compaction is added.
pub fn select_l0_retention_by_time(
    l0: &[FileInfo],
    max_l1_txid: TXID,
    threshold: SystemTime,
) -> Vec<FileInfo> {
    if max_l1_txid == TXID(0) {
        return Vec::new();
    }

    let mut deleted = Vec::new();
    let mut processed_all = true;
    for f in l0 {
        // Missing timestamps fall back to the threshold (db.go:2171-2178).
        let created = f.created_at.unwrap_or(threshold);
        if created > threshold {
            // CreatedAt.After(threshold): L0 is ordered, so once we reach a
            // newer file we stop — deleting past it would create a gap.
            processed_all = false;
            break;
        }
        if f.max_txid <= max_l1_txid {
            deleted.push(f.clone());
        }
        // else: not yet compacted into L1 — retain it.
    }

    if processed_all {
        keep_newest(&mut deleted, l0);
    }
    deleted
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn fi(min: u64, max: u64, created: Option<SystemTime>) -> FileInfo {
        FileInfo {
            level: 0,
            min_txid: TXID(min),
            max_txid: TXID(max),
            size: 1,
            created_at: created,
            ..Default::default()
        }
    }

    fn files(n: u64) -> Vec<FileInfo> {
        (1..=n).map(|i| fi(i, i, None)).collect()
    }

    #[test]
    fn retention_by_txid_uses_strict_less_than() {
        let f = files(5); // txids 1..5
                          // Below 3 => {1,2} (file 3 has max_txid==3, which is NOT < 3, so retained).
        let del = select_retention_by_txid(&f, TXID(3));
        assert_eq!(
            del.iter().map(|x| x.max_txid.0).collect::<Vec<_>>(),
            vec![1, 2],
            "strict <: max_txid==txid is kept"
        );
    }

    #[test]
    fn retention_by_txid_always_keeps_newest() {
        let f = files(3);
        // Everything is below 100, but the newest (3) must survive.
        let del = select_retention_by_txid(&f, TXID(100));
        assert_eq!(
            del.iter().map(|x| x.max_txid.0).collect::<Vec<_>>(),
            vec![1, 2],
            "keep-newest: file 3 retained"
        );
    }

    #[test]
    fn retention_by_txid_deletes_nothing_when_all_above() {
        let f = files(3);
        assert!(select_retention_by_txid(&f, TXID(1)).is_empty());
    }

    #[test]
    fn snapshot_retention_by_time_keeps_newest_and_tracks_min() {
        let now = SystemTime::now();
        let old = now - Duration::from_secs(3600);
        let recent = now - Duration::from_secs(10);
        let before = now - Duration::from_secs(60);
        // Two old snapshots + one recent; cutoff `before` is 60s ago.
        let f = vec![
            fi(1, 1, Some(old)),
            fi(2, 2, Some(old)),
            fi(3, 3, Some(recent)),
        ];
        let (del, min_txid) = select_snapshot_retention(&f, before);
        assert_eq!(
            del.iter().map(|x| x.max_txid.0).collect::<Vec<_>>(),
            vec![1, 2],
            "old snapshots deleted, recent retained"
        );
        assert_eq!(min_txid, TXID(3), "lowest retained snapshot txid");
    }

    #[test]
    fn snapshot_retention_keeps_newest_even_if_all_old() {
        let now = SystemTime::now();
        let old = now - Duration::from_secs(3600);
        let before = now - Duration::from_secs(60);
        let f = vec![fi(1, 1, Some(old)), fi(2, 2, Some(old))];
        let (del, _) = select_snapshot_retention(&f, before);
        assert_eq!(
            del.iter().map(|x| x.max_txid.0).collect::<Vec<_>>(),
            vec![1],
            "keep-newest applies to snapshots too"
        );
    }

    #[test]
    fn l0_retention_is_noop_without_compaction() {
        // KEEP scope: no L1 => max_l1_txid 0 => never deletes.
        let f = files(10);
        let threshold = SystemTime::now();
        assert!(select_l0_retention_by_time(&f, TXID(0), threshold).is_empty());
    }

    #[test]
    fn l0_retention_stops_early_on_recent_file() {
        let now = SystemTime::now();
        let old = now - Duration::from_secs(3600);
        let recent = now - Duration::from_secs(1);
        let threshold = now - Duration::from_secs(60);
        // Files 1,2 are old; file 3 is recent (after threshold) -> stop.
        // All have max_txid <= max_l1_txid (5), so 1,2 are eligible; we stop at 3.
        let f = vec![
            fi(1, 1, Some(old)),
            fi(2, 2, Some(old)),
            fi(3, 3, Some(recent)),
            fi(4, 4, Some(old)), // would be eligible, but we stopped before it
        ];
        let del = select_l0_retention_by_time(&f, TXID(5), threshold);
        assert_eq!(
            del.iter().map(|x| x.max_txid.0).collect::<Vec<_>>(),
            vec![1, 2],
            "stop-early preserves the contiguous tail; never reaches file 4"
        );
    }

    #[test]
    fn l0_retention_only_deletes_compacted_files() {
        let now = SystemTime::now();
        let old = now - Duration::from_secs(3600);
        let threshold = now - Duration::from_secs(60);
        // max_l1_txid = 2: only files with max_txid <= 2 are eligible.
        let f = vec![
            fi(1, 1, Some(old)),
            fi(2, 2, Some(old)),
            fi(3, 3, Some(old)), // not compacted into L1 yet -> retained
        ];
        let del = select_l0_retention_by_time(&f, TXID(2), threshold);
        assert_eq!(
            del.iter().map(|x| x.max_txid.0).collect::<Vec<_>>(),
            vec![1, 2],
            "only L1-compacted files are eligible"
        );
    }

    #[test]
    fn max_txid_of_slice() {
        assert_eq!(max_txid(&files(7)), TXID(7));
        assert_eq!(max_txid(&[]), TXID(0));
    }
}
