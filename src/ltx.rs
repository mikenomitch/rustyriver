//! ltx.rs — LTX (Lite Transaction) file reader/writer + CRC64-ISO checksums.
//!
//! Ported from ltx@v0.5.1 `ltx.go`, `checksum.go`, `encoder.go`, `decoder.go`
//! and litestream@v0.5.11 `v3.go`. The authoritative byte layout is written up
//! in `reference/ltx-format.md` (produced by this task).
//!
//! The reader decodes a complete LTX file — header, LZ4-framed page block,
//! varint page index, trailer — and verifies the CRC64-ISO file checksum and
//! (for snapshots) the rolling post-apply checksum. The writer round-trips the
//! same layout; byte-fidelity against the real binary is proven later by the
//! differential test D1 (PLAN.md §6.3), not here.

use crate::error::{Error, Result};
use crate::{Checksum, Pos, CHECKSUM_FLAG, TXID};
use std::io::{Read, Write};
use std::time::SystemTime;

// ── Constants (ltx@v0.5.1 ltx.go:18-55) ──────────────────────────────────────

/// First 4 bytes of every LTX file.
pub const MAGIC: &[u8; 4] = b"LTX1";
/// Current LTX file format version.
pub const VERSION: i32 = 3;
pub const HEADER_SIZE: usize = 100;
pub const PAGE_HEADER_SIZE: usize = 6;
pub const TRAILER_SIZE: usize = 16;
pub const CHECKSUM_SIZE: usize = 8;

/// Header flag: checksums are not tracked for this file.
pub const HEADER_FLAG_NO_CHECKSUM: u32 = 1 << 1;
pub const HEADER_FLAG_MASK: u32 = HEADER_FLAG_NO_CHECKSUM;

/// SQLite PENDING_BYTE offset; the lock page derives from it.
pub const PENDING_BYTE: i64 = 0x4000_0000;

fn corrupt(msg: impl Into<String>) -> Error {
    // Wrap a format error as LTXCorrupted, matching litestream's classification
    // of malformed LTX content (litestream.go ErrLTXCorrupted).
    let _ = msg;
    Error::LTXCorrupted
}

/// Returns the lock page number for a given page size (ltx.go:494).
pub fn lock_pgno(page_size: u32) -> u32 {
    (PENDING_BYTE / page_size as i64) as u32 + 1
}

// ── CRC64-ISO (checksum.go:177 `crc64.MakeTable(crc64.ISO)`) ──────────────────

/// CRC-64/ISO polynomial (reflected), identical to Go's `crc64.ISO`.
const CRC64_ISO_POLY: u64 = 0xD800_0000_0000_0000;

const fn crc64_iso_table() -> [u64; 256] {
    let mut table = [0u64; 256];
    let mut i = 0usize;
    while i < 256 {
        let mut crc = i as u64;
        let mut j = 0;
        while j < 8 {
            if crc & 1 == 1 {
                crc = (crc >> 1) ^ CRC64_ISO_POLY;
            } else {
                crc >>= 1;
            }
            j += 1;
        }
        table[i] = crc;
        i += 1;
    }
    table
}

static CRC64_TABLE: [u64; 256] = crc64_iso_table();

/// Streaming CRC64-ISO hasher matching Go's `hash/crc64` digest semantics
/// (init 0; each update performs the standard reflected invert-process-invert).
#[derive(Clone, Default)]
pub struct Crc64 {
    crc: u64,
}

impl Crc64 {
    pub fn new() -> Self {
        Crc64 { crc: 0 }
    }

    pub fn update(&mut self, data: &[u8]) {
        let mut crc = !self.crc;
        for &b in data {
            crc = CRC64_TABLE[((crc as u8) ^ b) as usize] ^ (crc >> 8);
        }
        self.crc = !crc;
    }

    pub fn sum64(&self) -> u64 {
        self.crc
    }
}

/// CRC64 checksum of a single page combined with its page number, with the
/// ChecksumFlag set (checksum.go:106-116). Input is `BE_u32(pgno) ++ data`.
pub fn checksum_page(pgno: u32, data: &[u8]) -> Checksum {
    let mut h = Crc64::new();
    h.update(&pgno.to_be_bytes());
    h.update(data);
    CHECKSUM_FLAG | h.sum64()
}

// ── Header / PageHeader / Trailer ─────────────────────────────────────────────

/// LTX file header (100 bytes). Ported from ltx@v0.5.1 ltx.go:179-326.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Header {
    pub version: i32,
    pub flags: u32,
    pub page_size: u32,
    pub commit: u32,
    pub min_txid: TXID,
    pub max_txid: TXID,
    pub timestamp: i64,
    pub pre_apply_checksum: Checksum,
    pub wal_offset: i64,
    pub wal_size: i64,
    pub wal_salt1: u32,
    pub wal_salt2: u32,
    pub node_id: u64,
}

impl Header {
    /// True if this header begins a complete database snapshot (MinTXID == 1).
    pub fn is_snapshot(&self) -> bool {
        self.min_txid == TXID(1)
    }

    /// True if checksum tracking is disabled for this file.
    pub fn no_checksum(&self) -> bool {
        self.flags & HEADER_FLAG_NO_CHECKSUM != 0
    }

    /// Decodes a header from a 100-byte slice (ltx.go:302-326). All big-endian.
    pub fn parse(b: &[u8]) -> Result<Header> {
        if b.len() < HEADER_SIZE {
            return Err(corrupt("short header"));
        }
        if &b[0..4] != MAGIC {
            return Err(corrupt("bad magic"));
        }
        Ok(Header {
            version: VERSION,
            flags: u32_be(&b[4..]),
            page_size: u32_be(&b[8..]),
            commit: u32_be(&b[12..]),
            min_txid: TXID(u64_be(&b[16..])),
            max_txid: TXID(u64_be(&b[24..])),
            timestamp: u64_be(&b[32..]) as i64,
            pre_apply_checksum: u64_be(&b[40..]),
            wal_offset: u64_be(&b[48..]) as i64,
            wal_size: u64_be(&b[56..]) as i64,
            wal_salt1: u32_be(&b[64..]),
            wal_salt2: u32_be(&b[68..]),
            node_id: u64_be(&b[72..]),
        })
    }

    /// Encodes the header to 100 bytes (ltx.go:283-299).
    pub fn marshal(&self) -> [u8; HEADER_SIZE] {
        let mut b = [0u8; HEADER_SIZE];
        b[0..4].copy_from_slice(MAGIC);
        b[4..8].copy_from_slice(&self.flags.to_be_bytes());
        b[8..12].copy_from_slice(&self.page_size.to_be_bytes());
        b[12..16].copy_from_slice(&self.commit.to_be_bytes());
        b[16..24].copy_from_slice(&self.min_txid.0.to_be_bytes());
        b[24..32].copy_from_slice(&self.max_txid.0.to_be_bytes());
        b[32..40].copy_from_slice(&(self.timestamp as u64).to_be_bytes());
        b[40..48].copy_from_slice(&self.pre_apply_checksum.to_be_bytes());
        b[48..56].copy_from_slice(&(self.wal_offset as u64).to_be_bytes());
        b[56..64].copy_from_slice(&(self.wal_size as u64).to_be_bytes());
        b[64..68].copy_from_slice(&self.wal_salt1.to_be_bytes());
        b[68..72].copy_from_slice(&self.wal_salt2.to_be_bytes());
        b[72..80].copy_from_slice(&self.node_id.to_be_bytes());
        b
    }

    /// Validates header invariants (ltx.go:208-267).
    pub fn validate(&self) -> Result<()> {
        if self.version != VERSION {
            return Err(corrupt("invalid version"));
        }
        if self.flags != (self.flags & HEADER_FLAG_MASK) {
            return Err(corrupt("invalid flags"));
        }
        if !is_valid_page_size(self.page_size) {
            return Err(corrupt("invalid page size"));
        }
        if self.min_txid == TXID(0) {
            return Err(corrupt("minimum transaction id required"));
        }
        if self.max_txid == TXID(0) {
            return Err(corrupt("maximum transaction id required"));
        }
        if self.min_txid > self.max_txid {
            return Err(corrupt("transaction ids out of order"));
        }
        if self.wal_offset < 0 {
            return Err(corrupt("wal offset cannot be negative"));
        }
        if self.wal_size < 0 {
            return Err(corrupt("wal size cannot be negative"));
        }
        if (self.wal_salt1 != 0 || self.wal_salt2 != 0) && self.wal_offset == 0 {
            return Err(corrupt("wal offset required if salt exists"));
        }
        if self.wal_offset == 0 && self.wal_size != 0 {
            return Err(corrupt("wal offset required if wal size exists"));
        }
        if self.is_snapshot() {
            if self.pre_apply_checksum != 0 {
                return Err(corrupt("pre-apply checksum must be zero on snapshots"));
            }
        } else if self.no_checksum() {
            if self.pre_apply_checksum != 0 {
                return Err(corrupt("pre-apply checksum not allowed"));
            }
        } else {
            if self.pre_apply_checksum == 0 {
                return Err(corrupt("pre-apply checksum required on non-snapshot files"));
            }
            if self.pre_apply_checksum & CHECKSUM_FLAG == 0 {
                return Err(corrupt("invalid pre-apply checksum format"));
            }
        }
        Ok(())
    }
}

/// Per-page header (6 bytes). Ported from ltx.go:408-447.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PageHeader {
    pub pgno: u32,
    pub flags: u16,
}

impl PageHeader {
    pub fn is_zero(&self) -> bool {
        self.pgno == 0 && self.flags == 0
    }

    pub fn parse(b: &[u8]) -> Result<PageHeader> {
        if b.len() < PAGE_HEADER_SIZE {
            return Err(corrupt("short page header"));
        }
        Ok(PageHeader {
            pgno: u32_be(&b[0..]),
            flags: u16_be(&b[4..]),
        })
    }

    pub fn marshal(&self) -> [u8; PAGE_HEADER_SIZE] {
        let mut b = [0u8; PAGE_HEADER_SIZE];
        b[0..4].copy_from_slice(&self.pgno.to_be_bytes());
        b[4..6].copy_from_slice(&self.flags.to_be_bytes());
        b
    }

    pub fn validate(&self) -> Result<()> {
        if self.pgno == 0 {
            return Err(corrupt("page number required"));
        }
        if self.flags != 0 {
            return Err(corrupt("no page flags allowed"));
        }
        Ok(())
    }
}

/// File trailer (16 bytes). Ported from ltx.go:348-393.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Trailer {
    pub post_apply_checksum: Checksum,
    pub file_checksum: Checksum,
}

impl Trailer {
    pub fn parse(b: &[u8]) -> Result<Trailer> {
        if b.len() < TRAILER_SIZE {
            return Err(corrupt("short trailer"));
        }
        Ok(Trailer {
            post_apply_checksum: u64_be(&b[0..]),
            file_checksum: u64_be(&b[8..]),
        })
    }

    pub fn marshal(&self) -> [u8; TRAILER_SIZE] {
        let mut b = [0u8; TRAILER_SIZE];
        b[0..8].copy_from_slice(&self.post_apply_checksum.to_be_bytes());
        b[8..16].copy_from_slice(&self.file_checksum.to_be_bytes());
        b
    }
}

/// True if `sz` is a power of two in [512, 65536] (ltx.go:399-406).
pub fn is_valid_page_size(sz: u32) -> bool {
    let mut i = 512u32;
    while i <= 65536 {
        if sz == i {
            return true;
        }
        i *= 2;
    }
    false
}

/// Formats an LTX filename for a transaction range (ltx.go:487-489).
pub fn format_filename(min_txid: TXID, max_txid: TXID) -> String {
    format!("{}-{}.ltx", min_txid, max_txid)
}

/// Parses a `<min>-<max>.ltx` filename (ltx.go:450-459).
pub fn parse_filename(name: &str) -> Result<(TXID, TXID)> {
    let stem = name
        .strip_suffix(".ltx")
        .ok_or_else(|| corrupt("invalid ltx filename"))?;
    let (a, b) = stem
        .split_once('-')
        .ok_or_else(|| corrupt("invalid ltx filename"))?;
    if a.len() != 16 || b.len() != 16 {
        return Err(corrupt("invalid ltx filename"));
    }
    let min = u64::from_str_radix(a, 16).map_err(|_| corrupt("invalid ltx filename"))?;
    let max = u64::from_str_radix(b, 16).map_err(|_| corrupt("invalid ltx filename"))?;
    Ok((TXID(min), TXID(max)))
}

// ── FileInfo ──────────────────────────────────────────────────────────────────

/// Metadata about an LTX file on a replica. Ported from ltx@v0.5.1 ltx.go:571-596.
///
/// `pre_apply_checksum`/`post_apply_checksum` are populated when known (e.g. by
/// decoding) and are zero when a file is discovered by a bare directory/bucket
/// listing. `created_at` is the file's timestamp (mtime / LastModified, or the
/// LTX header timestamp when `use_metadata` is requested).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FileInfo {
    pub level: i32,
    pub min_txid: TXID,
    pub max_txid: TXID,
    pub pre_apply_checksum: Checksum,
    pub post_apply_checksum: Checksum,
    pub size: i64,
    pub created_at: Option<SystemTime>,
}

impl FileInfo {
    /// Replication position *after* this file is applied (ltx.go:591-596).
    pub fn pos(&self) -> Pos {
        Pos::new(self.max_txid, self.post_apply_checksum)
    }

    /// Replication position *before* this file is applied (ltx.go:583-588).
    pub fn pre_apply_pos(&self) -> Pos {
        Pos::new(
            TXID(self.min_txid.0.saturating_sub(1)),
            self.pre_apply_checksum,
        )
    }
}

// ── Decoder ──────────────────────────────────────────────────────────────────

/// The verified result of decoding a complete LTX file.
#[derive(Debug, Clone)]
pub struct DecodedFile {
    pub header: Header,
    pub trailer: Trailer,
    /// Number of real pages (excludes the empty terminating page header).
    pub page_count: usize,
    /// Page numbers in file (write) order.
    pub pgnos: Vec<u32>,
}

/// Decodes and fully verifies an in-memory LTX file: header, LZ4-framed pages,
/// page index, trailer, the CRC64-ISO file checksum, and — for snapshots — the
/// rolling post-apply checksum. Returns `Error::ChecksumMismatch` /
/// `Error::LTXCorrupted` on any inconsistency.
///
/// Ported from the read+verify path in decoder.go:68-219.
pub fn decode_file(bytes: &[u8]) -> Result<DecodedFile> {
    let len = bytes.len();
    if len < HEADER_SIZE + PAGE_HEADER_SIZE + 8 + TRAILER_SIZE {
        return Err(corrupt("file too short"));
    }

    let header = Header::parse(&bytes[0..HEADER_SIZE])?;
    header.validate()?;
    let page_size = header.page_size as usize;

    let trailer = Trailer::parse(&bytes[len - TRAILER_SIZE..len])?;

    // Page index: the u64 size field sits just before the trailer; the index
    // elements precede it. (encoder.go:137-173 / decoder.go:309-346.)
    let size_field_off = len - TRAILER_SIZE - 8;
    let index_size = u64_be(&bytes[size_field_off..]) as usize;
    let idx_start = size_field_off
        .checked_sub(index_size)
        .ok_or_else(|| corrupt("bad page index size"))?;
    if idx_start < HEADER_SIZE + PAGE_HEADER_SIZE {
        return Err(corrupt("bad page index offset"));
    }
    let index = parse_page_index(&bytes[idx_start..size_field_off])?;

    // The empty page header terminates the page block, immediately before the
    // page index.
    let empty_header_off = idx_start - PAGE_HEADER_SIZE;

    let mut crc = Crc64::new();
    crc.update(&bytes[0..HEADER_SIZE]); // header

    // Rolling post-apply checksum (only meaningful for tracked snapshots).
    let track = header.is_snapshot() && !header.no_checksum();
    let mut rolling: Checksum = CHECKSUM_FLAG;
    let lock = lock_pgno(header.page_size);

    // Walk the page block sequentially in write order.
    let mut off = HEADER_SIZE;
    let mut pgnos = Vec::new();
    let mut scratch = vec![0u8; page_size];
    while off < empty_header_off {
        let ph = PageHeader::parse(&bytes[off..off + PAGE_HEADER_SIZE])?;
        if ph.is_zero() {
            return Err(corrupt("unexpected empty page header"));
        }
        ph.validate()?;
        let elem = index
            .get(&ph.pgno)
            .ok_or_else(|| corrupt("page missing from index"))?;
        if elem.offset as usize != off {
            return Err(corrupt("page index offset mismatch"));
        }
        let sz = elem.size as usize;
        if sz < PAGE_HEADER_SIZE || off + sz > empty_header_off {
            return Err(corrupt("bad page size in index"));
        }

        // page header bytes go into the file hash verbatim …
        crc.update(&bytes[off..off + PAGE_HEADER_SIZE]);
        // … and the *decompressed* data (the file checksum is over uncompressed
        // page bytes — decoder.go:177-180).
        decompress_page(&bytes[off + PAGE_HEADER_SIZE..off + sz], &mut scratch)?;
        crc.update(&scratch);

        if track && ph.pgno != lock {
            rolling = CHECKSUM_FLAG | (rolling ^ checksum_page(ph.pgno, &scratch));
        }

        pgnos.push(ph.pgno);
        off += sz;
    }
    if off != empty_header_off {
        return Err(corrupt("page block did not end on the index boundary"));
    }

    // empty page header (must be all zero) + page index + trailer[:8] all feed
    // the file hash as raw bytes (decoder.go:82-83, 128, 163).
    let empty = &bytes[empty_header_off..empty_header_off + PAGE_HEADER_SIZE];
    if !PageHeader::parse(empty)?.is_zero() {
        return Err(corrupt("missing empty page header terminator"));
    }
    crc.update(empty);
    crc.update(&bytes[idx_start..len - CHECKSUM_SIZE]);

    let file_checksum = CHECKSUM_FLAG | crc.sum64();
    if file_checksum != trailer.file_checksum {
        return Err(Error::ChecksumMismatch);
    }

    if track && rolling != trailer.post_apply_checksum {
        return Err(Error::ChecksumMismatch);
    }

    Ok(DecodedFile {
        header,
        trailer,
        page_count: pgnos.len(),
        pgnos,
    })
}

/// Decodes and verifies a complete LTX file, returning each page's
/// `(pgno, decompressed_data)` in write order.
///
/// This reuses [`decode_file`] for full verification (file checksum + page
/// index) and then re-walks the page block to materialize the page bytes —
/// needed by `DB.verify`'s `lastPageMatch` check (db.go:1457-1474), which
/// compares a WAL page against the pages stored in the last LTX file.
pub fn decode_file_pages(bytes: &[u8]) -> Result<Vec<(u32, Vec<u8>)>> {
    // Full verification first (also guarantees the index/geometry are sound).
    decode_file(bytes)?;

    let len = bytes.len();
    let header = Header::parse(&bytes[0..HEADER_SIZE])?;
    let page_size = header.page_size as usize;

    let size_field_off = len - TRAILER_SIZE - 8;
    let index_size = u64_be(&bytes[size_field_off..]) as usize;
    let idx_start = size_field_off
        .checked_sub(index_size)
        .ok_or_else(|| corrupt("bad page index size"))?;
    let index = parse_page_index(&bytes[idx_start..size_field_off])?;
    let empty_header_off = idx_start - PAGE_HEADER_SIZE;

    let mut out = Vec::new();
    let mut off = HEADER_SIZE;
    let mut scratch = vec![0u8; page_size];
    while off < empty_header_off {
        let ph = PageHeader::parse(&bytes[off..off + PAGE_HEADER_SIZE])?;
        let elem = index
            .get(&ph.pgno)
            .ok_or_else(|| corrupt("page missing from index"))?;
        let sz = elem.size as usize;
        decompress_page(&bytes[off + PAGE_HEADER_SIZE..off + sz], &mut scratch)?;
        out.push((ph.pgno, scratch.clone()));
        off += sz;
    }
    Ok(out)
}

/// Reconstructs the full SQLite database image from a **snapshot** LTX file
/// (every page `1..=commit`, with the lock page zero-filled).
///
/// Ported from `Decoder.DecodeDatabaseTo` in ltx@v0.5.1 decoder.go:223-268. Used
/// by the snapshot conformance test (the CRC64 of this image must equal the live
/// DB's CRC64). Errors if the file is not a snapshot or a page is missing.
pub fn decode_database_image(bytes: &[u8]) -> Result<Vec<u8>> {
    let header = Header::parse(&bytes[0..HEADER_SIZE])?;
    if !header.is_snapshot() {
        return Err(corrupt(
            "cannot decode non-snapshot LTX file to SQLite database",
        ));
    }
    let page_size = header.page_size as usize;
    let lock = lock_pgno(header.page_size);

    // Materialize the pages, keyed by page number.
    let pages = decode_file_pages(bytes)?;
    let mut by_pgno: std::collections::HashMap<u32, Vec<u8>> = std::collections::HashMap::new();
    for (pgno, data) in pages {
        by_pgno.insert(pgno, data);
    }

    let mut image = Vec::with_capacity(header.commit as usize * page_size);
    for pgno in 1..=header.commit {
        if pgno == lock {
            image.extend(std::iter::repeat_n(0u8, page_size));
            continue;
        }
        let data = by_pgno
            .get(&pgno)
            .ok_or_else(|| corrupt("missing page in snapshot"))?;
        image.extend_from_slice(data);
    }
    Ok(image)
}

/// Decompresses one LZ4 frame holding exactly one page into `out`.
fn decompress_page(compressed: &[u8], out: &mut [u8]) -> Result<()> {
    let mut fd = lz4_flex::frame::FrameDecoder::new(compressed);
    fd.read_exact(out)
        .map_err(|_| corrupt("lz4 decompress failed"))?;
    Ok(())
}

#[derive(Debug, Clone, Copy)]
struct PageIndexElem {
    offset: u64,
    size: u64,
}

/// Parses the varint page index (decoder.go:309-346): tuples of
/// `(pgno, offset, size)` terminated by a `pgno == 0` marker.
fn parse_page_index(buf: &[u8]) -> Result<std::collections::HashMap<u32, PageIndexElem>> {
    let mut map = std::collections::HashMap::new();
    let mut pos = 0usize;
    loop {
        let pgno = read_uvarint(buf, &mut pos)?;
        if pgno == 0 {
            break;
        }
        let offset = read_uvarint(buf, &mut pos)?;
        let size = read_uvarint(buf, &mut pos)?;
        map.insert(pgno as u32, PageIndexElem { offset, size });
    }
    Ok(map)
}

// ── Encoder (round-trip; byte-fidelity vs the real binary is D1's job) ────────

/// Encodes a complete LTX file from a header, an ordered set of pages, and the
/// post-apply checksum. Mirrors encoder.go's layout; LZ4 framing uses lz4_flex.
pub fn encode_file(
    header: &Header,
    pages: &[(u32, Vec<u8>)],
    post_apply_checksum: Checksum,
) -> Result<Vec<u8>> {
    header.validate()?;
    let page_size = header.page_size as usize;

    let mut out: Vec<u8> = Vec::new();
    let mut crc = Crc64::new();

    // Header.
    let hb = header.marshal();
    out.extend_from_slice(&hb);
    crc.update(&hb);

    // Pages.
    let mut index: Vec<(u32, u64, u64)> = Vec::with_capacity(pages.len());
    for (pgno, data) in pages {
        if data.len() != page_size {
            return Err(corrupt("page buffer size mismatch"));
        }
        let offset = out.len() as u64;
        let ph = PageHeader {
            pgno: *pgno,
            flags: 0,
        };
        ph.validate()?;
        let phb = ph.marshal();
        out.extend_from_slice(&phb);
        crc.update(&phb);

        let compressed = compress_page(data)?;
        out.extend_from_slice(&compressed);
        crc.update(data); // hash feeds the *uncompressed* data

        index.push((*pgno, offset, (PAGE_HEADER_SIZE + compressed.len()) as u64));
    }

    // Empty page header terminator.
    let empty = [0u8; PAGE_HEADER_SIZE];
    out.extend_from_slice(&empty);
    crc.update(&empty);

    // Page index (sorted ascending by pgno).
    index.sort_by_key(|e| e.0);
    let idx_region_start = out.len();
    let mut idx_bytes: Vec<u8> = Vec::new();
    for (pgno, offset, size) in &index {
        write_uvarint(&mut idx_bytes, *pgno as u64);
        write_uvarint(&mut idx_bytes, *offset);
        write_uvarint(&mut idx_bytes, *size);
    }
    write_uvarint(&mut idx_bytes, 0); // end marker
    out.extend_from_slice(&idx_bytes);
    crc.update(&idx_bytes);

    let index_size = (out.len() - idx_region_start) as u64;
    let size_field = index_size.to_be_bytes();
    out.extend_from_slice(&size_field);
    crc.update(&size_field);

    // Trailer: post-apply feeds the hash; file checksum is the hash result.
    crc.update(&post_apply_checksum.to_be_bytes());
    let file_checksum = CHECKSUM_FLAG | crc.sum64();
    let trailer = Trailer {
        post_apply_checksum,
        file_checksum,
    };
    out.extend_from_slice(&trailer.marshal());

    Ok(out)
}

fn compress_page(data: &[u8]) -> Result<Vec<u8>> {
    let mut fe = lz4_flex::frame::FrameEncoder::new(Vec::new());
    fe.write_all(data)
        .map_err(|_| corrupt("lz4 compress failed"))?;
    fe.finish().map_err(|_| corrupt("lz4 finish failed"))
}

// ── small byte / varint helpers ──────────────────────────────────────────────

fn u16_be(b: &[u8]) -> u16 {
    u16::from_be_bytes([b[0], b[1]])
}
fn u32_be(b: &[u8]) -> u32 {
    u32::from_be_bytes([b[0], b[1], b[2], b[3]])
}
fn u64_be(b: &[u8]) -> u64 {
    u64::from_be_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]])
}

/// LEB128 unsigned varint reader, matching Go `binary.Uvarint`.
fn read_uvarint(buf: &[u8], pos: &mut usize) -> Result<u64> {
    let mut x: u64 = 0;
    let mut s: u32 = 0;
    loop {
        let b = *buf.get(*pos).ok_or_else(|| corrupt("uvarint eof"))?;
        *pos += 1;
        if b < 0x80 {
            if s >= 64 || (s == 63 && b > 1) {
                return Err(corrupt("uvarint overflow"));
            }
            return Ok(x | ((b as u64) << s));
        }
        x |= ((b & 0x7f) as u64) << s;
        s += 7;
        if s >= 70 {
            return Err(corrupt("uvarint too long"));
        }
    }
}

fn write_uvarint(out: &mut Vec<u8>, mut x: u64) {
    while x >= 0x80 {
        out.push((x as u8) | 0x80);
        x >>= 7;
    }
    out.push(x as u8);
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    const GOLDEN_DIR: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/golden/replica/ltx/0"
    );

    // Byte-exact golden vectors: decode every real-litestream L0 file and verify
    // its CRC64-ISO checksums. A failure means rustyriver is wrong, never the
    // fixture (AGENTS.md rule 3).
    #[test]
    fn golden_ltx_files_decode_and_verify() {
        for i in 1u64..=6 {
            let name = format_filename(TXID(i), TXID(i));
            let path = format!("{GOLDEN_DIR}/{name}");
            let bytes = std::fs::read(&path).unwrap_or_else(|e| panic!("read {path}: {e}"));

            let f = decode_file(&bytes).unwrap_or_else(|e| panic!("decode {name} failed: {e}"));

            assert_eq!(f.header.version, VERSION, "{name}: version");
            assert_eq!(f.header.page_size, 4096, "{name}: page size");
            assert_eq!(f.header.min_txid, TXID(i), "{name}: min txid");
            assert_eq!(f.header.max_txid, TXID(i), "{name}: max txid");
            assert!(f.page_count >= 1, "{name}: has pages");
            // ChecksumFlag must be set on the stored file checksum.
            assert_ne!(
                f.trailer.file_checksum & CHECKSUM_FLAG,
                0,
                "{name}: file checksum flag"
            );
            // A successful decode means the CRC64-ISO *file checksum* matched
            // byte-exact against what real litestream wrote — the strong proof.
            // Real litestream L0 WAL-segment files set HeaderFlagNoChecksum (the
            // rolling DB checksum is tracked at the DB layer, not here), so the
            // trailer's post-apply checksum is untracked. decode_file verifies
            // the post-apply checksum only when it IS tracked.
            if i == 1 {
                assert!(f.header.is_snapshot(), "txid 1 is a snapshot");
            }
            assert!(
                f.header.no_checksum(),
                "{name}: real L0 files set HeaderFlagNoChecksum"
            );
        }
    }

    // Corrupting any byte must be caught by the file checksum (not silently ok).
    #[test]
    fn golden_corruption_is_detected() {
        let name = format_filename(TXID(1), TXID(1));
        let path = format!("{GOLDEN_DIR}/{name}");
        let mut bytes = std::fs::read(&path).unwrap();
        // Flip a bit in the page block (after the header).
        let mid = HEADER_SIZE + (bytes.len() - HEADER_SIZE) / 2;
        bytes[mid] ^= 0x01;
        match decode_file(&bytes) {
            Err(Error::ChecksumMismatch) | Err(Error::LTXCorrupted) => {}
            other => panic!("expected corruption to be detected, got {other:?}"),
        }
    }

    #[test]
    fn header_marshal_roundtrip() {
        let h = Header {
            version: VERSION,
            flags: 0,
            page_size: 4096,
            commit: 3,
            min_txid: TXID(1),
            max_txid: TXID(1),
            timestamp: 1_700_000_000_000,
            pre_apply_checksum: 0,
            wal_offset: 0,
            wal_size: 0,
            wal_salt1: 0,
            wal_salt2: 0,
            node_id: 0,
        };
        let b = h.marshal();
        assert_eq!(b.len(), HEADER_SIZE);
        assert_eq!(&b[0..4], MAGIC);
        assert_eq!(Header::parse(&b).unwrap(), h);
    }

    #[test]
    fn page_header_and_trailer_roundtrip() {
        let ph = PageHeader { pgno: 7, flags: 0 };
        assert_eq!(PageHeader::parse(&ph.marshal()).unwrap(), ph);
        assert!(PageHeader::default().is_zero());

        let t = Trailer {
            post_apply_checksum: CHECKSUM_FLAG | 0x1234,
            file_checksum: CHECKSUM_FLAG | 0xABCD,
        };
        assert_eq!(Trailer::parse(&t.marshal()).unwrap(), t);
    }

    #[test]
    fn filename_roundtrip() {
        let s = format_filename(TXID(1), TXID(6));
        assert_eq!(s, "0000000000000001-0000000000000006.ltx");
        assert_eq!(parse_filename(&s).unwrap(), (TXID(1), TXID(6)));
        assert!(parse_filename("nope.txt").is_err());
        assert!(parse_filename("0000000000000001.ltx").is_err());
    }

    #[test]
    fn checksum_page_sets_flag_and_combines_pgno() {
        let data = vec![0xABu8; 4096];
        let c1 = checksum_page(1, &data);
        let c2 = checksum_page(2, &data);
        assert_ne!(c1, c2, "page number must affect the checksum");
        assert_ne!(c1 & CHECKSUM_FLAG, 0, "flag must be set");
    }

    // Our writer + reader agree (round-trip). Byte-fidelity against the real
    // binary is verified later by differential D1, not here.
    #[test]
    fn encode_decode_roundtrip_snapshot() {
        let page_size = 4096usize;
        let pages: Vec<(u32, Vec<u8>)> =
            (1u32..=3).map(|p| (p, vec![p as u8; page_size])).collect();

        // Compute the post-apply checksum the way a snapshot does.
        let mut rolling = CHECKSUM_FLAG;
        for (p, d) in &pages {
            rolling = CHECKSUM_FLAG | (rolling ^ checksum_page(*p, d));
        }

        let header = Header {
            version: VERSION,
            flags: 0,
            page_size: page_size as u32,
            commit: 3,
            min_txid: TXID(1),
            max_txid: TXID(1),
            timestamp: 1_700_000_000_000,
            pre_apply_checksum: 0,
            wal_offset: 0,
            wal_size: 0,
            wal_salt1: 0,
            wal_salt2: 0,
            node_id: 0,
        };

        let encoded = encode_file(&header, &pages, rolling).expect("encode");
        let decoded = decode_file(&encoded).expect("decode");
        assert_eq!(decoded.header, header);
        assert_eq!(decoded.page_count, 3);
        assert_eq!(decoded.pgnos, vec![1, 2, 3]);
        assert_eq!(decoded.trailer.post_apply_checksum, rolling);
    }

    #[test]
    fn crc64_iso_matches_known_vector() {
        // CRC-64/GO-ISO check value of "123456789".
        let mut h = Crc64::new();
        h.update(b"123456789");
        assert_eq!(h.sum64(), 0xb909_56c7_75a4_1001);
    }
}
