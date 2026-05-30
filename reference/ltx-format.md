# LTX file format (Version 3) — byte layout

Derived by reading `reference/ltx-go` @ v0.5.1 (`ltx.go`, `checksum.go`,
`encoder.go`, `decoder.go`). This is the spec `src/ltx.rs` (T2) implements.
All multi-byte integers are **big-endian**. (Produced in T2; not a vendored file.)

## File structure

```
+----------------------+
| Header   (100 bytes) |
+----------------------+
| Page 0               |   PageHeader(6) + LZ4-frame(page data)
| Page 1               |
| ...                  |
+----------------------+
| Empty PageHeader (6) |   pgno=0 — terminates the page block
+----------------------+
| Page index           |   varint tuples + end marker + u64 size
+----------------------+
| Trailer  (16 bytes)  |
+----------------------+
```

## Header (100 bytes)

| Offset | Size | Field | Notes |
|-------:|-----:|-------|-------|
| 0  | 4 | Magic | ASCII `"LTX1"` |
| 4  | 4 | Flags (u32) | only `HeaderFlagNoChecksum = 1<<1` is valid |
| 8  | 4 | PageSize (u32) | power of two, 512..=65536 |
| 12 | 4 | Commit (u32) | DB size after txn, in pages |
| 16 | 8 | MinTXID (u64) | |
| 24 | 8 | MaxTXID (u64) | |
| 32 | 8 | Timestamp (i64) | ms since unix epoch |
| 40 | 8 | PreApplyChecksum (u64) | 0 on snapshots (MinTXID==1) |
| 48 | 8 | WALOffset (i64) | 0 if journal |
| 56 | 8 | WALSize (i64) | 0 if journal |
| 64 | 4 | WALSalt1 (u32) | |
| 68 | 4 | WALSalt2 (u32) | |
| 72 | 8 | NodeID (u64) | |
| 80 | 20 | (reserved) | zero |

`IsSnapshot() == (MinTXID == 1)`. Snapshots include all pages and have
`PreApplyChecksum == 0`.

## Page block

Each page: `PageHeader` then the page data as **one independent LZ4 frame**
(pierrec/lz4 v4, 64 KiB block, "Fast" level). The block ends with an **empty
PageHeader** (`pgno == 0`).

**PageHeader (6 bytes):** `Pgno (u32 @0)`, `Flags (u16 @4)` (must be 0).

## Page index

After the empty page header, for each page in ascending pgno order:
`uvarint(pgno) ++ uvarint(offset) ++ uvarint(size)` where `offset` is the page's
byte offset from file start and `size = 6 + len(compressed page data)`. Terminated
by `uvarint(0)`, then a `u64` big-endian giving the byte length of the index
region (elements + terminator, **excluding** this size field).

## Trailer (16 bytes)

`PostApplyChecksum (u64 @0)`, `FileChecksum (u64 @8)`.

## Checksums — CRC64-ISO with the ChecksumFlag

- Hasher: **CRC64 / ISO polynomial `0xD800000000000000`**, matching Go
  `crc64.MakeTable(crc64.ISO)` (reflected; init 0; per-update invert-process-invert).
- `ChecksumFlag = 1 << 63` is OR-ed into **every** stored checksum (so it is never 0).
- `ChecksumPage(pgno, data) = ChecksumFlag | CRC64( BE_u32(pgno) ++ data )`.
- Rolling DB checksum (post-apply): start `ChecksumFlag`, then for each non-lock
  page `chksum = ChecksumFlag | (chksum ^ ChecksumPage(pgno, data))`. Verified
  against the trailer's PostApplyChecksum **only for snapshots**.
- **FileChecksum** = `ChecksumFlag | CRC64(feed)` where `feed` is, in write order:
  `header(100)` ++ for each page `[ pageheader(6) ++ DECOMPRESSED data ]` ++
  `empty pageheader(6)` ++ `page index region (incl. u64 size field)` ++
  `trailer[0..8]` (PostApplyChecksum). The page **data is fed uncompressed**, so a
  decoder must decompress and re-feed — it cannot hash the raw file bytes.

## Filename

`<minTXID:016x>-<maxTXID:016x>.ltx`, e.g. `0000000000000001-0000000000000001.ltx`.
