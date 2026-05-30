//! Port of litestream_test.go helper tests.
//!
//! Ported from litestream@v0.5.11 litestream_test.go:18-163

use rustyriver::{
    ltx_dir, ltx_file_path, ltx_level_dir, new_ltx_error, parse_pos, parse_txid, Error, Pos, TXID,
};

// ── helpers ───────────────────────────────────────────────────────────────────

fn decode_hex(s: &str) -> Vec<u8> {
    assert!(s.len().is_multiple_of(2), "odd hex length");
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).expect("valid hex"))
        .collect()
}

// ── test vectors ──────────────────────────────────────────────────────────────
//
// Taken from litestream_test.go:20-46.
//
// OnePass total: 4128 bytes = WAL_HEADER (24) + FRAME_HEADER (8) + frame_data() (4096)
//
// WAL header and frame header are short enough to inline as hex constants.
// The 4096-byte frame is reconstructed from its sparse non-zero bytes.

const WAL_HEADER_HEX: &str = "377f0682002de218000010000000000052382eac857b1a4e";

const FRAME_HEADER_HEX: &str = "0000000200000002";

/// Reconstructs the exact 4096-byte SQLite page from litestream_test.go:42.
///
/// The page is mostly zeros; all 52 non-zero bytes are listed here.
/// We use sparse patches instead of a 8192-character hex literal to keep
/// the source readable while staying byte-identical to upstream.
fn frame_data() -> Vec<u8> {
    let mut b = vec![0u8; 4096];
    // Non-zero bytes extracted verbatim from the litestream_test.go backtick hex.
    let patches: &[(usize, u8)] = &[
        (0, 0x0d),
        (4, 0x08),
        (5, 0x0f),
        (6, 0xe0),
        (8, 0x0f),
        (9, 0xfc),
        (10, 0x0f),
        (11, 0xf8),
        (12, 0x0f),
        (13, 0xf4),
        (14, 0x0f),
        (15, 0xf0),
        (16, 0x0f),
        (17, 0xec),
        (18, 0x0f),
        (19, 0xe8),
        (20, 0x0f),
        (21, 0xe4),
        (22, 0x0f),
        (23, 0xe0),
        (4064, 0x02),
        (4065, 0x08),
        (4066, 0x02),
        (4067, 0x09),
        (4068, 0x02),
        (4069, 0x07),
        (4070, 0x02),
        (4071, 0x09),
        (4072, 0x02),
        (4073, 0x06),
        (4074, 0x02),
        (4075, 0x09),
        (4076, 0x02),
        (4077, 0x05),
        (4078, 0x02),
        (4079, 0x09),
        (4080, 0x02),
        (4081, 0x04),
        (4082, 0x02),
        (4083, 0x09),
        (4084, 0x02),
        (4085, 0x03),
        (4086, 0x02),
        (4087, 0x09),
        (4088, 0x02),
        (4089, 0x02),
        (4090, 0x02),
        (4091, 0x09),
        (4092, 0x02),
        (4093, 0x01),
        (4094, 0x02),
        (4095, 0x09),
    ];
    for &(off, val) in patches {
        b[off] = val;
    }
    b
}

// ── TestChecksum ──────────────────────────────────────────────────────────────
// Ported from litestream_test.go:18-47

#[test]
fn test_checksum_one_pass() {
    // Ported from TestChecksum/OnePass — litestream_test.go:20-30
    // Matches: hex.DecodeString("377f0682...0209") = 4128 bytes
    let mut input = decode_hex(WAL_HEADER_HEX);
    input.extend(decode_hex(FRAME_HEADER_HEX));
    input.extend(frame_data());
    assert_eq!(input.len(), 4128);
    let (s0, s1) = rustyriver::wal_checksum(false, 0, 0, &input);
    assert_eq!([s0, s1], [0xdc2f3e84u32, 0x540488d3u32]);
}

#[test]
fn test_checksum_incremental() {
    // Ported from TestChecksum/Incremental — litestream_test.go:33-46
    // Step 1: WAL header (24 bytes)
    let (s0, s1) = rustyriver::wal_checksum(false, 0, 0, &decode_hex(WAL_HEADER_HEX));
    assert_eq!([s0, s1], [0x81153b65u32, 0x87178e8fu32]);

    // Step 2: frame header (8 bytes)
    let (s0a, s1a) = rustyriver::wal_checksum(false, s0, s1, &decode_hex(FRAME_HEADER_HEX));

    // Step 3: frame data (4096 bytes)
    let fd = frame_data();
    assert_eq!(fd.len(), 4096);
    let (s0b, s1b) = rustyriver::wal_checksum(false, s0a, s1a, &fd);

    assert_eq!([s0b, s1b], [0xdc2f3e84u32, 0x540488d3u32]);
}

// ── TestLTXDir / TestLTXLevelDir ──────────────────────────────────────────────
// Ported from litestream_test.go:48-57

#[test]
fn test_ltx_dir() {
    assert_eq!(ltx_dir("foo"), "foo/ltx");
}

#[test]
fn test_ltx_level_dir() {
    assert_eq!(ltx_level_dir("foo", 0), "foo/ltx/0");
}

// ── REVIEWER (T4): path.Join normalization divergence ─────────────────────────
//
// Upstream `LTXDir`/`LTXLevelDir`/`LTXFilePath` (litestream@v0.5.11
// litestream.go:184-197) build their paths with Go's `path.Join`, which CLEANS
// the result: it collapses repeated separators, resolves "." / "..", and strips
// trailing slashes. The Rust port (lib.rs `ltx_dir` etc.) instead uses
// `format!("{}/ltx", root)`, naive string concatenation that performs no
// cleaning. The single ported happy-path test only exercises root="foo", so the
// divergence is silently uncovered.
//
// The expected values below were produced by running the real Go helpers
// (path.Join) — they are the source-of-truth oracle, NOT rustyriver output:
//
//   LTXDir("foo/")      = "foo/ltx"        (Rust currently: "foo//ltx")
//   LTXDir("foo//bar")  = "foo/bar/ltx"    (Rust currently: "foo//bar/ltx")
//   LTXDir("foo/..")    = "ltx"            (Rust currently: "foo/../ltx")
//   LTXLevelDir("foo/",0) = "foo/ltx/0"    (Rust currently: "foo//ltx/0")
//
// Why this matters: these helpers compute LTX object-store keys. On S3-style
// stores "foo//ltx/0/..." and "foo/ltx/0/..." are DIFFERENT keys, so a root
// carrying a trailing slash (a common prefix form) makes rustyriver read/write a
// different key than the real binary — breaking the differential oracle (G3,
// PLAN.md §6.3 D1/D3). A faithful port must replicate path.Join's cleaning.
#[test]
fn test_ltx_dir_normalizes_like_path_join() {
    // Trailing slash on the root: path.Join collapses the doubled separator.
    assert_eq!(
        ltx_dir("foo/"),
        "foo/ltx",
        "LTXDir must clean trailing slash like Go path.Join (litestream.go:185)"
    );
    // Doubled internal separator.
    assert_eq!(
        ltx_dir("foo//bar"),
        "foo/bar/ltx",
        "LTXDir must collapse repeated separators like Go path.Join"
    );
    // Level dir inherits the same cleaning.
    assert_eq!(
        ltx_level_dir("foo/", 0),
        "foo/ltx/0",
        "LTXLevelDir must clean like Go path.Join (litestream.go:190-191)"
    );
}

// ── REVIEWER (T1 review): path.Clean ".." backtrack divergence ────────────────
//
// The sibling test above only exercises trailing/doubled-slash collapsing, which
// the current `path_clean` (src/lib.rs) handles. It does NOT exercise a `..`
// element that backtracks over a *surviving* prefix element — and that path is
// BROKEN: the backtrack loop pops the element body but leaves the preceding '/'
// in the buffer (Go's `lazybuf` cursor logically excludes it), so the next
// element is appended after a doubled separator.
//
// Demonstration (Rust currently, confirmed by running the code):
//   path_clean("a/b/../c") = "a//c"   (Go path.Clean: "a/c")
// observed via the public helper:
//   ltx_dir("a/b/../c")   = "a//c/ltx"   (must be "a/c/ltx")
//
// EXPECTED VALUES ARE FROM THE REAL GO `path.Join` (the upstream oracle, run
// with go1.25 against `path.Join(root, "ltx")`), never from rustyriver — per
// AGENTS.md rule 3:
//   path.Join("a/b/../c","ltx")   = "a/c/ltx"
//   path.Join("x/y/../z","ltx")   = "x/z/ltx"
//   path.Join("a/b/c/../d","ltx") = "a/b/d/ltx"
//   path.Join("aa/bb/../cc","ltx")= "aa/cc/ltx"
//
// Why this matters: these helpers compute LTX object-store keys (litestream.go
// 184-197). Any `root` whose cleaned form contains an interior `..` over a
// surviving prefix (e.g. a configured prefix like "bucket/db/../ltxroot") yields
// a doubled-slash S3 key ("bucket//ltxroot/...") — a DIFFERENT object than the
// real binary writes/reads, silently breaking the differential oracle (G3,
// PLAN.md §6.3 D1/D3). A faithful port must match Go's `path.Clean` exactly.
#[test]
fn test_ltx_dir_cleans_dotdot_over_surviving_prefix() {
    // Interior ".." that backtracks over one element, leaving a prefix.
    assert_eq!(
        ltx_dir("a/b/../c"),
        "a/c/ltx",
        "path.Clean must collapse 'a/b/../c' to 'a/c' (no doubled separator)"
    );
    assert_eq!(
        ltx_dir("x/y/../z"),
        "x/z/ltx",
        "path.Clean must collapse 'x/y/../z' to 'x/z'"
    );
    // Backtrack with two surviving prefix elements.
    assert_eq!(
        ltx_dir("a/b/c/../d"),
        "a/b/d/ltx",
        "path.Clean must collapse 'a/b/c/../d' to 'a/b/d'"
    );
    // Multi-char element names (rules out any single-byte coincidence).
    assert_eq!(
        ltx_level_dir("aa/bb/../cc", 0),
        "aa/cc/ltx/0",
        "path.Clean must collapse 'aa/bb/../cc' to 'aa/cc'"
    );
}

// ── LTXFilePath ───────────────────────────────────────────────────────────────
// litestream_test.go:60-65 declares a function named `LTXFilePath` (no Test
// prefix), so Go's test runner never executes it; it also uses "-" as the
// expected value — a placeholder stub.  We port it as a proper Rust test with
// the correct expected value derived from ltx@v0.5.1 ltx.go:487 FormatFilename.
//   TXID(100) = 0x64  →  "0000000000000064"
//   TXID(200) = 0xc8  →  "00000000000000c8"

#[test]
fn test_ltx_file_path() {
    assert_eq!(
        ltx_file_path("foo", 0, TXID(100), TXID(200)),
        "foo/ltx/0/0000000000000064-00000000000000c8.ltx"
    );
}

// ── TXID formatting / parsing ─────────────────────────────────────────────────

#[test]
fn test_txid_display() {
    assert_eq!(TXID(1).to_string(), "0000000000000001");
    assert_eq!(TXID(0).to_string(), "0000000000000000");
    assert_eq!(TXID(u64::MAX).to_string(), "ffffffffffffffff");
    assert_eq!(TXID(0x64).to_string(), "0000000000000064");
}

#[test]
fn test_txid_roundtrip() {
    for v in [0u64, 1, 100, 200, 0xdead_beef_cafe_babe, u64::MAX] {
        let s = TXID(v).to_string();
        assert_eq!(s.len(), 16, "must be 16 chars: {s:?}");
        let parsed = parse_txid(&s).expect("parse must succeed");
        assert_eq!(parsed, TXID(v));
    }
}

#[test]
fn test_parse_txid_wrong_length() {
    assert!(parse_txid("000000000000001").is_err()); // 15 chars
    assert!(parse_txid("00000000000000001").is_err()); // 17 chars
    assert!(parse_txid("").is_err());
}

#[test]
fn test_parse_txid_invalid_chars() {
    assert!(parse_txid("000000000000001g").is_err());
}

// ── REVIEWER (T4): leading-sign divergence vs Go strconv.ParseUint ─────────────
//
// Upstream `ParseTXID` (ltx@v0.5.1 ltx.go:130-138) and `ParseChecksum`
// (checksum.go:135-144) both delegate to Go's `strconv.ParseUint(s, 16, 64)`.
// Per the Go stdlib contract, ParseUint **does not permit a sign prefix**:
//   strconv.ParseUint("+000000000000abc", 16, 64) -> error.
//
// The Rust port (lib.rs `parse_txid` / `parse_pos`) uses
// `u64::from_str_radix`, which **accepts a leading '+'**. So a 16-char,
// '+'-prefixed string is rejected by upstream Litestream but ACCEPTED by
// rustyriver — an observable Err-vs-Ok behavioral divergence on the public
// parse surface. A faithful port must reject the leading sign.
#[test]
fn test_parse_txid_rejects_leading_plus_sign() {
    // 16 characters, leading '+'. Go's strconv.ParseUint forbids sign prefixes,
    // so upstream ParseTXID returns an error here. rustyriver's
    // u64::from_str_radix accepts '+', so it currently returns Ok(...) — a real
    // Err-vs-Ok divergence on the public parse surface. The faithful port must
    // reject the leading sign.
    assert!(
        parse_txid("+000000000000abc").is_err(),
        "Go strconv.ParseUint forbids a leading '+'; rustyriver must reject it too \
         (diverges from ltx@v0.5.1 ParseTXID, which uses strconv.ParseUint)"
    );
}

// ── Pos formatting / parsing ──────────────────────────────────────────────────

#[test]
fn test_pos_display() {
    let p = Pos::new(TXID(1), 0x8000_0000_0000_0001);
    assert_eq!(p.to_string(), "0000000000000001/8000000000000001");
}

#[test]
fn test_pos_is_zero() {
    assert!(Pos::ZERO.is_zero());
    assert!(!Pos::new(TXID(1), 0).is_zero());
}

#[test]
fn test_parse_pos_roundtrip() {
    let p = Pos::new(TXID(42), 0x8000_0000_0000_0042);
    let s = p.to_string();
    let p2 = parse_pos(&s).expect("parse must succeed");
    assert_eq!(p, p2);
}

#[test]
fn test_parse_pos_wrong_length() {
    assert!(parse_pos("").is_err());
    assert!(parse_pos("0000000000000001/800000000000000").is_err()); // 32 chars
    assert!(parse_pos("0000000000000001/80000000000000001").is_err()); // 34 chars
}

// ── TestNewLTXError ───────────────────────────────────────────────────────────
// Ported from litestream_test.go:75-124

#[test]
fn test_new_ltx_error_missing_file_has_hint() {
    let err = new_ltx_error(
        "open",
        "/path/to/file.ltx",
        0,
        1,
        1,
        Error::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "file not found",
        )),
    );
    assert!(!err.hint.is_empty(), "expected hint for missing file error");
    assert!(
        err.hint.contains("missing"),
        "hint should mention missing: {}",
        err.hint
    );
    assert!(
        err.hint.contains("litestream reset"),
        "hint should mention reset: {}",
        err.hint
    );
}

#[test]
fn test_new_ltx_error_corrupted_has_hint() {
    let err = new_ltx_error("decode", "/path/to/file.ltx", 0, 1, 1, Error::LTXCorrupted);
    assert!(
        !err.hint.is_empty(),
        "expected hint for corrupted file error"
    );
    assert!(
        err.hint.contains("corrupted"),
        "hint should mention corruption: {}",
        err.hint
    );
}

#[test]
fn test_new_ltx_error_checksum_mismatch_has_hint() {
    let err = new_ltx_error(
        "validate",
        "/path/to/file.ltx",
        0,
        1,
        1,
        Error::ChecksumMismatch,
    );
    assert!(!err.hint.is_empty(), "expected hint for checksum mismatch");
}

#[test]
fn test_new_ltx_error_string_contains_op_and_path() {
    let err = new_ltx_error(
        "open",
        "/path/to/file.ltx",
        0,
        1,
        1,
        Error::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "file not found",
        )),
    );
    let s = err.to_string();
    assert!(s.contains("open"), "error should contain operation: {s}");
    assert!(
        s.contains("/path/to/file.ltx"),
        "error should contain path: {s}"
    );
}

#[test]
fn test_new_ltx_error_unwrap() {
    // Ported from TestNewLTXError/Unwrap — litestream_test.go:117-123
    // Go: errors.Is(err, underlying). Rust: source() must be Some.
    use std::error::Error as StdError;
    let err = new_ltx_error("read", "/path/to/file.ltx", 0, 1, 1, Error::LTXMissing);
    assert!(
        err.source().is_some(),
        "LTXError should expose a source error"
    );
}

// ── TestLTXErrorHints ─────────────────────────────────────────────────────────
// Ported from litestream_test.go:126-137

#[test]
fn test_ltx_error_hints_ltx_missing() {
    let err = new_ltx_error("open", "/path/to/file.ltx", 0, 1, 1, Error::LTXMissing);
    assert!(!err.hint.is_empty(), "expected hint for ErrLTXMissing");
    assert!(
        err.hint.contains("litestream reset"),
        "hint should mention reset: {}",
        err.hint
    );
}

// ── TestLTXError_IsAutoRecoverable ────────────────────────────────────────────
// Ported from litestream_test.go:139-163

#[test]
fn test_ltx_error_is_auto_recoverable() {
    struct Case {
        name: &'static str,
        err: Error,
        recoverable: bool,
    }

    let cases = vec![
        Case {
            name: "NotExist",
            err: Error::Io(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "not found",
            )),
            recoverable: true,
        },
        Case {
            name: "LTXMissing",
            err: Error::LTXMissing,
            recoverable: true,
        },
        Case {
            name: "LTXCorrupted",
            err: Error::LTXCorrupted,
            recoverable: true,
        },
        Case {
            name: "ChecksumMismatch",
            err: Error::ChecksumMismatch,
            recoverable: true,
        },
        Case {
            // Go: {"WrappedCorrupted", fmt.Errorf("%w: bad data", ErrLTXCorrupted), true}
            // Mirrors a low-level reader returning context-wrapped corruption;
            // errors.Is unwraps the chain, so still auto-recoverable.
            name: "WrappedCorrupted",
            err: Error::Other(Box::new(Error::LTXCorrupted)),
            recoverable: true,
        },
        Case {
            name: "PermissionDenied",
            err: Error::Io(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "permission denied",
            )),
            recoverable: false,
        },
        Case {
            name: "GenericError",
            err: Error::Other("something went wrong".into()),
            recoverable: false,
        },
        Case {
            // Go: {"IOError", fmt.Errorf("disk failure: %w", errors.New("EIO")), false}
            // A generic wrapped I/O error (not NotFound) is not auto-recoverable.
            name: "IOError",
            err: Error::Other(Box::new(std::io::Error::other("EIO"))),
            recoverable: false,
        },
    ];

    for c in cases {
        let ltx_err = new_ltx_error("open", "/path/to/file.ltx", 0, 1, 1, c.err);
        assert_eq!(
            ltx_err.is_auto_recoverable(),
            c.recoverable,
            "case '{}' failed",
            c.name
        );
    }
}

// ── REVIEWER (T4): standalone verification of the `WrappedCorrupted` sub-case ──
//
// The Go suite TestLTXError_IsAutoRecoverable (litestream_test.go:139-163) has
// EIGHT sub-cases, all of which are already covered by
// test_ltx_error_is_auto_recoverable above (including WrappedCorrupted at
// litestream_test.go:155-158 and IOError at litestream_test.go:159-162).
//
// This standalone test isolates the chain-walking behaviour for the
// semantically load-bearing sub-case:
//
//     {"WrappedCorrupted", fmt.Errorf("%w: bad data", litestream.ErrLTXCorrupted), true}
//
// In Go, `errors.Is` unwraps the chain, so a *wrapped* corruption sentinel is
// still auto-recoverable. This is the normal production shape: a low-level
// reader returns context-wrapped corruption (e.g. "reading page 7: <corrupt>")
// and the caller hands it to NewLTXError. Recovery MUST still trigger.
//
// The faithful Rust analog of `%w`-wrapping the sentinel is to carry the
// corruption inside the error chain (Error::Other(Box::new(Error::LTXCorrupted)))
// rather than as the bare top-level discriminant. The chain-walking in
// error.rs must recognise this wrapped form and return true.
#[test]
fn test_ltx_error_is_auto_recoverable_wrapped_corrupted() {
    // Wrap the corruption sentinel with added context, mirroring
    // fmt.Errorf("%w: bad data", ErrLTXCorrupted).
    let wrapped: Box<dyn std::error::Error + Send + Sync> =
        Box::new(Error::LTXCorrupted) as Box<dyn std::error::Error + Send + Sync>;
    let err = new_ltx_error("open", "/path/to/file.ltx", 0, 1, 1, Error::Other(wrapped));
    assert!(
        err.is_auto_recoverable(),
        "wrapped ErrLTXCorrupted must be auto-recoverable (Go: errors.Is unwraps the chain)"
    );
}
