//! integration_minio — runs the T5 generic `run_client_suite` against the T7
//! `ObjectStoreClient` backed by a **real MinIO** instance. This is the T7
//! Definition-of-Done gate ("passes the conformance suite against MinIO").
//!
//! The Go test file (`s3/replica_client_test.go`) has no live-MinIO test — those
//! live in the integration suite — so the conformance contract is the same
//! `run_client_suite` the file client (T6) passes, exercised here over S3.
//!
//! MinIO is **optional**: the test connects to the endpoint first and, if it is
//! unreachable (no Docker / no MinIO), it prints a clear SKIP note and returns
//! green rather than blocking the gate (per the task brief). To run it, start
//! MinIO and create the bucket:
//!
//! ```sh
//! docker run -d --name minio -p 9000:9000 \
//!   -e MINIO_ROOT_USER=minioadmin -e MINIO_ROOT_PASSWORD=minioadmin \
//!   minio/minio:latest server /data
//! # then: mc mb local/litestream  (or any S3 client)
//! ```
//!
//! Configuration (all have defaults matching the command above):
//!   * `RUSTYRIVER_MINIO_ENDPOINT`  (default `http://127.0.0.1:9000`)
//!   * `RUSTYRIVER_MINIO_BUCKET`    (default `litestream`)
//!   * `AWS_ACCESS_KEY_ID`          (default `minioadmin`)
//!   * `AWS_SECRET_ACCESS_KEY`      (default `minioadmin`)

#![cfg(feature = "s3")]

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rustyriver::client::object_store::{ObjectStoreClient, ObjectStoreConfig};
use rustyriver::client::{run_client_suite, ReplicaClient};
use rustyriver::ltx::{self, Header, HEADER_FLAG_NO_CHECKSUM, VERSION};
use rustyriver::TXID;

const DEFAULT_ENDPOINT: &str = "http://127.0.0.1:9000";
const DEFAULT_BUCKET: &str = "litestream";
const DEFAULT_KEY: &str = "minioadmin";

fn endpoint() -> String {
    std::env::var("RUSTYRIVER_MINIO_ENDPOINT").unwrap_or_else(|_| DEFAULT_ENDPOINT.to_string())
}
fn bucket() -> String {
    std::env::var("RUSTYRIVER_MINIO_BUCKET").unwrap_or_else(|_| DEFAULT_BUCKET.to_string())
}
fn access_key() -> String {
    std::env::var("AWS_ACCESS_KEY_ID").unwrap_or_else(|_| DEFAULT_KEY.to_string())
}
fn secret_key() -> String {
    std::env::var("AWS_SECRET_ACCESS_KEY").unwrap_or_else(|_| DEFAULT_KEY.to_string())
}

/// Returns `(host, port)` parsed from an `http://host:port` endpoint.
fn endpoint_host_port(ep: &str) -> Option<(String, u16)> {
    let rest = ep
        .strip_prefix("http://")
        .or_else(|| ep.strip_prefix("https://"))?;
    let hostport = rest.split('/').next()?;
    let (host, port) = hostport.rsplit_once(':')?;
    Some((host.to_string(), port.parse().ok()?))
}

/// `true` if a TCP connection to the MinIO endpoint succeeds within a short
/// timeout. Used to decide skip-vs-run without blocking.
fn minio_reachable() -> bool {
    use std::net::ToSocketAddrs;
    let ep = endpoint();
    let Some((host, port)) = endpoint_host_port(&ep) else {
        return false;
    };
    let Ok(mut addrs) = (host.as_str(), port).to_socket_addrs() else {
        return false;
    };
    let Some(addr) = addrs.next() else {
        return false;
    };
    std::net::TcpStream::connect_timeout(&addr, Duration::from_millis(750)).is_ok()
}

/// A unique path prefix per test run so concurrent / repeated runs don't collide
/// and the suite always starts from a clean slate (it `delete_all`s first).
fn unique_path(tag: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("rustyriver-test/{tag}/{nanos}")
}

fn make_client(path: String) -> ObjectStoreClient {
    let config = ObjectStoreConfig {
        bucket: bucket(),
        path,
        region: "us-east-1".into(),
        endpoint: endpoint(),
        access_key_id: access_key(),
        secret_access_key: secret_key(),
        force_path_style: true, // MinIO requires path-style
        ..Default::default()
    };
    ObjectStoreClient::new(config)
}

#[tokio::test]
async fn object_store_passes_conformance_suite_vs_minio() {
    if !minio_reachable() {
        eprintln!(
            "SKIP object_store_passes_conformance_suite_vs_minio: MinIO not reachable at {} \
             (start it with `docker run -d --name minio -p 9000:9000 -e MINIO_ROOT_USER=minioadmin \
             -e MINIO_ROOT_PASSWORD=minioadmin minio/minio:latest server /data` and create the \
             `{}` bucket). The client code is exercised by the pure-unit tests regardless.",
            endpoint(),
            bucket(),
        );
        return;
    }

    let client = make_client(unique_path("conformance"));
    // Surface a build/connect failure clearly instead of a generic panic.
    client
        .init()
        .await
        .expect("init ObjectStoreClient against MinIO");

    run_client_suite(&client).await;

    // Leave no residue behind.
    client.delete_all().await.expect("final cleanup");
}

/// Exercises the timestamp-metadata path (`use_metadata = true`): the
/// `litestream-timestamp` header value written on PUT must be read back via the
/// `head` fan-out, distinct from the listing's `last_modified`. Ported in spirit
/// from s3/replica_client.go:679-683 + 1543-1553.
#[tokio::test]
async fn object_store_use_metadata_reads_header_timestamp() {
    if !minio_reachable() {
        eprintln!("SKIP object_store_use_metadata_reads_header_timestamp: MinIO not reachable");
        return;
    }

    let client = make_client(unique_path("metadata"));
    client.delete_all().await.expect("clean start");

    // Build an LTX file with a specific, non-"now" header timestamp.
    // 2021-01-01T00:00:00.123Z = 1_609_459_200_123 ms.
    let header_millis: i64 = 1_609_459_200_123;
    let data = make_ltx_with_timestamp(TXID(1), TXID(1), 7, header_millis);

    client
        .write_ltx_file(0, TXID(1), TXID(1), &data)
        .await
        .expect("write ltx with timestamp");

    // With use_metadata, created_at must equal the header timestamp we stored,
    // NOT the wall-clock LastModified of the object.
    let files = client
        .ltx_files(0, TXID(0), true)
        .await
        .expect("list with metadata");
    assert_eq!(files.len(), 1, "one file listed");
    let created = files[0].created_at.expect("created_at present");
    let got_millis = created
        .duration_since(UNIX_EPOCH)
        .expect("after epoch")
        .as_millis() as i64;
    assert_eq!(
        got_millis, header_millis,
        "use_metadata reads back the LTX header timestamp, not LastModified"
    );

    client.delete_all().await.expect("cleanup");
}

/// The 5 MiB multipart boundary: a file at or above the threshold is uploaded
/// via multipart and round-trips byte-exact. Ported (boundary only) from
/// `TestReplicaClient_MultipartUploadThreshold` (s3/replica_client_test.go:351).
#[tokio::test]
async fn object_store_multipart_upload_round_trips() {
    if !minio_reachable() {
        eprintln!("SKIP object_store_multipart_upload_round_trips: MinIO not reachable");
        return;
    }

    let client = make_client(unique_path("multipart"));
    client.delete_all().await.expect("clean start");

    // A ~6 MiB LTX file (above the 5 MiB single-PUT threshold) forces the
    // multipart path. We pad an LTX file's pages so the encoded file exceeds
    // 5 MiB while still being a real, decodable LTX file.
    let data = make_large_ltx(TXID(1), TXID(1), 6 * 1024 * 1024);
    assert!(
        data.len() >= 5 * 1024 * 1024,
        "test file must exceed the multipart threshold (got {} bytes)",
        data.len()
    );

    let info = client
        .write_ltx_file(0, TXID(1), TXID(1), &data)
        .await
        .expect("multipart write");
    assert_eq!(info.size, data.len() as i64);

    // Full read returns identical bytes.
    let got = client
        .open_ltx_file(0, TXID(1), TXID(1), 0, 0)
        .await
        .expect("read back multipart object");
    assert_eq!(got, data, "multipart round-trip is byte-exact");

    // Partial (byte-range) read still works on a multipart object.
    let mid = client
        .open_ltx_file(0, TXID(1), TXID(1), 100, 64)
        .await
        .expect("range read");
    assert_eq!(mid.as_slice(), &data[100..164], "range read window");

    client.delete_all().await.expect("cleanup");
}

// ── LTX builders (real, decodable files; mirror client::make_test_ltx_file) ───

fn header(min_txid: TXID, max_txid: TXID, page_size: u32, timestamp: i64, commit: u32) -> Header {
    Header {
        version: VERSION,
        flags: HEADER_FLAG_NO_CHECKSUM,
        page_size,
        commit,
        min_txid,
        max_txid,
        timestamp,
        pre_apply_checksum: 0,
        wal_offset: 0,
        wal_size: 0,
        wal_salt1: 0,
        wal_salt2: 0,
        node_id: 0,
    }
}

fn make_ltx_with_timestamp(min_txid: TXID, max_txid: TXID, seed: u8, timestamp: i64) -> Vec<u8> {
    let page_size: u32 = 512;
    let pages = vec![(1u32, vec![seed; page_size as usize])];
    let hdr = header(min_txid, max_txid, page_size, timestamp, 1);
    ltx::encode_file(&hdr, &pages, 0).expect("encode ltx")
}

/// Build a real LTX file whose *encoded* size is at least `min_bytes` by writing
/// enough pages of incompressible bytes. The lock page is skipped (LTX forbids
/// it). Page content comes from a SplitMix64 PRNG seeded per page, so LZ4 cannot
/// shrink it — keeping the encoded file above the 5 MiB multipart threshold.
fn make_large_ltx(min_txid: TXID, max_txid: TXID, min_bytes: usize) -> Vec<u8> {
    let page_size: u32 = 4096;
    let lock = ltx::lock_pgno(page_size);
    let n_pages = (min_bytes / page_size as usize) + 8;

    let mut pages: Vec<(u32, Vec<u8>)> = Vec::with_capacity(n_pages);
    let mut pgno: u32 = 1;
    let mut commit: u32 = 0;
    while pages.len() < n_pages {
        if pgno == lock {
            pgno += 1;
            continue;
        }
        // SplitMix64-filled page: high-entropy, incompressible, deterministic.
        let mut state: u64 = 0x9E37_79B9_7F4A_7C15u64.wrapping_mul(pgno as u64 + 1);
        let mut buf = vec![0u8; page_size as usize];
        let mut i = 0;
        while i < buf.len() {
            state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = state;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^= z >> 31;
            let bytes = z.to_le_bytes();
            let take = bytes.len().min(buf.len() - i);
            buf[i..i + take].copy_from_slice(&bytes[..take]);
            i += take;
        }
        pages.push((pgno, buf));
        commit = pgno;
        pgno += 1;
    }

    let hdr = header(min_txid, max_txid, page_size, 0, commit);
    ltx::encode_file(&hdr, &pages, 0).expect("encode large ltx")
}
