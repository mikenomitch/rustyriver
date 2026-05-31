//! client::file — file-backed `ReplicaClient`.
//!
//! Ported from litestream@v0.5.11 `file/replica_client.go` (the v0.5 methods
//! only; the `*V3` legacy generation shim is dropped, PLAN.md §2). On-disk
//! layout matches upstream: `<root>/ltx/<level>/<minTXID>-<maxTXID>.ltx`, the
//! same tree the golden fixtures were captured from.

use crate::error::{Error, Result};
use crate::ltx::{self, FileInfo};
use crate::{ltx_file_path, ltx_level_dir, TXID};
use async_trait::async_trait;
use std::time::{Duration, UNIX_EPOCH};

use super::ReplicaClient;

/// A `ReplicaClient` that stores LTX files on the local filesystem.
#[derive(Debug, Clone)]
pub struct FileReplicaClient {
    path: String,
}

impl FileReplicaClient {
    /// Creates a client rooted at `path` (the replica destination directory).
    pub fn new(path: impl Into<String>) -> Self {
        FileReplicaClient { path: path.into() }
    }

    /// The replica destination path.
    pub fn path(&self) -> &str {
        &self.path
    }
}

#[async_trait]
impl ReplicaClient for FileReplicaClient {
    fn type_name(&self) -> &str {
        "file"
    }

    async fn ltx_files(
        &self,
        level: i32,
        seek: TXID,
        _use_metadata: bool,
    ) -> Result<Vec<FileInfo>> {
        let dir = ltx_level_dir(&self.path, level as u32);
        let mut rd = match tokio::fs::read_dir(&dir).await {
            Ok(rd) => rd,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e.into()),
        };

        let mut infos = Vec::new();
        while let Some(entry) = rd.next_entry().await? {
            let name = entry.file_name().to_string_lossy().into_owned();
            // ModTime is the timestamp set at write time; skip non-LTX names.
            let (min_txid, max_txid) = match ltx::parse_filename(&name) {
                Ok(t) => t,
                Err(_) => continue,
            };
            if min_txid < seek {
                continue;
            }
            let meta = entry.metadata().await?;
            infos.push(FileInfo {
                level,
                min_txid,
                max_txid,
                size: meta.len() as i64,
                created_at: meta.modified().ok(),
                ..Default::default()
            });
        }

        // Iterator contract: ascending by (level, min_txid, max_txid)
        // (ltx.NewFileInfoSliceIterator sorts the slice).
        infos.sort_by(|a, b| {
            (a.level, a.min_txid.0, a.max_txid.0).cmp(&(b.level, b.min_txid.0, b.max_txid.0))
        });
        Ok(infos)
    }

    async fn open_ltx_file(
        &self,
        level: i32,
        min_txid: TXID,
        max_txid: TXID,
        offset: i64,
        size: i64,
    ) -> Result<Vec<u8>> {
        let path = ltx_file_path(&self.path, level as u32, min_txid, max_txid);
        // NotFound is preserved so callers can classify auto-recoverable errors.
        let bytes = tokio::fs::read(&path).await.map_err(Error::Io)?;

        let off = offset.max(0) as usize;
        if off >= bytes.len() {
            return Ok(Vec::new());
        }
        // size == 0 means "read to end of file" (NOT zero bytes).
        let end = if size <= 0 {
            bytes.len()
        } else {
            (off + size as usize).min(bytes.len())
        };
        Ok(bytes[off..end].to_vec())
    }

    async fn write_ltx_file(
        &self,
        level: i32,
        min_txid: TXID,
        max_txid: TXID,
        data: &[u8],
    ) -> Result<FileInfo> {
        // Peek the LTX header timestamp (preserved as the file's creation time).
        let header = ltx::Header::parse(data)?;
        let created_at = Some(UNIX_EPOCH + Duration::from_millis(header.timestamp.max(0) as u64));

        let filename = ltx_file_path(&self.path, level as u32, min_txid, max_txid);
        let dir = ltx_level_dir(&self.path, level as u32);
        tokio::fs::create_dir_all(&dir).await?;

        // Write to a temp file then atomically rename; clean the temp on error.
        let tmp = format!("{filename}.tmp");
        if let Err(e) = write_then_rename(&tmp, &filename, data).await {
            let _ = tokio::fs::remove_file(&tmp).await;
            return Err(e);
        }

        Ok(FileInfo {
            level,
            min_txid,
            max_txid,
            size: data.len() as i64,
            created_at,
            ..Default::default()
        })
    }

    async fn delete_ltx_files(&self, files: &[FileInfo]) -> Result<()> {
        for info in files {
            let path = ltx_file_path(&self.path, info.level as u32, info.min_txid, info.max_txid);
            match tokio::fs::remove_file(&path).await {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
        }
        Ok(())
    }

    async fn delete_all(&self) -> Result<()> {
        match tokio::fs::remove_dir_all(&self.path).await {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }
}

/// Writes `data` to `tmp`, fsyncs it, and renames it onto `final_path`.
async fn write_then_rename(tmp: &str, final_path: &str, data: &[u8]) -> Result<()> {
    use tokio::io::AsyncWriteExt;
    let mut f = tokio::fs::File::create(tmp).await?;
    f.write_all(data).await?;
    f.sync_all().await?;
    drop(f);
    tokio::fs::rename(tmp, final_path).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::run_client_suite;

    #[tokio::test]
    async fn passes_conformance_suite() {
        let dir = tempfile::tempdir().unwrap();
        let client = FileReplicaClient::new(dir.path().to_string_lossy().into_owned());
        run_client_suite(&client).await;
    }

    #[test]
    fn type_name_is_file() {
        assert_eq!(FileReplicaClient::new("/x").type_name(), "file");
    }

    // The file client reads the real golden replica tree (captured from the
    // litestream binary): 6 L0 files, in order, each decoding byte-exact.
    #[tokio::test]
    async fn lists_and_reads_golden_replica() {
        let root = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/golden/replica");
        let client = FileReplicaClient::new(root);

        let files = client.ltx_files(0, TXID(0), false).await.unwrap();
        assert_eq!(files.len(), 6, "golden L0 file count");
        let order: Vec<u64> = files.iter().map(|f| f.min_txid.0).collect();
        assert_eq!(order, vec![1, 2, 3, 4, 5, 6], "ascending by txid");

        for f in &files {
            let bytes = client
                .open_ltx_file(0, f.min_txid, f.max_txid, 0, 0)
                .await
                .unwrap();
            assert_eq!(bytes.len() as i64, f.size, "read size matches listing");
            let decoded = ltx::decode_file(&bytes).expect("golden file decodes via client");
            assert_eq!(decoded.header.min_txid, f.min_txid);
        }

        // Partial read of the page-index tail (the restore fast-path).
        let f = &files[0];
        let tail = client
            .open_ltx_file(0, f.min_txid, f.max_txid, f.size - 24, 0)
            .await
            .unwrap();
        assert_eq!(tail.len(), 24, "tail read returns last 24 bytes");
    }
}
