//! wal.rs — SQLite WAL header/frame parsing + cumulative SQLite checksums.
//!
//! Ported from litestream@v0.5.11 wal_reader.go:14-295 (read path) and the
//! `WALChecksum` helper in litestream@v0.5.11 litestream.go:110-119 (re-exported
//! from the crate root as [`crate::wal_checksum`]).
//!
//! A [`WalReader`] wraps a byte buffer and parses SQLite WAL frames, verifying
//! salt and cumulative-checksum integrity as it reads. It does **not** enforce
//! transaction boundaries (it may return uncommitted frames); honoring commit
//! records is the caller's responsibility (see [`WalReader::page_map`]).
//!
//! ## Byte format (read straight from the SQLite WAL spec; see wal_reader.go)
//!
//! WAL header (32 bytes, all fields big-endian at fixed offsets):
//! ```text
//!   [0..4]   magic       0x377f0682 => checksums LITTLE-endian
//!                        0x377f0683 => checksums BIG-endian
//!   [4..8]   version     must equal 3007000
//!   [8..12]  page size
//!   [12..16] checkpoint sequence number
//!   [16..20] salt-1
//!   [20..24] salt-2
//!   [24..28] checksum-1  (cumulative checksum of bytes [0..24])
//!   [28..32] checksum-2
//! ```
//!
//! WAL frame header (24 bytes, all fields big-endian):
//! ```text
//!   [0..4]   page number
//!   [4..8]   commit size in pages for a commit record, else 0
//!   [8..12]  salt-1  (must match the header salt-1)
//!   [12..16] salt-2  (must match the header salt-2)
//!   [16..20] checksum-1  (cumulative checksum through this frame's data)
//!   [20..24] checksum-2
//! ```
//!
//! A frame is `pageSize + 24` bytes. The cumulative checksum is seeded with the
//! header checksum and rolled forward over each frame's 8-byte header prefix and
//! then its page data, in the byte order chosen by the magic.

use std::collections::HashMap;

use crate::{wal_checksum, WAL_FRAME_HEADER_SIZE, WAL_HEADER_SIZE};

/// Required WAL format version (`3007000`), as found at header offset 4.
///
/// Ported from litestream@v0.5.11 wal_reader.go:118.
pub const WAL_VERSION: u32 = 3_007_000;

/// WAL header magic indicating checksums are computed **little-endian**.
///
/// Ported from litestream@v0.5.11 wal_reader.go:101.
pub const WAL_MAGIC_LITTLE_ENDIAN: u32 = 0x377f_0682;

/// WAL header magic indicating checksums are computed **big-endian**.
///
/// Ported from litestream@v0.5.11 wal_reader.go:103.
pub const WAL_MAGIC_BIG_ENDIAN: u32 = 0x377f_0683;

// ── Errors ────────────────────────────────────────────────────────────────────

/// Errors returned by [`WalReader`].
///
/// The Go reader leans on `io.EOF` as a control-flow sentinel: it signals the
/// clean end of the *valid* WAL, but also a short/partial read, a salt mismatch,
/// or a checksum mismatch — all of which mean "stop reading here, the rest of the
/// file is not a valid continuation." We model that single sentinel as
/// [`WalError::Eof`] so callers can branch on it exactly like Go's
/// `errors.Is(err, io.EOF)` (see [`WalError::is_eof`]). Non-EOF variants carry
/// the same human-readable messages as the Go `fmt.Errorf` strings so the ported
/// tests can assert them byte-for-byte.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WalError {
    /// End of the valid WAL.
    ///
    /// Mirrors every `return ..., io.EOF` in wal_reader.go: a partial header, a
    /// failed header checksum, a short frame read, a salt mismatch, or a frame
    /// checksum mismatch. In all of these the WAL has no further valid frames.
    Eof,

    /// `invalid wal header magic: <hex>` — the magic was neither
    /// `0x377f0682` nor `0x377f0683`.
    ///
    /// Ported from litestream@v0.5.11 wal_reader.go:106.
    InvalidMagic(u32),

    /// `unsupported wal version: <n>` — header version field was not `3007000`.
    ///
    /// Ported from litestream@v0.5.11 wal_reader.go:119.
    UnsupportedVersion(u32),

    /// `WALReader.ReadFrame(): buffer size (<n>) must match page size (<m>)`.
    ///
    /// Ported from litestream@v0.5.11 wal_reader.go:139.
    BufferSize {
        /// Length of the buffer the caller supplied.
        got: usize,
        /// The WAL's page size.
        want: u32,
    },
}

impl WalError {
    /// Returns `true` for the [`WalError::Eof`] sentinel.
    ///
    /// This is the analog of Go's `errors.Is(err, io.EOF)`, which the upstream
    /// callers (`PageMap`, the test suite) use to detect the end of the WAL.
    #[inline]
    pub fn is_eof(&self) -> bool {
        matches!(self, WalError::Eof)
    }
}

impl std::fmt::Display for WalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            // Matches the Go string "EOF" (io.EOF.Error()).
            WalError::Eof => f.write_str("EOF"),
            // Go: fmt.Errorf("invalid wal header magic: %x", magic) — lowercase
            // hex, no leading zeros (matches Go's %x for a uint32).
            WalError::InvalidMagic(magic) => write!(f, "invalid wal header magic: {magic:x}"),
            WalError::UnsupportedVersion(v) => write!(f, "unsupported wal version: {v}"),
            WalError::BufferSize { got, want } => write!(
                f,
                "WALReader.ReadFrame(): buffer size ({got}) must match page size ({want})"
            ),
        }
    }
}

impl std::error::Error for WalError {}

impl From<WalError> for crate::Error {
    /// Lifts a [`WalError`] into the crate-wide error type for callers that work
    /// in terms of [`crate::Error`]. EOF and the structured WAL errors become
    /// `Error::Other` carrying the same message.
    fn from(e: WalError) -> Self {
        crate::Error::Other(Box::new(e))
    }
}

/// `WalReader`'s `Result` alias.
pub type WalResult<T> = std::result::Result<T, WalError>;

// ── WalReader ───────────────────────────────────────────────────────────────

/// Reads SQLite WAL frames from an in-memory byte buffer, verifying salts and
/// the cumulative SQLite checksum as it goes.
///
/// This is the faithful analog of Go's `WALReader` (wal_reader.go:19-31). Go
/// wraps an `io.ReaderAt`; the upstream tests always back that with a
/// `bytes.Reader` over the whole WAL, so we wrap a borrowed `&[u8]` and emulate
/// `ReadAt` semantics directly: a read whose requested length runs past the end
/// of the buffer yields fewer bytes, which the algorithm treats as `io.EOF`
/// exactly as Go does.
///
/// Ported from litestream@v0.5.11 wal_reader.go:19-187.
#[derive(Debug)]
pub struct WalReader<'a> {
    /// Backing WAL bytes (the whole file, as the Go `io.ReaderAt`).
    data: &'a [u8],
    /// Index of the *next* frame to read (0-based). Go field `frameN`.
    frame_n: i64,

    /// `true` when checksums are big-endian (magic `0x377f0683`),
    /// `false` when little-endian (magic `0x377f0682`). Go field `bo`.
    big_endian: bool,
    /// Page size from the header. Go field `pageSize`.
    page_size: u32,
    /// Checkpoint sequence number from the header. Go field `seq`.
    #[allow(dead_code)]
    seq: u32,

    /// Header salt-1 / salt-2; frames must match these. Go fields `salt1/salt2`.
    salt1: u32,
    salt2: u32,
    /// Running cumulative checksum. Seeded from the header checksum, then rolled
    /// forward frame by frame. Go fields `chksum1/chksum2`.
    chksum1: u32,
    chksum2: u32,
}

impl<'a> WalReader<'a> {
    /// Creates a new reader over `data`, parsing the WAL header immediately.
    ///
    /// Returns [`WalError::Eof`] if the buffer is too short to hold a header or
    /// the header checksum does not validate (a partial WAL-header write during
    /// checkpointing). Returns [`WalError::InvalidMagic`] /
    /// [`WalError::UnsupportedVersion`] for a malformed header.
    ///
    /// Ported from `NewWALReader` in litestream@v0.5.11 wal_reader.go:34-40.
    pub fn new(data: &'a [u8]) -> WalResult<Self> {
        let mut r = WalReader {
            data,
            frame_n: 0,
            big_endian: false,
            page_size: 0,
            seq: 0,
            salt1: 0,
            salt2: 0,
            chksum1: 0,
            chksum2: 0,
        };
        r.read_header()?;
        Ok(r)
    }

    /// Returns the page size from the header.
    ///
    /// Ported from `PageSize` in litestream@v0.5.11 wal_reader.go:78.
    #[inline]
    pub fn page_size(&self) -> u32 {
        self.page_size
    }

    /// Returns `true` when checksums for this WAL are big-endian (magic
    /// `0x377f0683`), `false` when little-endian (`0x377f0682`).
    ///
    /// Exposed because the byte order is a load-bearing, hard-to-observe property
    /// that the golden test asserts directly.
    #[inline]
    pub fn is_big_endian(&self) -> bool {
        self.big_endian
    }

    /// Returns the header salt pair `(salt1, salt2)`.
    #[inline]
    pub fn salt(&self) -> (u32, u32) {
        (self.salt1, self.salt2)
    }

    /// Returns the file offset of the last frame read, or `0` if no frame has
    /// been read yet.
    ///
    /// Ported from `Offset` in litestream@v0.5.11 wal_reader.go:82-87.
    pub fn offset(&self) -> i64 {
        if self.frame_n == 0 {
            return 0;
        }
        WAL_HEADER_SIZE as i64
            + ((self.frame_n - 1) * (WAL_FRAME_HEADER_SIZE as i64 + self.page_size as i64))
    }

    /// Reads `n` bytes at absolute `offset`, returning `None` (the `io.EOF`
    /// case) when fewer than `n` bytes are available — exactly the behavior of
    /// Go's `io.ReaderAt.ReadAt` over a `bytes.Reader` short read, which the
    /// upstream code converts to `io.EOF`.
    fn read_at(&self, offset: i64, n: usize) -> Option<&'a [u8]> {
        if offset < 0 {
            return None;
        }
        let start = offset as usize;
        let end = start.checked_add(n)?;
        if end > self.data.len() {
            return None;
        }
        Some(&self.data[start..end])
    }

    /// Reads and validates the WAL header into `self`.
    ///
    /// Ported from `readHeader` in litestream@v0.5.11 wal_reader.go:90-129.
    fn read_header(&mut self) -> WalResult<()> {
        // If we have a partial WAL, mark WAL as done (io.EOF).
        let hdr = match self.read_at(0, WAL_HEADER_SIZE) {
            Some(b) => b,
            None => return Err(WalError::Eof),
        };

        // Determine byte order of checksums from the magic (always read
        // big-endian, like Go's binary.BigEndian.Uint32(hdr[0:])).
        let magic = be_u32(&hdr[0..]);
        self.big_endian = match magic {
            WAL_MAGIC_LITTLE_ENDIAN => false,
            WAL_MAGIC_BIG_ENDIAN => true,
            _ => return Err(WalError::InvalidMagic(magic)),
        };

        // If the header checksum doesn't match then we may have failed with a
        // partial WAL header write during checkpointing => io.EOF.
        let chksum1 = be_u32(&hdr[24..]);
        let chksum2 = be_u32(&hdr[28..]);
        let (v0, v1) = wal_checksum(self.big_endian, 0, 0, &hdr[..24]);
        if v0 != chksum1 || v1 != chksum2 {
            return Err(WalError::Eof);
        }

        // Verify version is correct.
        let version = be_u32(&hdr[4..]);
        if version != WAL_VERSION {
            return Err(WalError::UnsupportedVersion(version));
        }

        self.page_size = be_u32(&hdr[8..]);
        self.seq = be_u32(&hdr[12..]);
        self.salt1 = be_u32(&hdr[16..]);
        self.salt2 = be_u32(&hdr[20..]);
        self.chksum1 = chksum1;
        self.chksum2 = chksum2;

        Ok(())
    }

    /// Reads the next frame into `data` and returns `(pgno, commit)`.
    ///
    /// Returns [`WalError::Eof`] at the end of the valid WAL (including on a
    /// salt or checksum mismatch, which terminate the valid region). `data` must
    /// be exactly `page_size` bytes or [`WalError::BufferSize`] is returned.
    ///
    /// Ported from `ReadFrame` in litestream@v0.5.11 wal_reader.go:131-135.
    pub fn read_frame(&mut self, data: &mut [u8]) -> WalResult<(u32, u32)> {
        self.read_frame_inner(data, true)
    }

    /// Frame-read core shared by [`Self::read_frame`] and the offset constructor.
    ///
    /// When `verify_checksum` is `false`, the running checksum is *set* from the
    /// frame's stored checksum rather than verified against a rolling value —
    /// used when seeking to an offset without checksumming from the beginning.
    ///
    /// Ported from `readFrame` in litestream@v0.5.11 wal_reader.go:137-187.
    fn read_frame_inner(
        &mut self,
        data: &mut [u8],
        verify_checksum: bool,
    ) -> WalResult<(u32, u32)> {
        if data.len() != self.page_size as usize {
            return Err(WalError::BufferSize {
                got: data.len(),
                want: self.page_size,
            });
        }

        let frame_size = self.page_size as i64 + WAL_FRAME_HEADER_SIZE as i64;
        let offset = WAL_HEADER_SIZE as i64 + (self.frame_n * frame_size);

        // Read WAL frame header. A short read is io.EOF.
        let hdr = match self.read_at(offset, WAL_FRAME_HEADER_SIZE) {
            Some(b) => b,
            None => return Err(WalError::Eof),
        };

        // Read WAL page data. A short read is io.EOF.
        let page = match self.read_at(offset + WAL_FRAME_HEADER_SIZE as i64, data.len()) {
            Some(b) => b,
            None => return Err(WalError::Eof),
        };
        data.copy_from_slice(page);

        // Verify salt matches the salt in the header; otherwise end of valid WAL.
        let salt1 = be_u32(&hdr[8..]);
        let salt2 = be_u32(&hdr[12..]);
        if self.salt1 != salt1 || self.salt2 != salt2 {
            return Err(WalError::Eof);
        }

        // Verify the cumulative checksum. If verification is disabled, it is
        // because we are jumping to an offset and not checksumming from the
        // beginning, so we simply adopt the frame's stored checksum.
        let chksum1 = be_u32(&hdr[16..]);
        let chksum2 = be_u32(&hdr[20..]);
        if verify_checksum {
            let (c0, c1) = wal_checksum(self.big_endian, self.chksum1, self.chksum2, &hdr[..8]);
            let (c0, c1) = wal_checksum(self.big_endian, c0, c1, data);
            self.chksum1 = c0;
            self.chksum2 = c1;
            if self.chksum1 != chksum1 || self.chksum2 != chksum2 {
                return Err(WalError::Eof);
            }
        } else {
            self.chksum1 = chksum1;
            self.chksum2 = chksum2;
        }

        let pgno = be_u32(&hdr[0..]);
        let commit = be_u32(&hdr[4..]);

        self.frame_n += 1;

        Ok((pgno, commit))
    }

    /// Reads all committed frames to end-of-file and returns a map of page
    /// number → byte offset of the latest committed version of that page, the
    /// max offset of the WAL segment read, and the final database size in pages.
    ///
    /// Pages above the final commit size are dropped (handles a DB that shrank,
    /// e.g. via `VACUUM`, between transactions).
    ///
    /// Ported from `PageMap` in litestream@v0.5.11 wal_reader.go:189-244.
    pub fn page_map(&mut self) -> WalResult<(HashMap<u32, i64>, i64, u32)> {
        let mut m: HashMap<u32, i64> = HashMap::new();
        let mut tx_map: HashMap<u32, i64> = HashMap::new();
        let mut commit: u32 = 0;
        let mut data = vec![0u8; self.page_size as usize];

        loop {
            let (pgno, fcommit) = match self.read_frame(&mut data) {
                Ok(v) => v,
                Err(e) if e.is_eof() => break,
                Err(e) => return Err(e),
            };

            // Update latest offset for this page within the current transaction.
            // Not promoted to the full map until the txn commits.
            let offset = self.offset();
            tx_map.insert(pgno, offset);

            // On a commit record, transfer the txn offsets into the full map and
            // record the new DB size.
            if fcommit != 0 {
                for (p, o) in tx_map.drain() {
                    m.insert(p, o);
                }
                commit = fcommit;
            }
        }

        // Remove pages that exceed the final commit size (DB shrank mid-WAL).
        m.retain(|&pgno, _| pgno <= commit);

        // No complete transactions => original (zero) offset.
        if m.is_empty() {
            return Ok((m, 0, 0));
        }

        // Highest page offset, extended to the end of that frame.
        let mut end: i64 = 0;
        for &offset in m.values() {
            if end == 0 || offset > end {
                end = offset;
            }
        }
        end += WAL_FRAME_HEADER_SIZE as i64 + self.page_size as i64;

        Ok((m, end, commit))
    }

    /// Returns the set of unique frame salt pairs in the WAL, scanning until the
    /// `until` salt pair is seen or end-of-file is reached.
    ///
    /// Unlike frame reading, this does **not** verify checksums or that frame
    /// salts match the header — it deliberately collects *every* distinct salt,
    /// including those from superseded transactions.
    ///
    /// Ported from `FrameSaltsUntil` in litestream@v0.5.11 wal_reader.go:246-270.
    pub fn frame_salts_until(
        &self,
        until: (u32, u32),
    ) -> WalResult<std::collections::HashSet<(u32, u32)>> {
        let mut m = std::collections::HashSet::new();
        let step = WAL_FRAME_HEADER_SIZE as i64 + self.page_size as i64;
        let mut offset = WAL_HEADER_SIZE as i64;
        // The loop ends either when a frame-header read runs short (the Go
        // `n != len(hdr)` => break) or when we reach the `until` salt below.
        while let Some(hdr) = self.read_at(offset, WAL_FRAME_HEADER_SIZE) {
            let salt1 = be_u32(&hdr[8..]);
            let salt2 = be_u32(&hdr[12..]);

            // Track unique salts.
            m.insert((salt1, salt2));

            // Stop once we've seen the salt we were asked to read up to.
            if salt1 == until.0 && salt2 == until.1 {
                break;
            }

            offset += step;
        }
        Ok(m)
    }
}

/// Reads a big-endian `u32` from the first four bytes of `b`.
///
/// All WAL header/frame scalar fields are big-endian regardless of the checksum
/// byte order (Go uses `binary.BigEndian.Uint32`). Panics if `b.len() < 4`,
/// which never happens for the fixed-offset accesses in this module.
#[inline]
fn be_u32(b: &[u8]) -> u32 {
    u32::from_be_bytes([b[0], b[1], b[2], b[3]])
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// Loads a WAL test vector from the **read-only** upstream Go testdata tree
    /// (`reference/litestream-go/testdata/wal-reader/<name>/wal`). These are the
    /// exact fixtures the ported Go tests (`wal_reader_test.go`) consume.
    fn read_testdata(name: &str) -> Vec<u8> {
        let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        p.push("reference/litestream-go/testdata/wal-reader");
        p.push(name);
        p.push("wal");
        std::fs::read(&p).unwrap_or_else(|e| panic!("read {}: {e}", p.display()))
    }

    /// Loads the immutable golden SQLite WAL fixture.
    fn read_golden_wal() -> Vec<u8> {
        let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        p.push("tests/fixtures/golden/sample.wal");
        std::fs::read(&p).unwrap_or_else(|e| panic!("read {}: {e}", p.display()))
    }

    // ── Port of TestWALReader/OK (wal_reader_test.go:16-75) ────────────────────
    #[test]
    fn test_wal_reader_ok() {
        let mut buf = vec![0u8; 4096];
        let b = read_testdata("ok");

        let mut r = WalReader::new(&b).expect("new reader");
        assert_eq!(r.page_size(), 4096, "PageSize");
        assert_eq!(r.offset(), 0, "Offset");

        // First frame.
        let (pgno, commit) = r.read_frame(&mut buf).expect("frame 1");
        assert_eq!(pgno, 1, "pgno");
        assert_eq!(commit, 0, "commit");
        assert_eq!(&buf[..], &b[56..4152], "page data mismatch");
        assert_eq!(r.offset(), 32, "Offset");

        // Second frame: end of transaction.
        let (pgno, commit) = r.read_frame(&mut buf).expect("frame 2");
        assert_eq!(pgno, 2, "pgno");
        assert_eq!(commit, 2, "commit");
        assert_eq!(&buf[..], &b[4176..8272], "page data mismatch");
        assert_eq!(r.offset(), 4152, "Offset");

        // Third frame.
        let (pgno, commit) = r.read_frame(&mut buf).expect("frame 3");
        assert_eq!(pgno, 2, "pgno");
        assert_eq!(commit, 2, "commit");
        assert_eq!(&buf[..], &b[8296..12392], "page data mismatch");
        assert_eq!(r.offset(), 8272, "Offset");

        // End of WAL.
        let err = r.read_frame(&mut buf).expect_err("expected EOF");
        assert!(err.is_eof(), "unexpected error: {err}");
    }

    // ── Port of TestWALReader/SaltMismatch (wal_reader_test.go:77-109) ─────────
    #[test]
    fn test_wal_reader_salt_mismatch() {
        let mut buf = vec![0u8; 4096];
        let b = read_testdata("salt-mismatch");

        let mut r = WalReader::new(&b).expect("new reader");
        assert_eq!(r.page_size(), 4096);
        assert_eq!(r.offset(), 0);

        // First frame is valid.
        let (pgno, commit) = r.read_frame(&mut buf).expect("frame 1");
        assert_eq!(pgno, 1);
        assert_eq!(commit, 0);
        assert_eq!(&buf[..], &b[56..4152], "page data mismatch");

        // Second frame: salt altered so it doesn't match header => EOF.
        let err = r.read_frame(&mut buf).expect_err("expected EOF");
        assert!(err.is_eof(), "unexpected error: {err}");
    }

    // ── Port of TestWALReader/FrameChecksumMismatch (wal_reader_test.go:111-143) ─
    #[test]
    fn test_wal_reader_frame_checksum_mismatch() {
        let mut buf = vec![0u8; 4096];
        let b = read_testdata("frame-checksum-mismatch");

        let mut r = WalReader::new(&b).expect("new reader");
        assert_eq!(r.page_size(), 4096);
        assert_eq!(r.offset(), 0);

        // First frame is valid.
        let (pgno, commit) = r.read_frame(&mut buf).expect("frame 1");
        assert_eq!(pgno, 1);
        assert_eq!(commit, 0);
        assert_eq!(&buf[..], &b[56..4152], "page data mismatch");

        // Second frame: checksum altered => EOF.
        let err = r.read_frame(&mut buf).expect_err("expected EOF");
        assert!(err.is_eof(), "unexpected error: {err}");
    }

    // ── Port of TestWALReader/ZeroLength (wal_reader_test.go:145-150) ──────────
    #[test]
    fn test_wal_reader_zero_length() {
        let err = WalReader::new(&[]).expect_err("expected EOF");
        assert!(err.is_eof(), "unexpected error: {err}");
    }

    // ── Port of TestWALReader/PartialHeader (wal_reader_test.go:152-157) ───────
    #[test]
    fn test_wal_reader_partial_header() {
        let err = WalReader::new(&[0u8; 10]).expect_err("expected EOF");
        assert!(err.is_eof(), "unexpected error: {err}");
    }

    // ── Port of TestWALReader/BadMagic (wal_reader_test.go:159-164) ────────────
    #[test]
    fn test_wal_reader_bad_magic() {
        // All-zero 32-byte header => magic 0.
        let err = WalReader::new(&[0u8; 32]).expect_err("expected error");
        assert_eq!(err.to_string(), "invalid wal header magic: 0");
    }

    // ── Port of TestWALReader/BadHeaderChecksum (wal_reader_test.go:166-176) ───
    #[test]
    fn test_wal_reader_bad_header_checksum() {
        // Valid big-endian magic, zero checksum fields => header checksum fails.
        let data: [u8; 32] = [
            0x37, 0x7f, 0x06, 0x83, 0x00, 0x00, 0x00, 0x00, //
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, //
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, //
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        ];
        let err = WalReader::new(&data).expect_err("expected EOF");
        assert!(err.is_eof(), "unexpected error: {err}");
    }

    // ── Port of TestWALReader/BadHeaderVersion (wal_reader_test.go:178-188) ────
    #[test]
    fn test_wal_reader_bad_header_version() {
        // Valid magic + version 1 + a header checksum that actually validates
        // over bytes [0..24] (these checksum bytes are taken verbatim from the
        // upstream Go test vector).
        let data: [u8; 32] = [
            0x37, 0x7f, 0x06, 0x83, 0x00, 0x00, 0x00, 0x01, //
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, //
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, //
            0x15, 0x7b, 0x20, 0x92, 0xbb, 0xf8, 0x34, 0x1d,
        ];
        let err = WalReader::new(&data).expect_err("expected error");
        assert_eq!(err.to_string(), "unsupported wal version: 1");
    }

    // ── Port of TestWALReader/ErrBufferSize (wal_reader_test.go:190-204) ───────
    #[test]
    fn test_wal_reader_err_buffer_size() {
        let b = read_testdata("ok");
        let mut r = WalReader::new(&b).expect("new reader");
        let mut small = vec![0u8; 512];
        let err = r
            .read_frame(&mut small)
            .expect_err("expected buffer-size error");
        assert_eq!(
            err.to_string(),
            "WALReader.ReadFrame(): buffer size (512) must match page size (4096)"
        );
    }

    // ── Port of TestWALReader/ErrPartialFrameHeader (wal_reader_test.go:206-218) ─
    #[test]
    fn test_wal_reader_err_partial_frame_header() {
        let b = read_testdata("ok");
        // Truncate to 40 bytes: header (32) + 8 bytes of frame header.
        let mut r = WalReader::new(&b[..40]).expect("new reader");
        let mut buf = vec![0u8; 4096];
        let err = r.read_frame(&mut buf).expect_err("expected EOF");
        assert!(err.is_eof(), "unexpected error: {err}");
    }

    // ── Port of TestWALReader/ErrFrameHeaderOnly (wal_reader_test.go:220-232) ──
    #[test]
    fn test_wal_reader_err_frame_header_only() {
        let b = read_testdata("ok");
        // Truncate to 56 bytes: header (32) + full frame header (24), no page.
        let mut r = WalReader::new(&b[..56]).expect("new reader");
        let mut buf = vec![0u8; 4096];
        let err = r.read_frame(&mut buf).expect_err("expected EOF");
        assert!(err.is_eof(), "unexpected error: {err}");
    }

    // ── Port of TestWALReader/ErrPartialFrameData (wal_reader_test.go:234-246) ──
    #[test]
    fn test_wal_reader_err_partial_frame_data() {
        let b = read_testdata("ok");
        // Truncate to 1000 bytes: full frame header but only partial page data.
        let mut r = WalReader::new(&b[..1000]).expect("new reader");
        let mut buf = vec![0u8; 4096];
        let err = r.read_frame(&mut buf).expect_err("expected EOF");
        assert!(err.is_eof(), "unexpected error: {err}");
    }

    // ── Port of TestWALReader_FrameSaltsUntil/OK (wal_reader_test.go:249-278) ──
    #[test]
    fn test_wal_reader_frame_salts_until_ok() {
        let b = read_testdata("frame-salts");
        let r = WalReader::new(&b).expect("new reader");

        // No frame carries salt (0,0), so the scan runs to EOF and collects all
        // three distinct salt pairs present in the file.
        let m = r
            .frame_salts_until((0x0000_0000, 0x0000_0000))
            .expect("frame salts");
        assert_eq!(m.len(), 3, "len(m)");
        assert!(m.contains(&(0x1b9a_294b, 0x37f9_1916)), "salt 0 not found");
        assert!(m.contains(&(0x1b9a_294a, 0x031f_195e)), "salt 1 not found");
        assert!(m.contains(&(0x1b9a_2949, 0x13b3_dd67)), "salt 2 not found");
    }

    // ── GOLDEN (byte-exact): real SQLite WAL from tests/fixtures/golden ────────
    //
    // sample.wal: 16,512 bytes, page size 4096, magic 0x377f0682 (little-endian
    // checksums), per tests/fixtures/golden/MANIFEST.md. A checksum failure here
    // means the port is wrong, NOT the fixture (AGENTS.md rule 3).
    #[test]
    fn test_golden_sample_wal() {
        let b = read_golden_wal();
        assert_eq!(b.len(), 16_512, "fixture size changed unexpectedly");

        let mut r = WalReader::new(&b).expect("new reader over golden WAL");

        // Byte order: magic 0x377f0682 => checksums little-endian.
        assert!(
            !r.is_big_endian(),
            "golden WAL magic 0x377f0682 selects LITTLE-endian checksums"
        );

        // Page size from the header.
        assert_eq!(r.page_size(), 4096, "golden WAL page size");

        // Salt values straight from the header bytes (offsets 16/20).
        assert_eq!(
            r.salt(),
            (0x9bf2_9a02, 0x6867_0130),
            "golden WAL header salts"
        );

        // Every frame must read cleanly and pass the cumulative SQLite checksum.
        // read_frame returns EOF the instant a checksum (or salt) check fails, so
        // a successful read of a frame == that frame's checksum verified.
        let mut buf = vec![0u8; r.page_size() as usize];
        let mut frame_count = 0u64;
        loop {
            match r.read_frame(&mut buf) {
                Ok((pgno, _commit)) => {
                    assert!(pgno >= 1, "page numbers are 1-based");
                    frame_count += 1;
                }
                Err(e) if e.is_eof() => break,
                Err(e) => panic!("unexpected non-EOF error reading golden WAL: {e}"),
            }
        }

        // A real WAL with data => a nonzero frame count, every frame checksummed.
        assert!(
            frame_count > 0,
            "expected a nonzero frame count in the golden WAL"
        );

        // Cross-check the geometry: with all frames valid to EOF, the file must
        // be exactly header + frame_count whole frames (no trailing partial).
        let frame_size = WAL_FRAME_HEADER_SIZE as i64 + r.page_size() as i64;
        assert_eq!(
            WAL_HEADER_SIZE as i64 + frame_count as i64 * frame_size,
            b.len() as i64,
            "golden WAL is not an exact header + N*frame layout"
        );
    }
}
