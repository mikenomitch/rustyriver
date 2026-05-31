//! fuzz_parsers — T16 / Gate G4: robustness (fuzz) targets for the LTX and WAL
//! parsers.
//!
//! ## Contract under test
//! The two byte-format readers in this crate — [`rustyriver::ltx`]'s decode
//! family ([`decode_file`], [`decode_file_pages`], [`decode_database_image`], and
//! the `Header`/`PageHeader`/`Trailer` field parsers) and the SQLite WAL reader
//! ([`WalReader`]) — are the **untrusted-input boundary**: every byte they see
//! comes from object storage or a possibly-truncated/partially-written WAL on
//! disk. The hard invariant for that boundary is:
//!
//! > For **any** input slice — empty, truncated, random, or an adversarially
//! > mutated copy of a real file — these functions return `Err` (or a valid
//! > parse). They MUST NOT panic, abort, hang, or read out of bounds.
//!
//! This mirrors the spirit of the upstream Go fuzz target
//! `restore_fuzz_test.go` (`FuzzRestoreWithMissingCompactedFile`), which asserts
//! the *restore pipeline* survives a randomly-deleted compacted file. We do not
//! have compaction (KEEP scope is L0-only), so this suite instead drives the two
//! lower-level parsers directly with malformed bytes — the resilience surface
//! that actually parses untrusted input. Go gets this no-panic property "for
//! free" from `recover()`/bounds-checked slices returning errors; in Rust an
//! out-of-range slice *panics*, so the property must be asserted explicitly.
//!
//! ## How "no panic" is asserted
//! Each parser call runs inside [`std::panic::catch_unwind`]. A caught panic
//! fails the test and prints the exact input bytes (hex) plus the deterministic
//! seed, so any regression is reproducible. A returned `Err` (or `Ok`) is the
//! pass condition. (The panic hook is silenced for the duration so a *passing*
//! run is quiet even though we are deliberately not panicking.)
//!
//! ## Determinism & budget
//! cargo-fuzz is not wired up in this repo (no nightly/libFuzzer dependency in
//! KEEP scope), so this is the "bounded in-tree fuzz-style test" the task allows:
//! a fixed iteration budget driven by a small, self-contained SplitMix64 PRNG
//! seeded from a constant. No new dependency, and every iteration is byte-for-byte
//! reproducible from [`SEED`]. The corpus is the union of:
//!   * a hand-written **adversarial** set (empty, 1 byte, exactly a header,
//!     all-zeros, valid-magic-then-garbage, huge declared index size, …),
//!   * **random** buffers of random length, and
//!   * **mutated golden** bytes — the real-litestream L0 `.ltx` files and the
//!     golden SQLite WAL, hit with bit flips, byte sets, truncations, splices and
//!     zero-runs (the highest-signal fuzz inputs: structurally *almost* valid).

use rustyriver::ltx;
use rustyriver::wal::WalReader;
use std::path::PathBuf;

// ── Iteration budget (bounded so `cargo test` stays fast & reproducible) ─────

/// Random-buffer iterations per parser.
const RANDOM_ITERS: usize = 20_000;
/// Mutation iterations per golden seed file. Bounded so the whole suite stays
/// fast: each "almost valid" mutation can drive a full decode (LZ4 + checksum)
/// across all three decode entrypoints, so this is the dominant cost. 800 × 6
/// LTX seeds (= 4_800 mutated files) still hits every one of the 9 mutation
/// strategies hundreds of times each. Raise locally for a longer soak.
const MUTATION_ITERS: usize = 800;
/// Fixed PRNG seed — every run is identical; a failure reproduces from this.
const SEED: u64 = 0x5279_5374_795F_5232; // "RySty_R2"

// ── Self-contained deterministic PRNG (SplitMix64) ───────────────────────────
//
// A tiny, well-known generator: zero dependencies, fully reproducible, fast
// enough for tens of thousands of iterations. We only need "random-ish" bytes to
// poke the parsers; statistical quality is irrelevant.
struct SplitMix64(u64);

impl SplitMix64 {
    fn new(seed: u64) -> Self {
        SplitMix64(seed)
    }
    #[inline]
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    #[inline]
    fn next_byte(&mut self) -> u8 {
        self.next_u64() as u8
    }
    /// Uniform-ish in `[0, n)` for small `n` (modulo bias is irrelevant here).
    #[inline]
    fn below(&mut self, n: usize) -> usize {
        if n == 0 {
            0
        } else {
            (self.next_u64() % n as u64) as usize
        }
    }
    fn fill(&mut self, buf: &mut [u8]) {
        for b in buf.iter_mut() {
            *b = self.next_byte();
        }
    }
}

// ── Golden-corpus loaders (read-only ground truth) ───────────────────────────

/// All six real-litestream golden L0 `.ltx` files (immutable fixtures).
fn golden_ltx_files() -> Vec<Vec<u8>> {
    let mut base = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    base.push("tests/fixtures/golden/replica/ltx/0");
    (1u64..=6)
        .map(|i| {
            let name = format!("{:016x}-{:016x}.ltx", i, i);
            let p = base.join(&name);
            std::fs::read(&p).unwrap_or_else(|e| panic!("read {}: {e}", p.display()))
        })
        .collect()
}

/// The golden SQLite WAL fixture (immutable).
fn golden_wal() -> Vec<u8> {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.push("tests/fixtures/golden/sample.wal");
    std::fs::read(&p).unwrap_or_else(|e| panic!("read {}: {e}", p.display()))
}

// ── Panic-capture harness ────────────────────────────────────────────────────

/// Runs `f` (a parser call on `input`) and converts a panic into a test failure
/// that prints the offending bytes + seed for reproduction. The return value of
/// `f` is intentionally discarded — we only care that it returned *something*
/// rather than unwinding.
fn assert_no_panic<T>(label: &str, input: &[u8], f: impl FnOnce() -> T + std::panic::UnwindSafe) {
    let result = std::panic::catch_unwind(f);
    if result.is_err() {
        panic!(
            "PANIC in {label} on {}-byte input (must return Err, never panic).\n\
             seed=0x{SEED:016x}\nbytes(hex)={}",
            input.len(),
            hex_preview(input),
        );
    }
}

/// Hex of the input, capped so a failure message stays readable for big buffers.
fn hex_preview(b: &[u8]) -> String {
    const CAP: usize = 512;
    let shown = &b[..b.len().min(CAP)];
    let mut s = String::with_capacity(shown.len() * 2 + 16);
    for byte in shown {
        s.push_str(&format!("{byte:02x}"));
    }
    if b.len() > CAP {
        s.push_str(&format!("… (+{} more bytes)", b.len() - CAP));
    }
    s
}

/// Drives EVERY LTX parser entrypoint over one input and asserts none panic.
/// This is the function whose no-panic property the whole suite is protecting.
fn exercise_ltx(input: &[u8]) {
    // The three public decode entrypoints (the untrusted-input boundary the
    // restore path and fault-injection rely on).
    assert_no_panic("ltx::decode_file", input, || {
        let _ = ltx::decode_file(input);
    });
    assert_no_panic("ltx::decode_file_pages", input, || {
        let _ = ltx::decode_file_pages(input);
    });
    assert_no_panic("ltx::decode_database_image", input, || {
        let _ = ltx::decode_database_image(input);
    });
    // The fixed-width field parsers (called with the raw slice; each must guard
    // its own length rather than slicing blindly).
    assert_no_panic("ltx::Header::parse", input, || {
        let _ = ltx::Header::parse(input);
    });
    assert_no_panic("ltx::PageHeader::parse", input, || {
        let _ = ltx::PageHeader::parse(input);
    });
    assert_no_panic("ltx::Trailer::parse", input, || {
        let _ = ltx::Trailer::parse(input);
    });
}

/// Drives the WAL reader over one input and asserts no panic: construction, a
/// bounded `read_frame` drain, the `page_map` aggregation, a `frame_salts_until`
/// scan, and `new_with_offset` at a handful of adversarial offsets.
fn exercise_wal(input: &[u8]) {
    assert_no_panic("WalReader::new+read_frame", input, || {
        if let Ok(mut r) = WalReader::new(input) {
            // Drain frames with a hard cap so a crafted header advertising a
            // tiny page size can't make this loop run unboundedly.
            let ps = r.page_size() as usize;
            // A zero page size would make `read_frame`'s buffer empty; the reader
            // must still terminate (it returns BufferSize/EOF), but guard anyway.
            let mut buf = vec![0u8; ps];
            for _ in 0..100_000 {
                match r.read_frame(&mut buf) {
                    Ok(_) => {}
                    Err(_) => break,
                }
            }
        }
    });

    assert_no_panic("WalReader::page_map", input, || {
        if let Ok(mut r) = WalReader::new(input) {
            let _ = r.page_map();
        }
    });

    assert_no_panic("WalReader::frame_salts_until", input, || {
        if let Ok(r) = WalReader::new(input) {
            // Until a salt that very likely never appears → forces a full scan to
            // EOF (the longest path through the scanner).
            let _ = r.frame_salts_until((0xDEAD_BEEF, 0xFEED_FACE));
        }
    });

    // Adversarial offsets into new_with_offset: negative-ish (0), the header
    // boundary, unaligned, and well past EOF. None may panic.
    for &off in &[
        0i64,
        rustyriver::WAL_HEADER_SIZE as i64,
        rustyriver::WAL_HEADER_SIZE as i64 + 7,
        rustyriver::WAL_HEADER_SIZE as i64 + 4096 + 24,
        i64::from(u32::MAX),
        i64::MAX,
    ] {
        assert_no_panic("WalReader::new_with_offset", input, move || {
            let _ = WalReader::new_with_offset(input, off, 0x1234_5678, 0x9ABC_DEF0);
        });
    }
}

// ── Byte-mutation strategies (applied to a fresh copy of a golden file) ──────

/// Mutates `seed_bytes` in place with one randomly-chosen strategy and returns
/// the result. Strategies are weighted toward "structurally almost valid"
/// mutations (single/multi bit flips, field-sized byte sets, truncations) — the
/// inputs most likely to slip past an early guard and reach deep indexing.
fn mutate(rng: &mut SplitMix64, seed_bytes: &[u8]) -> Vec<u8> {
    let mut b = seed_bytes.to_vec();
    match rng.below(9) {
        // Flip a single bit.
        0 => {
            if !b.is_empty() {
                let i = rng.below(b.len());
                b[i] ^= 1 << rng.below(8);
            }
        }
        // Flip several bits at random positions.
        1 => {
            let k = 1 + rng.below(16);
            for _ in 0..k {
                if b.is_empty() {
                    break;
                }
                let i = rng.below(b.len());
                b[i] ^= 1 << rng.below(8);
            }
        }
        // Set a random byte to a random value.
        2 => {
            if !b.is_empty() {
                let i = rng.below(b.len());
                b[i] = rng.next_byte();
            }
        }
        // Overwrite a contiguous run with random bytes (corrupt a whole field).
        3 => {
            if !b.is_empty() {
                let start = rng.below(b.len());
                let span = 1 + rng.below(16.min(b.len() - start).max(1));
                for x in b.iter_mut().skip(start).take(span) {
                    *x = rng.next_byte();
                }
            }
        }
        // Truncate to a random shorter length (partial write / short read).
        4 => {
            let new_len = rng.below(b.len() + 1);
            b.truncate(new_len);
        }
        // Truncate to *just below* a structural boundary (header / +page header /
        // index size field) — boundary-adjacent lengths flush off-by-one bugs.
        5 => {
            let boundaries = [
                ltx::HEADER_SIZE,
                ltx::HEADER_SIZE + ltx::PAGE_HEADER_SIZE,
                ltx::HEADER_SIZE + 1,
                ltx::TRAILER_SIZE,
                ltx::TRAILER_SIZE + 8,
                b.len().saturating_sub(1),
                b.len().saturating_sub(ltx::TRAILER_SIZE),
            ];
            let target = boundaries[rng.below(boundaries.len())];
            b.truncate(target.min(b.len()));
        }
        // Zero out a contiguous run (e.g. wipe the magic, a length field, salts).
        6 => {
            if !b.is_empty() {
                let start = rng.below(b.len());
                let span = 1 + rng.below(32.min(b.len() - start).max(1));
                for x in b.iter_mut().skip(start).take(span) {
                    *x = 0;
                }
            }
        }
        // Append random trailing bytes (overlong file / wrong size field).
        7 => {
            let extra = rng.below(64);
            for _ in 0..extra {
                b.push(rng.next_byte());
            }
        }
        // Splice: drop a random middle chunk (shifts every later offset).
        _ => {
            if b.len() > 2 {
                let start = rng.below(b.len());
                let span = 1 + rng.below((b.len() - start).max(1));
                let end = (start + span).min(b.len());
                b.drain(start..end);
            }
        }
    }
    b
}

// ── Hand-written adversarial corpus (deterministic, always run) ──────────────

/// Structured edge cases that target specific indexing arithmetic in the
/// decoders. These are the cases a human reviewer would reach for; the random +
/// mutation sweeps then widen coverage around them.
fn adversarial_ltx_corpus() -> Vec<Vec<u8>> {
    let floor = ltx::HEADER_SIZE + ltx::PAGE_HEADER_SIZE + 8 + ltx::TRAILER_SIZE;
    // Degenerate lengths.
    let mut v: Vec<Vec<u8>> = vec![
        vec![],                           // empty
        vec![0x00],                       // 1 byte
        b"LTX".to_vec(),                  // partial magic
        b"LTX1".to_vec(),                 // magic only
        vec![0x00; ltx::HEADER_SIZE - 1], // one short of a header
        vec![0x00; ltx::HEADER_SIZE],     // exactly a (zero) header
        vec![0xFF; ltx::HEADER_SIZE],     // all-ones header
        vec![0x00; floor],                // the decode_file length floor, all zero
    ];

    // Valid magic, then garbage geometry of various plausible sizes.
    for extra in [0usize, 8, 16, 32, 100, 200, 1000] {
        let mut b = vec![0u8; ltx::HEADER_SIZE + extra];
        b[0..4].copy_from_slice(ltx::MAGIC);
        v.push(b);
    }

    // Valid magic + a header whose declared page-index size field is enormous
    // (forces `checked_sub` underflow → must be Err, not panic). Build a buffer
    // at the floor length and write 0xFFFF_FFFF_FFFF_FFFF into the size field.
    {
        let len = floor;
        let mut b = vec![0u8; len];
        b[0..4].copy_from_slice(ltx::MAGIC);
        let size_off = len - ltx::TRAILER_SIZE - 8;
        b[size_off..size_off + 8].copy_from_slice(&u64::MAX.to_be_bytes());
        v.push(b);
    }
    // Same, but index size = exactly size_field_off (idx_start == 0 → below the
    // header floor → Err) and index size = size_field_off+1 (underflow → Err).
    for delta in [0i64, 1, -1] {
        let len = floor + 64;
        let mut b = vec![0u8; len];
        b[0..4].copy_from_slice(ltx::MAGIC);
        let size_off = len - ltx::TRAILER_SIZE - 8;
        let declared = (size_off as i64 + delta).max(0) as u64;
        b[size_off..size_off + 8].copy_from_slice(&declared.to_be_bytes());
        v.push(b);
    }

    v
}

/// Adversarial WAL headers: every magic boundary, zero/huge page sizes, partial
/// headers, and an otherwise-valid-looking header with a bogus checksum.
fn adversarial_wal_corpus() -> Vec<Vec<u8>> {
    let mut v: Vec<Vec<u8>> = vec![
        vec![],                                  // empty
        vec![0x00; 10],                          // partial header
        vec![0x00; rustyriver::WAL_HEADER_SIZE], // zero header (bad magic)
    ];

    // Both valid magics with a zero page-size field (page size 0 is a footgun for
    // the frame loop's buffer sizing).
    for magic in [0x377f_0682u32, 0x377f_0683u32] {
        let mut b = vec![0u8; rustyriver::WAL_HEADER_SIZE];
        b[0..4].copy_from_slice(&magic.to_be_bytes());
        // version = 3007000 so we get past the version gate to the geometry.
        b[4..8].copy_from_slice(&3_007_000u32.to_be_bytes());
        // page size left 0.
        v.push(b);
    }
    // Valid magic + huge page size (0x4000_0000) + a single short frame: the frame
    // loop must not try to allocate/scan past the actual buffer.
    {
        let mut b = vec![0u8; rustyriver::WAL_HEADER_SIZE + 100];
        b[0..4].copy_from_slice(&0x377f_0682u32.to_be_bytes());
        b[4..8].copy_from_slice(&3_007_000u32.to_be_bytes());
        b[8..12].copy_from_slice(&0x4000_0000u32.to_be_bytes());
        v.push(b);
    }
    v
}

// ── Tests ─────────────────────────────────────────────────────────────────────

/// LTX decoders survive the hand-written adversarial corpus with no panic.
#[test]
fn ltx_adversarial_corpus_never_panics() {
    for input in adversarial_ltx_corpus() {
        exercise_ltx(&input);
    }
}

/// **Regression guard**: the exact short-input that used to panic in
/// `decode_database_image` (it sliced `bytes[0..HEADER_SIZE]` before any length
/// check). It must now return an error, never unwind. Kept as a named test so the
/// regression is obvious if it ever reappears.
#[test]
fn ltx_decode_database_image_short_input_returns_err_not_panic() {
    for len in [0usize, 1, 10, 50, ltx::HEADER_SIZE - 1] {
        let input = vec![0u8; len];
        // Must not panic …
        exercise_ltx(&input);
        // … and specifically must be an Err (a short buffer is never a valid DB).
        assert!(
            ltx::decode_database_image(&input).is_err(),
            "decode_database_image on {len} zero bytes must be Err"
        );
    }
}

/// LTX decoders survive a large sweep of fully-random buffers (random length and
/// content) with no panic.
#[test]
fn ltx_random_bytes_never_panic() {
    let mut rng = SplitMix64::new(SEED ^ 0x1111_1111_1111_1111);
    // Bias lengths toward the structurally-interesting small range, but include
    // some large buffers too.
    let max_floor = ltx::HEADER_SIZE + ltx::PAGE_HEADER_SIZE + 8 + ltx::TRAILER_SIZE;
    for _ in 0..RANDOM_ITERS {
        let len = match rng.below(4) {
            0 => rng.below(max_floor + 4), // sub/at the length floor
            1 => max_floor + rng.below(256),
            2 => rng.below(2048),
            _ => rng.below(8192),
        };
        let mut buf = vec![0u8; len];
        rng.fill(&mut buf);
        // ~1 in 6 buffers gets a correct magic so more of them reach deep parsing.
        if len >= 4 && rng.below(6) == 0 {
            buf[0..4].copy_from_slice(ltx::MAGIC);
        }
        exercise_ltx(&buf);
    }
}

/// LTX decoders survive mutated copies of every real golden `.ltx` file. These
/// "almost valid" inputs are the highest-signal fuzz cases — they pass the early
/// magic/length guards and exercise the page-walk, index, and checksum paths.
#[test]
fn ltx_mutated_golden_never_panics() {
    let seeds = golden_ltx_files();
    let mut rng = SplitMix64::new(SEED ^ 0x2222_2222_2222_2222);
    for (idx, seed) in seeds.iter().enumerate() {
        // Sanity: the pristine golden file decodes (proves the seed is real and
        // our mutations are perturbing a genuinely-valid file).
        assert!(
            ltx::decode_file(seed).is_ok(),
            "golden ltx seed #{idx} must decode cleanly before mutation"
        );
        for _ in 0..MUTATION_ITERS {
            let m = mutate(&mut rng, seed);
            exercise_ltx(&m);
        }
    }
}

/// WAL reader survives the hand-written adversarial header corpus with no panic.
#[test]
fn wal_adversarial_corpus_never_panics() {
    for input in adversarial_wal_corpus() {
        exercise_wal(&input);
    }
}

/// WAL reader survives a large sweep of fully-random buffers with no panic.
#[test]
fn wal_random_bytes_never_panic() {
    let mut rng = SplitMix64::new(SEED ^ 0x3333_3333_3333_3333);
    for _ in 0..RANDOM_ITERS {
        let len = match rng.below(4) {
            0 => rng.below(rustyriver::WAL_HEADER_SIZE + 4),
            1 => rustyriver::WAL_HEADER_SIZE + rng.below(256),
            2 => rng.below(4096),
            _ => rng.below(20_000),
        };
        let mut buf = vec![0u8; len];
        rng.fill(&mut buf);
        // Occasionally stamp a valid magic + version so the reader proceeds past
        // the header into frame parsing.
        if len >= rustyriver::WAL_HEADER_SIZE && rng.below(5) == 0 {
            let magic = if rng.below(2) == 0 {
                0x377f_0682u32
            } else {
                0x377f_0683u32
            };
            buf[0..4].copy_from_slice(&magic.to_be_bytes());
            buf[4..8].copy_from_slice(&3_007_000u32.to_be_bytes());
        }
        exercise_wal(&buf);
    }
}

/// WAL reader survives mutated copies of the real golden SQLite WAL with no
/// panic. Bit flips / truncations of a valid WAL probe the header-checksum,
/// salt, frame-checksum, and offset arithmetic on near-valid input.
#[test]
fn wal_mutated_golden_never_panics() {
    let seed = golden_wal();
    // Sanity: the pristine WAL header is accepted (a real, valid seed).
    assert!(
        WalReader::new(&seed).is_ok(),
        "golden WAL must construct a reader before mutation"
    );
    let mut rng = SplitMix64::new(SEED ^ 0x4444_4444_4444_4444);
    for _ in 0..(MUTATION_ITERS * 2) {
        let m = mutate(&mut rng, &seed);
        exercise_wal(&m);
    }
}
