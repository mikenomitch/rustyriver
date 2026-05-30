//! `rustyriver` — embeddable streaming replication of a SQLite database to
//! object storage, with point-in-time restore and object-storage lease fencing.
//!
//! A from-scratch Rust reimplementation of **Litestream v0.5** (pinned
//! `v0.5.11`). See `PLAN.md` for the full specification and `AGENTS.md` for the
//! non-negotiable operating rules.
//!
//! The public surface (`Db`, `Replica`, `Leaser`, `Config`, `restore`) is
//! defined incrementally by the task DAG in `PLAN.md` §5; module bodies are
//! filled by their owning tasks and are scaffold placeholders until then.
//!
//! # Core position/identity helpers  (T4)
//!
//! Ported from litestream@v0.5.11 litestream.go:18-203 and
//! ltx@v0.5.1 ltx.go:66-145.

pub mod client;
pub mod db;
pub mod error;
pub mod leaser;
pub mod ltx;
pub mod replica;
pub mod replica_url;
pub mod store;
pub mod wal;

// Re-export the error model at the crate root.
pub use error::{new_ltx_error, Error, LTXError, Result};

// ── Naming constants ──────────────────────────────────────────────────────────

/// Suffix appended to a database path to obtain the Litestream metadata
/// directory.
///
/// Ported from litestream@v0.5.11 litestream.go:19 (`MetaDirSuffix`).
pub const META_DIR_SUFFIX: &str = "-litestream";

// ── SQLite checkpoint modes ───────────────────────────────────────────────────
//
// Ported from litestream@v0.5.11 litestream.go:22-28.

pub const CHECKPOINT_MODE_PASSIVE: &str = "PASSIVE";
pub const CHECKPOINT_MODE_FULL: &str = "FULL";
pub const CHECKPOINT_MODE_RESTART: &str = "RESTART";
pub const CHECKPOINT_MODE_TRUNCATE: &str = "TRUNCATE";

// ── SQLite WAL size constants ─────────────────────────────────────────────────
//
// Ported from litestream@v0.5.11 litestream.go:95-128.

/// Size of a SQLite WAL file header, in bytes.
pub const WAL_HEADER_SIZE: usize = 32;

/// Size of a SQLite WAL frame header, in bytes.
pub const WAL_FRAME_HEADER_SIZE: usize = 24;

/// Byte offset of the checksum pair inside a WAL header.
pub const WAL_HEADER_CHECKSUM_OFFSET: usize = 24;

/// Byte offset of the checksum pair inside a WAL frame header.
pub const WAL_FRAME_HEADER_CHECKSUM_OFFSET: usize = 16;

// ── TXID ──────────────────────────────────────────────────────────────────────

/// A monotonically increasing transaction identifier.
///
/// Represented as a `u64` and formatted as a zero-padded 16-character
/// lowercase hex string (e.g. `"0000000000000001"`).  This matches the
/// on-disk LTX filename convention and `ltx.TXID.String()` in
/// ltx@v0.5.1 ltx.go:142-144.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct TXID(pub u64);

impl TXID {
    /// The zero TXID.
    pub const ZERO: TXID = TXID(0);

    /// Returns the raw `u64` value.
    #[inline]
    pub fn get(self) -> u64 {
        self.0
    }
}

impl std::fmt::Display for TXID {
    /// Formats as a 16-digit lowercase hex string, zero-padded.
    ///
    /// Ported from `TXID.String()` in ltx@v0.5.1 ltx.go:142.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:016x}", self.0)
    }
}

/// Parses a TXID from its canonical 16-character lowercase hex representation.
///
/// Returns an error if the string is not exactly 16 hex characters or contains
/// any character outside `[0-9a-fA-F]`.
///
/// Ported from `ParseTXID` in ltx@v0.5.1 ltx.go:130-138.
///
/// Note: Go's `strconv.ParseUint(s, 16, 64)` rejects sign prefixes (`+`, `-`).
/// Rust's `u64::from_str_radix` accepts a leading `+`, which would be a
/// behavioural divergence.  We pre-validate that every byte is a valid hex
/// digit to match Go's stricter contract.
pub fn parse_txid(s: &str) -> Result<TXID> {
    if s.len() != 16 {
        return Err(Error::Other(
            format!("invalid formatted transaction id length: {:?}", s).into(),
        ));
    }
    // Reject any character outside [0-9a-fA-F].  This matches Go's
    // strconv.ParseUint which rejects sign prefixes ('+'/'-') and any
    // non-hex byte, whereas Rust's from_str_radix would accept a leading '+'.
    if !s.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(Error::Other(
            format!("invalid transaction id format: {:?}", s).into(),
        ));
    }
    let v = u64::from_str_radix(s, 16)
        .map_err(|_| Error::Other(format!("invalid transaction id format: {:?}", s).into()))?;
    Ok(TXID(v))
}

impl From<u64> for TXID {
    fn from(v: u64) -> Self {
        TXID(v)
    }
}

impl From<TXID> for u64 {
    fn from(t: TXID) -> Self {
        t.0
    }
}

// ── Checksum ──────────────────────────────────────────────────────────────────

/// A 64-bit LTX rolling / file checksum.
///
/// The high bit is the `ChecksumFlag` sentinel used by the LTX format to
/// distinguish a real checksum from an absent one.
///
/// Ported from ltx@v0.5.1 ltx.go (type alias `Checksum uint64`).
pub type Checksum = u64;

/// Flag bit set on every valid non-zero LTX checksum.
///
/// Ported from `ChecksumFlag` in ltx@v0.5.1 ltx.go:55.
pub const CHECKSUM_FLAG: Checksum = 1 << 63;

// ── Pos ──────────────────────────────────────────────────────────────────────

/// Replication position — the TXID plus a rolling post-apply checksum.
///
/// Together these two values uniquely identify the state of a database at a
/// point in the replication log.
///
/// Ported from `Pos` / `ParsePos` / `Pos.String` in ltx@v0.5.1 ltx.go:66-109.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Pos {
    /// Transaction ID at this position.
    pub txid: TXID,
    /// Rolling CRC checksum of the database state after applying the
    /// transaction at `txid`.
    pub post_apply_checksum: Checksum,
}

impl Pos {
    /// The zero position (no transactions applied yet).
    pub const ZERO: Pos = Pos {
        txid: TXID::ZERO,
        post_apply_checksum: 0,
    };

    /// Constructs a new `Pos`.
    ///
    /// Ported from `NewPos` in ltx@v0.5.1 ltx.go:72.
    pub fn new(txid: TXID, post_apply_checksum: Checksum) -> Self {
        Pos {
            txid,
            post_apply_checksum,
        }
    }

    /// Returns `true` when this is the zero position.
    ///
    /// Ported from `Pos.IsZero` in ltx@v0.5.1 ltx.go:107.
    pub fn is_zero(self) -> bool {
        self == Pos::ZERO
    }
}

impl std::fmt::Display for Pos {
    /// Formats as `"<txid>/<checksum>"` — e.g. `"0000000000000001/8000000000000001"`.
    ///
    /// Ported from `Pos.String` in ltx@v0.5.1 ltx.go:102.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{:016x}", self.txid, self.post_apply_checksum)
    }
}

/// Parses a `Pos` from its canonical `"<txid>/<checksum>"` string form.
///
/// The string must be exactly 33 characters: 16 hex digits, a slash, and
/// 16 hex digits.
///
/// Ported from `ParsePos` in ltx@v0.5.1 ltx.go:80-99.
pub fn parse_pos(s: &str) -> Result<Pos> {
    if s.len() != 33 {
        return Err(Error::Other(
            format!("invalid formatted position length: {:?}", s).into(),
        ));
    }
    let txid = parse_txid(&s[..16])
        .map_err(|_| Error::Other(format!("invalid position txid: {:?}", &s[..16]).into()))?;
    // DECISION: We validate that byte 16 is '/' (the separator) even though
    // Go's ParsePos (ltx@v0.5.1 ltx.go:80-99) blindly slices s[:16] and s[17:]
    // without checking the separator byte, so Go would accept e.g.
    // "0000000000000001X8000000000000001".  We keep the stricter check because:
    // (a) the '/' is mandated by the format doc and Pos.String() always emits it,
    // (b) silently accepting a malformed separator would hide caller bugs, and
    // (c) no production caller or test exercises that path.
    // This is the conservative/least-data-loss interpretation per AGENTS.md §9.
    if s.as_bytes()[16] != b'/' {
        return Err(Error::Other(
            format!("invalid formatted position (missing /): {:?}", s).into(),
        ));
    }
    // Same sign-prefix pre-validation as parse_txid: reject non-hex characters
    // so we match Go's strconv.ParseUint contract for the checksum field too.
    let checksum_str = &s[17..];
    if !checksum_str.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(Error::Other(
            format!("invalid position checksum: {:?}", checksum_str).into(),
        ));
    }
    let checksum = u64::from_str_radix(checksum_str, 16).map_err(|_| {
        Error::Other(format!("invalid position checksum: {:?}", checksum_str).into())
    })?;
    Ok(Pos {
        txid,
        post_apply_checksum: checksum,
    })
}

// ── SQLite checksum ───────────────────────────────────────────────────────────

/// Computes a running SQLite WAL checksum over a byte slice.
///
/// The slice length **must** be a multiple of 8 bytes (the function panics
/// otherwise, matching the `assert` in the Go source).
///
/// `s0` / `s1` are the running checksum values (pass `(0, 0)` for the first
/// call, then thread the result through subsequent calls for incremental
/// checksumming).
///
/// `big_endian` selects the byte order used to read 32-bit words from `b`:
/// `true` = big-endian (WAL headers with magic `0x377f0683`),
/// `false` = little-endian (WAL headers with magic `0x377f0682`).
///
/// Ported from `Checksum` in litestream@v0.5.11 litestream.go:110-119.
///
/// # Panics
/// Panics if `b.len() % 8 != 0`.
pub fn wal_checksum(big_endian: bool, s0: u32, s1: u32, b: &[u8]) -> (u32, u32) {
    assert!(b.len().is_multiple_of(8), "misaligned checksum byte slice");

    let mut s0 = s0;
    let mut s1 = s1;

    // Iterate over 8-byte units and compute checksum.
    // Matches litestream.go Checksum loop exactly.
    let mut i = 0usize;
    while i < b.len() {
        let w0 = if big_endian {
            u32::from_be_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]])
        } else {
            u32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]])
        };
        let w1 = if big_endian {
            u32::from_be_bytes([b[i + 4], b[i + 5], b[i + 6], b[i + 7]])
        } else {
            u32::from_le_bytes([b[i + 4], b[i + 5], b[i + 6], b[i + 7]])
        };
        s0 = s0.wrapping_add(w0).wrapping_add(s1);
        s1 = s1.wrapping_add(w1).wrapping_add(s0);
        i += 8;
    }
    (s0, s1)
}

// ── LTX path helpers ──────────────────────────────────────────────────────────
//
// Ported from litestream@v0.5.11 litestream.go:184-197.

/// Returns the path to the LTX directory under a given root.
///
/// Ported from `LTXDir` in litestream@v0.5.11 litestream.go:185.
pub fn ltx_dir(root: &str) -> String {
    format!("{}/ltx", root)
}

/// Returns the path to the LTX level sub-directory.
///
/// Ported from `LTXLevelDir` in litestream@v0.5.11 litestream.go:190.
pub fn ltx_level_dir(root: &str, level: u32) -> String {
    format!("{}/ltx/{}", root, level)
}

/// Returns the path to a single LTX file for a given transaction range.
///
/// The filename follows the convention `<minTXID>-<maxTXID>.ltx` with both
/// TXIDs formatted as 16-digit lowercase hex.
///
/// Ported from `LTXFilePath` in litestream@v0.5.11 litestream.go:195 and
/// `FormatFilename` in ltx@v0.5.1 ltx.go:487.
pub fn ltx_file_path(root: &str, level: u32, min_txid: TXID, max_txid: TXID) -> String {
    format!(
        "{}/{}-{}.ltx",
        ltx_level_dir(root, level),
        min_txid,
        max_txid
    )
}
