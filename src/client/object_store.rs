//! client::object_store — S3/R2/MinIO `ReplicaClient` via the `object_store` crate.
//!
//! Ported (behavior only) from litestream@v0.5.11 `s3/replica_client.go`. We do
//! **not** use the Go AWS SDK; instead the five `ReplicaClient` operations are
//! mapped onto `object_store::ObjectStore` (the `s3` cargo feature wires the
//! `object_store::aws::AmazonS3Builder` backend). The behavioral invariants that
//! let this client pass the T5 conformance suite against a real MinIO are kept:
//!
//!   * key/path scheme `{path}/{level:04x}/{min}-{max}.ltx`
//!     (s3/replica_client.go:629, 677, 1040-1042);
//!   * the 5 MiB single-PUT vs multipart threshold
//!     (s3/replica_client.go:99 + the Go uploader default);
//!   * list + seek-skip on `min_txid < seek`, ascending TXID order
//!     (s3/replica_client.go:1530-1533);
//!   * the `litestream-timestamp` header-timestamp metadata, written on every
//!     PUT and (on `use_metadata=true`) read back via a parallel `head` fan-out
//!     (s3/replica_client.go:53, 679-683, 1384-1455);
//!   * `NoSuchKey` → `os.ErrNotExist` error mapping
//!     (s3/replica_client.go:647-649, 1662-1668);
//!   * batch DELETE up to 1000 keys per call with per-key error surfacing
//!     (s3/replica_client.go:1028-1101).
//!
//! The provider-defaults table (`ParseHost`, R2 concurrency, path-style flags,
//! endpoint env var) is a faithful port of `NewReplicaClientFromURL`
//! (s3/replica_client.go:133-314) so a `s3://…` URL configures the same way.
//!
//! This whole module is gated behind `#[cfg(feature = "s3")]` because it needs
//! `object_store`'s AWS backend.

#![cfg(feature = "s3")]

use std::sync::Arc;

use futures::stream::{FuturesUnordered, StreamExt, TryStreamExt};
use object_store::aws::AmazonS3Builder;
use object_store::path::Path as ObjPath;
use object_store::{
    Attribute, AttributeValue, Attributes, GetOptions, GetRange, ObjectStore, PutMultipartOpts,
    PutOptions, PutPayload,
};

use crate::error::{Error, Result};
use crate::ltx::{self, FileInfo};
use crate::replica_url::{
    self, bool_query_value, ensure_endpoint_scheme, region_from_s3_arn, ParsedReplicaUrl,
};
use crate::TXID;

use super::ReplicaClient;

/// The replica backend type string, matching `ReplicaClientType` ("s3").
pub const REPLICA_CLIENT_TYPE: &str = "s3";

/// S3 metadata key carrying the LTX-header timestamp (RFC3339Nano), so accurate
/// timestamps survive across restores. Ported from `MetadataKeyTimestamp`
/// (s3/replica_client.go:53).
pub const METADATA_KEY_TIMESTAMP: &str = "litestream-timestamp";

/// Max keys S3 operates on per batch DELETE. `MaxKeys` (s3/replica_client.go:56).
pub const MAX_KEYS: usize = 1000;

/// Region used when none is specified. `DefaultRegion` (s3/replica_client.go:59).
pub const DEFAULT_REGION: &str = "us-east-1";

/// Default parallel `head` calls for timestamp-based restore.
/// `DefaultMetadataConcurrency` (s3/replica_client.go:64).
pub const DEFAULT_METADATA_CONCURRENCY: usize = 50;

/// Default concurrent multipart parts for Cloudflare R2 (strict limits).
/// `DefaultR2Concurrency` (s3/replica_client.go:68).
pub const DEFAULT_R2_CONCURRENCY: usize = 2;

/// Multipart upload threshold: data at or above this size is uploaded with
/// `put_multipart`; below it, a single `put`. Matches the Go uploader's 5 MiB
/// `PartSize` default (s3/replica_client.go:99).
pub const MULTIPART_THRESHOLD: usize = 5 * 1024 * 1024;

// ── Config ────────────────────────────────────────────────────────────────────

/// Configuration for the S3/R2/MinIO backend.
///
/// Maps to the public fields of Go's `ReplicaClient` struct
/// (s3/replica_client.go:78-116). Zero/`None` values mean "use the backend
/// default".
#[derive(Debug, Clone, Default)]
pub struct ObjectStoreConfig {
    /// Bucket name (required).
    pub bucket: String,
    /// Key prefix within the bucket.
    pub path: String,
    /// AWS region.
    pub region: String,
    /// Custom endpoint (MinIO, R2, …); empty = native AWS.
    pub endpoint: String,
    /// Static access key id; empty = ambient credential chain.
    pub access_key_id: String,
    /// Static secret access key; empty = ambient credential chain.
    pub secret_access_key: String,
    /// Force path-style addressing (required for MinIO/Backblaze/Supabase/Filebase).
    pub force_path_style: bool,
    /// Skip TLS verification (allows self-signed endpoints).
    pub skip_verify: bool,
    /// Multipart part size in bytes; 0 = default (5 MiB).
    pub part_size: u64,
    /// Concurrent multipart parts; 0 = default. R2 endpoints default to 2.
    pub concurrency: usize,
}

impl ObjectStoreConfig {
    /// Construct from a parsed `s3://` URL, mirroring `NewReplicaClientFromURL`
    /// (s3/replica_client.go:133-314): host → bucket/region/endpoint/path-style
    /// (or ARN), query-param overrides (camelCase ↔ hyphenated aliases), the
    /// `AWS_*`/`LITESTREAM_*` env credentials, the `LITESTREAM_S3_ENDPOINT` env
    /// fallback, and the provider-specific defaults (R2 concurrency; path-style
    /// for MinIO/Backblaze/Filebase/Supabase).
    pub fn from_url(parsed: &ParsedReplicaUrl) -> Result<Self> {
        let host = &parsed.host;
        let query = &parsed.query;

        // Host → bucket/region/endpoint/forcePathStyle (or ARN access point).
        let (bucket, mut region, mut endpoint, mut force_path_style) = if host.starts_with("arn:") {
            (host.clone(), region_from_s3_arn(host), String::new(), false)
        } else {
            parse_host(host)
        };

        let q = Some(query);

        // endpoint query param: ensure scheme, default to path-style for custom
        // endpoints unless force-path-style is explicitly set to false.
        let q_endpoint = query.get("endpoint");
        if !q_endpoint.is_empty() {
            let (ep, _) = ensure_endpoint_scheme(q_endpoint);
            endpoint = ep;
            match bool_query_value(q, &["forcePathStyle", "force-path-style"]) {
                Some(false) => {}
                _ => force_path_style = true,
            }
        }
        let q_region = query.get("region");
        if !q_region.is_empty() {
            region = q_region.to_string();
        }
        if let Some(v) = bool_query_value(q, &["forcePathStyle", "force-path-style"]) {
            force_path_style = v;
        }
        let mut skip_verify = false;
        if let Some(v) = bool_query_value(q, &["skipVerify", "skip-verify"]) {
            skip_verify = v;
        }

        let mut concurrency: usize = 0;
        let v = query.get("concurrency");
        if !v.is_empty() {
            if let Ok(n) = v.parse::<usize>() {
                if n > 0 {
                    concurrency = n;
                }
            }
        }
        let mut part_size: u64 = 0;
        let v = query.get("partSize");
        let v2 = query.get("part-size");
        if !v.is_empty() {
            if let Ok(n) = v.parse::<u64>() {
                if n > 0 {
                    part_size = n;
                }
            }
        } else if !v2.is_empty() {
            if let Ok(n) = v2.parse::<u64>() {
                if n > 0 {
                    part_size = n;
                }
            }
        }

        if bucket.is_empty() {
            return Err(Error::Other("bucket required for s3 replica URL".into()));
        }

        // Track whether forcePathStyle was explicitly set via query param
        // (s3/replica_client.go:208) — this gates the env-var/provider defaults.
        let force_path_style_set =
            !query.get("forcePathStyle").is_empty() || !query.get("force-path-style").is_empty();

        // Static credentials from env (AWS_* preferred, then LITESTREAM_*).
        let mut access_key_id = String::new();
        let mut secret_access_key = String::new();
        if let Some(v) = nonempty_env("AWS_ACCESS_KEY_ID") {
            access_key_id = v;
        } else if let Some(v) = nonempty_env("LITESTREAM_ACCESS_KEY_ID") {
            access_key_id = v;
        }
        if let Some(v) = nonempty_env("AWS_SECRET_ACCESS_KEY") {
            secret_access_key = v;
        } else if let Some(v) = nonempty_env("LITESTREAM_SECRET_ACCESS_KEY") {
            secret_access_key = v;
        }

        // LITESTREAM_S3_ENDPOINT env fallback (only when no endpoint yet).
        if endpoint.is_empty() {
            if let Some(v) = nonempty_env("LITESTREAM_S3_ENDPOINT") {
                let (ep, _) = ensure_endpoint_scheme(&v);
                endpoint = ep;
                if !force_path_style_set {
                    force_path_style = true;
                }
            }
        }

        // Provider detection for applying defaults.
        let is_filebase = replica_url::is_filebase_endpoint(&endpoint);
        let is_backblaze = replica_url::is_backblaze_endpoint(&endpoint);
        let is_minio = replica_url::is_minio_endpoint(&endpoint);
        let is_supabase = replica_url::is_supabase_endpoint(&endpoint);
        let is_r2 = replica_url::is_cloudflare_r2_endpoint(&endpoint);

        if !force_path_style_set && (is_filebase || is_backblaze || is_minio || is_supabase) {
            force_path_style = true;
        }
        if is_r2 {
            // R2 has strict per-bucket multipart concurrency limits.
            concurrency = DEFAULT_R2_CONCURRENCY;
        }

        Ok(ObjectStoreConfig {
            bucket,
            path: parsed.path.clone(),
            region,
            endpoint,
            access_key_id,
            secret_access_key,
            force_path_style,
            skip_verify,
            part_size,
            concurrency,
        })
    }

    /// Build the configured `AmazonS3` store. Ported from the relevant subset of
    /// `Init` (s3/replica_client.go:322-477): bucket validation, region default,
    /// custom endpoint, path-style toggle, static credentials, and `allow_http`
    /// for plaintext/local endpoints.
    fn build_store(&self) -> Result<Arc<dyn ObjectStore>> {
        if self.bucket.is_empty() {
            return Err(Error::Other("s3: bucket name is required".into()));
        }

        let region = if self.region.is_empty() {
            DEFAULT_REGION.to_string()
        } else {
            self.region.clone()
        };

        let mut builder = AmazonS3Builder::new()
            .with_bucket_name(&self.bucket)
            .with_region(region)
            // Path-style ⇔ NOT virtual-hosted-style (s3/replica_client.go:258-263).
            .with_virtual_hosted_style_request(!self.force_path_style);

        if !self.endpoint.is_empty() {
            // A plaintext (http://) or local endpoint must allow non-TLS, and
            // skip_verify likewise permits http (object_store gates http behind
            // allow_http rather than a TLS-verify toggle).
            let allow_http = self.endpoint.starts_with("http://")
                || self.skip_verify
                || replica_url::is_local_endpoint(&self.endpoint);
            builder = builder
                .with_endpoint(&self.endpoint)
                .with_allow_http(allow_http);
        }

        if !self.access_key_id.is_empty() {
            builder = builder.with_access_key_id(&self.access_key_id);
        }
        if !self.secret_access_key.is_empty() {
            builder = builder.with_secret_access_key(&self.secret_access_key);
        }

        let store = builder
            .build()
            .map_err(|e| Error::Other(format!("s3: build store: {e}").into()))?;
        Ok(Arc::new(store))
    }

    /// Effective multipart part size (`part_size`, or the 5 MiB default).
    fn effective_part_size(&self) -> usize {
        if self.part_size > 0 {
            self.part_size as usize
        } else {
            MULTIPART_THRESHOLD
        }
    }
}

/// Returns `Some(value)` for a non-empty env var, else `None`. Mirrors Go's
/// `if v := os.Getenv(k); v != ""` pattern (s3/replica_client.go:211-224).
fn nonempty_env(key: &str) -> Option<String> {
    match std::env::var(key) {
        Ok(v) if !v.is_empty() => Some(v),
        _ => None,
    }
}

// ── ParseHost ─────────────────────────────────────────────────────────────────

/// Parse an S3 host into `(bucket, region, endpoint, force_path_style)`.
///
/// Direct port of `ParseHost` (s3/replica_client.go:1608-1652): MinIO-style
/// `bucket.host:port`, then the AWS / DigitalOcean / Backblaze / Filebase /
/// Scaleway provider patterns, falling back to "host *is* the bucket".
pub fn parse_host(host: &str) -> (String, String, String, bool) {
    // MinIO-style hosts: `bucket.host:port` (a colon and not a ".com").
    if host.contains(':') && !host.contains(".com") {
        // SplitN(host, ".", 2)
        if let Some((bucket, rest)) = host.split_once('.') {
            return (
                bucket.to_string(),
                DEFAULT_REGION.to_string(),
                format!("http://{rest}"),
                true,
            );
        }
        // No bucket in host, just host:port.
        return (String::new(), String::new(), format!("http://{host}"), true);
    }

    // AWS S3: `^(.+)\.s3(?:\.([^.]+))?\.amazonaws\.com$`
    if let Some((bucket, region)) = match_aws_s3(host) {
        return (bucket, region, String::new(), false);
    }
    // DigitalOcean: `^(?:(.+)\.)?([^.]+)\.digitaloceanspaces.com$`
    if let Some((bucket, region)) = match_two_label_suffix(host, ".digitaloceanspaces.com") {
        return (
            bucket,
            region.clone(),
            format!("https://{region}.digitaloceanspaces.com"),
            false,
        );
    }
    // Backblaze: `^(?:(.+)\.)?s3.([^.]+)\.backblazeb2.com$`
    if let Some((bucket, region)) = match_s3_region_suffix(host, ".backblazeb2.com") {
        return (
            bucket,
            region.clone(),
            format!("https://s3.{region}.backblazeb2.com"),
            true,
        );
    }
    // Filebase: `^(?:(.+)\.)?s3.filebase.com$`
    if let Some(bucket) = match_filebase(host) {
        return (bucket, String::new(), "s3.filebase.com".to_string(), false);
    }
    // Scaleway: `^(?:(.+)\.)?s3.([^.]+)\.scw\.cloud$`
    if let Some((bucket, region)) = match_s3_region_suffix(host, ".scw.cloud") {
        return (
            bucket,
            region.clone(),
            format!("s3.{region}.scw.cloud"),
            false,
        );
    }

    // Standard S3: the host is the bucket name.
    (host.to_string(), String::new(), String::new(), false)
}

/// `^(.+)\.s3(?:\.([^.]+))?\.amazonaws\.com$` → (bucket, region).
fn match_aws_s3(host: &str) -> Option<(String, String)> {
    let rest = host.strip_suffix(".amazonaws.com")?;
    // rest = "<bucket>.s3" or "<bucket>.s3.<region>"
    if let Some(bucket) = rest.strip_suffix(".s3") {
        if bucket.is_empty() {
            return None;
        }
        return Some((bucket.to_string(), String::new()));
    }
    // "<bucket>.s3.<region>": find the ".s3." separator; region is a single
    // label ([^.]+) — i.e. the remainder after ".s3." must contain no dot.
    let idx = rest.find(".s3.")?;
    let bucket = &rest[..idx];
    let region = &rest[idx + 4..];
    if bucket.is_empty() || region.is_empty() || region.contains('.') {
        return None;
    }
    Some((bucket.to_string(), region.to_string()))
}

/// `^(?:(.+)\.)?([^.]+)\.<suffix>$` → (bucket, region). `suffix` starts with '.'.
fn match_two_label_suffix(host: &str, suffix: &str) -> Option<(String, String)> {
    let rest = host.strip_suffix(suffix)?;
    if rest.is_empty() {
        return None;
    }
    // The last label before the suffix is the region; anything before it
    // (optionally) is the bucket.
    match rest.rfind('.') {
        Some(i) => {
            let bucket = &rest[..i];
            let region = &rest[i + 1..];
            if region.is_empty() {
                return None;
            }
            Some((bucket.to_string(), region.to_string()))
        }
        None => Some((String::new(), rest.to_string())),
    }
}

/// `^(?:(.+)\.)?s3.([^.]+)\.<suffix>$` → (bucket, region). `suffix` starts with '.'.
fn match_s3_region_suffix(host: &str, suffix: &str) -> Option<(String, String)> {
    let rest = host.strip_suffix(suffix)?;
    // rest = "[bucket.]s3.<region>"; region is one label ([^.]+).
    // Bucket-less form: rest == "s3.<region>".
    if let Some(region) = rest.strip_prefix("s3.") {
        if region.is_empty() || region.contains('.') {
            return None;
        }
        return Some((String::new(), region.to_string()));
    }
    // Bucketed form: rest == "<bucket>.s3.<region>".
    let sep = rest.find(".s3.")?;
    let bucket = &rest[..sep];
    let region = &rest[sep + 4..];
    if bucket.is_empty() || region.is_empty() || region.contains('.') {
        return None;
    }
    Some((bucket.to_string(), region.to_string()))
}

/// `^(?:(.+)\.)?s3.filebase.com$` → bucket.
fn match_filebase(host: &str) -> Option<String> {
    if host == "s3.filebase.com" {
        return Some(String::new());
    }
    let bucket = host.strip_suffix(".s3.filebase.com")?;
    if bucket.is_empty() {
        None
    } else {
        Some(bucket.to_string())
    }
}

// ── Client ────────────────────────────────────────────────────────────────────

/// Concrete S3/R2/MinIO backend, wrapping a lazily-initialised
/// `Arc<dyn ObjectStore>`.
///
/// Mirrors Go `ReplicaClient` (s3/replica_client.go:78-116). The inner store is
/// created on the first call that needs it (`OnceCell`, mirroring `Init`,
/// s3/replica_client.go:322-477), so construction is infallible and race-free.
pub struct ObjectStoreClient {
    store: tokio::sync::OnceCell<Arc<dyn ObjectStore>>,
    config: ObjectStoreConfig,
}

impl std::fmt::Debug for ObjectStoreClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ObjectStoreClient")
            .field("config", &self.config)
            .field("initialized", &self.store.initialized())
            .finish()
    }
}

impl ObjectStoreClient {
    /// Create a client from config (no I/O; the store is built on first use).
    pub fn new(config: ObjectStoreConfig) -> Self {
        ObjectStoreClient {
            store: tokio::sync::OnceCell::new(),
            config,
        }
    }

    /// Create a client directly from an already-built `ObjectStore` (e.g. an
    /// in-memory store for tests, or a pre-configured backend).
    pub fn with_store(config: ObjectStoreConfig, store: Arc<dyn ObjectStore>) -> Self {
        let cell = tokio::sync::OnceCell::new();
        cell.set(store).ok();
        ObjectStoreClient {
            store: cell,
            config,
        }
    }

    /// The configuration this client was built with.
    pub fn config(&self) -> &ObjectStoreConfig {
        &self.config
    }

    /// Force early initialisation; idempotent (mirrors `Init`,
    /// s3/replica_client.go:322-477).
    pub async fn init(&self) -> Result<()> {
        self.store().await.map(|_| ())
    }

    /// Get-or-build the inner store, once.
    async fn store(&self) -> Result<&Arc<dyn ObjectStore>> {
        self.store
            .get_or_try_init(|| async { self.config.build_store() })
            .await
    }

    /// Build the S3 key for an LTX file: `{path}/{level:04x}/{min}-{max}.ltx`.
    /// Ported from s3/replica_client.go:629, 677, 1040-1042.
    fn ltx_key(&self, level: i32, min_txid: TXID, max_txid: TXID) -> String {
        let filename = ltx::format_filename(min_txid, max_txid);
        format!("{}/{:04x}/{}", self.config.path, level, filename)
    }

    /// Prefix for listing a level: `{path}/{level:04x}/`.
    /// Ported from s3/replica_client.go:1363.
    fn level_prefix(&self, level: i32) -> String {
        format!("{}/{:04x}/", self.config.path, level)
    }

    /// Root prefix for delete-all: `{path}/`. (s3/replica_client.go:1114).
    fn root_prefix(&self) -> String {
        format!("{}/", self.config.path)
    }
}

/// Map an `object_store::Error` to `crate::Error`, preserving NotFound as
/// `io::ErrorKind::NotFound` so callers keep working with the std error kind.
/// Mirrors `isNotExists` → `os.ErrNotExist` (s3/replica_client.go:647-649,
/// 1662-1668).
fn map_os_error(e: object_store::Error) -> Error {
    match e {
        object_store::Error::NotFound { .. } => {
            Error::Io(std::io::Error::new(std::io::ErrorKind::NotFound, e))
        }
        other => Error::Other(Box::new(other)),
    }
}

#[async_trait::async_trait]
impl ReplicaClient for ObjectStoreClient {
    fn type_name(&self) -> &str {
        REPLICA_CLIENT_TYPE
    }

    async fn init(&self) -> Result<()> {
        ObjectStoreClient::init(self).await
    }

    async fn ltx_files(&self, level: i32, seek: TXID, use_metadata: bool) -> Result<Vec<FileInfo>> {
        let store = self.store().await?;
        let prefix = ObjPath::from(self.level_prefix(level));

        // List everything under the level prefix. object_store yields keys in
        // lexicographic order; since TXID hex is zero-padded to 16 digits,
        // lexicographic == numeric ascending (brief §5.10). We still sort
        // defensively after collecting, because the listing of an L0 level is
        // bounded and small.
        let metas: Vec<object_store::ObjectMeta> = store
            .list(Some(&prefix))
            .map_err(map_os_error)
            .try_collect()
            .await?;

        // Parse filenames, applying the seek-skip filter (min_txid < seek).
        // Done as a post-parse filter, NOT via a list prefix, because S3 prefix
        // listing is lexicographic on the full key (brief §5.4).
        let mut entries: Vec<(object_store::ObjectMeta, TXID, TXID)> =
            Vec::with_capacity(metas.len());
        for meta in metas {
            let name = meta.location.filename().unwrap_or("");
            let (min_txid, max_txid) = match ltx::parse_filename(name) {
                Ok(t) => t,
                Err(_) => continue, // skip non-LTX keys
            };
            if min_txid < seek {
                continue;
            }
            entries.push((meta, min_txid, max_txid));
        }

        // Timestamp source: when use_metadata is requested, read the
        // litestream-timestamp attribute back via a parallel `head` fan-out
        // (concurrency DEFAULT_METADATA_CONCURRENCY); otherwise fall back to the
        // listing's last_modified. Ported from s3/replica_client.go:1384-1455,
        // 1543-1553.
        let mut header_ts: std::collections::HashMap<String, std::time::SystemTime> =
            std::collections::HashMap::new();
        if use_metadata && !entries.is_empty() {
            let keys: Vec<String> = entries
                .iter()
                .map(|(m, _, _)| m.location.to_string())
                .collect();
            header_ts = fetch_timestamp_metadata(store.as_ref(), &keys).await;
        }

        let mut infos: Vec<FileInfo> = entries
            .into_iter()
            .map(|(meta, min_txid, max_txid)| {
                let key = meta.location.to_string();
                let created_at = if use_metadata {
                    header_ts
                        .get(&key)
                        .copied()
                        .or_else(|| Some(std::time::SystemTime::from(meta.last_modified)))
                } else {
                    Some(std::time::SystemTime::from(meta.last_modified))
                };
                FileInfo {
                    level,
                    min_txid,
                    max_txid,
                    size: meta.size as i64,
                    created_at,
                    ..Default::default()
                }
            })
            .collect();

        // Iterator contract: ascending by (level, min_txid, max_txid).
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
        let store = self.store().await?;
        let key = ObjPath::from(self.ltx_key(level, min_txid, max_txid));

        // Range: bytes=offset-(offset+size-1) when size>0, else bytes=offset-
        // (s3/replica_client.go:620-625).
        let off = offset.max(0) as usize;
        let range = if size > 0 {
            GetRange::Bounded(off..(off + size as usize))
        } else {
            GetRange::Offset(off)
        };

        let opts = GetOptions {
            range: Some(range),
            ..Default::default()
        };

        let result = match store.get_opts(&key, opts).await {
            Ok(r) => r,
            Err(object_store::Error::NotFound { .. }) => {
                return Err(Error::Io(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    format!("s3: get object {key}: not found"),
                )));
            }
            // DECISION: an `offset >= object_size` range yields HTTP 416 from
            // S3/MinIO (object_store surfaces it as a Generic "Range Not
            // Satisfiable" error). The Go client would propagate that 416 as an
            // error; our file client (T6) instead returns an empty slice when
            // `offset >= len`. We mirror the *file client* here so both backends
            // are interchangeable under `run_client_suite` (the T7 DoD). No real
            // read path requests a past-EOF offset (restore reads the in-bounds
            // page-index tail), so this only affects the degenerate case, and a
            // genuine read error (anything not 416) still propagates below.
            // Logged in OPEN_QUESTIONS.md.
            Err(e) if is_range_not_satisfiable(&e) => return Ok(Vec::new()),
            Err(e) => return Err(map_os_error(e)),
        };

        let bytes = result.bytes().await.map_err(map_os_error)?;
        Ok(bytes.to_vec())
    }

    async fn write_ltx_file(
        &self,
        level: i32,
        min_txid: TXID,
        max_txid: TXID,
        data: &[u8],
    ) -> Result<FileInfo> {
        let store = self.store().await?;

        // Peek the LTX header timestamp (preserved as litestream-timestamp).
        // Ported from s3/replica_client.go:662-683.
        let header = ltx::Header::parse(data)?;
        let created_at = std::time::UNIX_EPOCH
            + std::time::Duration::from_millis(header.timestamp.max(0) as u64);
        let ts_rfc3339 = format_rfc3339_nano(header.timestamp);

        let mut attributes = Attributes::new();
        attributes.insert(
            Attribute::Metadata(METADATA_KEY_TIMESTAMP.into()),
            AttributeValue::from(ts_rfc3339),
        );

        let key = ObjPath::from(self.ltx_key(level, min_txid, max_txid));

        // Multipart threshold: < 5 MiB → single PUT; ≥ 5 MiB → multipart with
        // fixed-size parts. Ported from the Go uploader's 5 MiB PartSize default
        // (s3/replica_client.go:99, brief §5.1).
        let part_size = self.config.effective_part_size();
        if data.len() < MULTIPART_THRESHOLD {
            let payload = PutPayload::from(data.to_vec());
            let opts = PutOptions {
                attributes,
                ..Default::default()
            };
            store
                .put_opts(&key, payload, opts)
                .await
                .map_err(|e| Error::Other(format!("s3: upload to {key}: {e}").into()))?;
        } else {
            let opts = PutMultipartOpts {
                attributes,
                ..Default::default()
            };
            let mut upload = store
                .put_multipart_opts(&key, opts)
                .await
                .map_err(|e| Error::Other(format!("s3: upload to {key}: {e}").into()))?;
            // Upload in fixed-size parts (each ≥ 5 MiB except possibly the last,
            // matching object_store's part-size requirement).
            for chunk in data.chunks(part_size.max(MULTIPART_THRESHOLD)) {
                upload
                    .put_part(PutPayload::from(chunk.to_vec()))
                    .await
                    .map_err(|e| Error::Other(format!("s3: upload part to {key}: {e}").into()))?;
            }
            upload
                .complete()
                .await
                .map_err(|e| Error::Other(format!("s3: complete upload to {key}: {e}").into()))?;
        }

        Ok(FileInfo {
            level,
            min_txid,
            max_txid,
            size: data.len() as i64,
            created_at: Some(created_at),
            ..Default::default()
        })
    }

    async fn delete_ltx_files(&self, files: &[FileInfo]) -> Result<()> {
        if files.is_empty() {
            return Ok(());
        }
        let store = self.store().await?;

        // Build the key list, then delete in batches of MAX_KEYS via the
        // store's delete_stream, surfacing per-key errors (brief §5.5).
        let keys: Vec<ObjPath> = files
            .iter()
            .map(|info| ObjPath::from(self.ltx_key(info.level, info.min_txid, info.max_txid)))
            .collect();

        for batch in keys.chunks(MAX_KEYS) {
            delete_batch(store.as_ref(), batch, /*ignore_missing=*/ true).await?;
        }
        Ok(())
    }

    async fn delete_all(&self) -> Result<()> {
        let store = self.store().await?;
        let prefix = ObjPath::from(self.root_prefix());

        // List everything under the path prefix, then batch-delete.
        // (s3/replica_client.go:1104-1148).
        let keys: Vec<ObjPath> = store
            .list(Some(&prefix))
            .map_ok(|m| m.location)
            .map_err(map_os_error)
            .try_collect()
            .await?;

        for batch in keys.chunks(MAX_KEYS) {
            delete_batch(store.as_ref(), batch, /*ignore_missing=*/ true).await?;
        }
        Ok(())
    }
}

/// Returns `true` if the error is S3's "range not satisfiable" (HTTP 416),
/// which object_store surfaces as a `Generic`/`NotModified`-shaped error when a
/// requested range begins at or after the object's end.
fn is_range_not_satisfiable(e: &object_store::Error) -> bool {
    // object_store does not expose a dedicated variant; match on the rendered
    // message, which includes the upstream status. This only affects the
    // offset-past-EOF parity case; a real read error still propagates.
    let msg = e.to_string();
    msg.contains("Range Not Satisfiable") || msg.contains("416")
}

/// Delete a batch of keys via `delete_stream`, surfacing per-key errors.
///
/// When `ignore_missing` is set, `NotFound` is tolerated (delete is idempotent —
/// the file client swallows ENOENT the same way), but every other per-key error
/// is returned (brief §5.5: do not silently swallow partial failures).
async fn delete_batch(
    store: &dyn ObjectStore,
    keys: &[ObjPath],
    ignore_missing: bool,
) -> Result<()> {
    if keys.is_empty() {
        return Ok(());
    }
    let owned: Vec<ObjPath> = keys.to_vec();
    let stream = futures::stream::iter(owned.into_iter().map(Ok));
    let mut results = store.delete_stream(stream.boxed());
    while let Some(res) = results.next().await {
        match res {
            Ok(_) => {}
            Err(object_store::Error::NotFound { .. }) if ignore_missing => {}
            Err(e) => return Err(map_os_error(e)),
        }
    }
    Ok(())
}

/// Fetch the `litestream-timestamp` header timestamp for each key via a parallel
/// `head` fan-out, bounded to `DEFAULT_METADATA_CONCURRENCY` in-flight requests.
///
/// A missing/unparseable attribute or a failed `head` is non-fatal (the key is
/// simply absent from the map, and the caller falls back to `last_modified`),
/// matching the Go behavior (s3/replica_client.go:1427-1442).
async fn fetch_timestamp_metadata(
    store: &dyn ObjectStore,
    keys: &[String],
) -> std::collections::HashMap<String, std::time::SystemTime> {
    use std::collections::HashMap;
    let mut out: HashMap<String, std::time::SystemTime> = HashMap::new();
    let mut in_flight = FuturesUnordered::new();
    let mut iter = keys.iter();

    // Prime the pump up to the concurrency limit.
    for _ in 0..DEFAULT_METADATA_CONCURRENCY {
        match iter.next() {
            Some(k) => in_flight.push(head_timestamp(store, k.clone())),
            None => break,
        }
    }

    while let Some((key, ts)) = in_flight.next().await {
        if let Some(ts) = ts {
            out.insert(key, ts);
        }
        if let Some(k) = iter.next() {
            in_flight.push(head_timestamp(store, k.clone()));
        }
    }
    out
}

/// `head` one key and parse its `litestream-timestamp` attribute, if present.
async fn head_timestamp(
    store: &dyn ObjectStore,
    key: String,
) -> (String, Option<std::time::SystemTime>) {
    let path = ObjPath::from(key.clone());
    let opts = GetOptions {
        head: true,
        ..Default::default()
    };
    match store.get_opts(&path, opts).await {
        Ok(result) => {
            let attr = result
                .attributes
                .get(&Attribute::Metadata(METADATA_KEY_TIMESTAMP.into()))
                .map(|v| v.as_ref().to_string());
            let ts = attr.as_deref().and_then(parse_rfc3339_nano);
            (key, ts)
        }
        // Non-fatal: fall back to LastModified for this key.
        Err(_) => (key, None),
    }
}

// ── RFC3339Nano timestamp (de)serialisation ──────────────────────────────────
//
// Go stores `time.UnixMilli(hdr.Timestamp).UTC().Format(time.RFC3339Nano)` in
// the metadata (s3/replica_client.go:671-681) and parses it back with
// `time.Parse(time.RFC3339Nano, ts)` (s3/replica_client.go:1435). We don't pull
// in a date crate (AGENTS.md rule 7); these two helpers round-trip the exact
// shape we write — `YYYY-MM-DDTHH:MM:SS[.fffffffff]Z` — which is all the
// `use_metadata` restore path needs, since it only ever reads timestamps that
// this same client wrote.

/// Format `unix_millis` as an RFC3339Nano UTC string (`…Z`, fractional seconds
/// trimmed of trailing zeros, matching Go's `time.RFC3339Nano`).
fn format_rfc3339_nano(unix_millis: i64) -> String {
    // Decompose into whole seconds + millisecond remainder (>= 0).
    let millis = unix_millis.max(0);
    let secs = millis / 1000;
    let ms = (millis % 1000) as u32;

    let (year, month, day, hour, min, sec) = civil_from_unix_secs(secs);
    if ms == 0 {
        format!("{year:04}-{month:02}-{day:02}T{hour:02}:{min:02}:{sec:02}Z")
    } else {
        // RFC3339Nano trims trailing zeros; milliseconds → up to 3 digits.
        let mut frac = format!("{ms:03}");
        while frac.ends_with('0') {
            frac.pop();
        }
        format!("{year:04}-{month:02}-{day:02}T{hour:02}:{min:02}:{sec:02}.{frac}Z")
    }
}

/// Parse an RFC3339Nano UTC string (as produced by [`format_rfc3339_nano`])
/// back into a `SystemTime`. Returns `None` on any shape we don't recognise.
fn parse_rfc3339_nano(s: &str) -> Option<std::time::SystemTime> {
    // Expect: YYYY-MM-DDTHH:MM:SS[.fraction]Z
    let s = s.strip_suffix('Z')?;
    let (date, time) = s.split_once('T')?;
    let mut dparts = date.split('-');
    let year: i64 = dparts.next()?.parse().ok()?;
    let month: u32 = dparts.next()?.parse().ok()?;
    let day: u32 = dparts.next()?.parse().ok()?;
    if dparts.next().is_some() {
        return None;
    }

    let (hms, frac) = match time.split_once('.') {
        Some((hms, frac)) => (hms, Some(frac)),
        None => (time, None),
    };
    let mut tparts = hms.split(':');
    let hour: u32 = tparts.next()?.parse().ok()?;
    let min: u32 = tparts.next()?.parse().ok()?;
    let sec: u32 = tparts.next()?.parse().ok()?;
    if tparts.next().is_some() {
        return None;
    }

    let secs = unix_secs_from_civil(year, month, day, hour, min, sec)?;
    let nanos: u32 = match frac {
        Some(frac) => {
            if frac.is_empty() || frac.len() > 9 || !frac.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            let mut padded = frac.to_string();
            while padded.len() < 9 {
                padded.push('0');
            }
            padded.parse().ok()?
        }
        None => 0,
    };

    if secs < 0 {
        return None;
    }
    Some(std::time::UNIX_EPOCH + std::time::Duration::new(secs as u64, nanos))
}

/// Convert a Unix timestamp (whole seconds, >= 0) to a civil
/// `(year, month, day, hour, min, sec)` in UTC. Uses Howard Hinnant's
/// days-from-civil inverse (public-domain algorithm), exact for all dates.
fn civil_from_unix_secs(secs: i64) -> (i64, u32, u32, u32, u32, u32) {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let hour = (rem / 3600) as u32;
    let min = ((rem % 3600) / 60) as u32;
    let sec = (rem % 60) as u32;

    // civil_from_days (days since 1970-01-01).
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    let year = if m <= 2 { y + 1 } else { y };
    (year, m, d, hour, min, sec)
}

/// Inverse of [`civil_from_unix_secs`]: civil UTC → Unix seconds. Returns `None`
/// for an out-of-range month/day. Uses Howard Hinnant's days_from_civil.
fn unix_secs_from_civil(
    year: i64,
    month: u32,
    day: u32,
    hour: u32,
    min: u32,
    sec: u32,
) -> Option<i64> {
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) || hour > 23 || min > 59 || sec > 60 {
        return None;
    }
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400; // [0, 399]
    let m = month as i64;
    let d = day as i64;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    let days = era * 146_097 + doe - 719_468;
    Some(days * 86_400 + hour as i64 * 3600 + min as i64 * 60 + sec as i64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::replica_url::parse_replica_url_with_query;

    // ── ParseHost (port of TestParseHost, s3/replica_client_test.go:1071) ──────
    #[test]
    fn parse_host_table() {
        let cases: &[(&str, &str, &str, &str, bool)] = &[
            (
                "my-space.sgp1.digitaloceanspaces.com",
                "my-space",
                "sgp1",
                "https://sgp1.digitaloceanspaces.com",
                false,
            ),
            (
                "test-bucket.nyc3.digitaloceanspaces.com",
                "test-bucket",
                "nyc3",
                "https://nyc3.digitaloceanspaces.com",
                false,
            ),
            (
                "mybucket.s3.us-east-1.amazonaws.com",
                "mybucket",
                "us-east-1",
                "",
                false,
            ),
            ("mybucket.s3.amazonaws.com", "mybucket", "", "", false),
            (
                "mybucket.s3.us-west-004.backblazeb2.com",
                "mybucket",
                "us-west-004",
                "https://s3.us-west-004.backblazeb2.com",
                true,
            ),
            (
                "mybucket.localhost:9000",
                "mybucket",
                "us-east-1",
                "http://localhost:9000",
                true,
            ),
        ];
        for (host, b, r, e, fps) in cases {
            let (bucket, region, endpoint, force) = parse_host(host);
            assert_eq!(&bucket, b, "bucket for {host}");
            assert_eq!(&region, r, "region for {host}");
            assert_eq!(&endpoint, e, "endpoint for {host}");
            assert_eq!(force, *fps, "force_path_style for {host}");
        }
    }

    #[test]
    fn parse_host_standard_s3_is_bucket() {
        let (bucket, region, endpoint, force) = parse_host("mybucket");
        assert_eq!(bucket, "mybucket");
        assert_eq!(region, "");
        assert_eq!(endpoint, "");
        assert!(!force);
    }

    fn cfg_from_url(url: &str) -> ObjectStoreConfig {
        let parsed = parse_replica_url_with_query(url).unwrap();
        ObjectStoreConfig::from_url(&parsed).unwrap()
    }

    // ── R2 concurrency default (port of TestReplicaClient_R2ConcurrencyDefault,
    //    s3/replica_client_test.go:1795) ─────────────────────────────────────────
    #[test]
    fn r2_concurrency_default() {
        assert_eq!(
            cfg_from_url("s3://mybucket/path?endpoint=https://account123.r2.cloudflarestorage.com")
                .concurrency,
            2,
            "R2 endpoint defaults concurrency to 2"
        );
        assert_eq!(
            cfg_from_url("s3://mybucket/path").concurrency,
            0,
            "AWS: no concurrency override"
        );
        assert_eq!(
            cfg_from_url("s3://mybucket/path?endpoint=http://localhost:9000").concurrency,
            0,
            "MinIO: no concurrency override"
        );
    }

    // ── URL query param aliases (port of
    //    TestNewReplicaClientFromURL_QueryParamAliases, test:1940) ───────────────
    #[test]
    fn query_param_aliases() {
        let c = cfg_from_url("s3://mybucket/path?forcePathStyle=true");
        assert!(c.force_path_style);

        let c = cfg_from_url("s3://mybucket/path?force-path-style=true");
        assert!(c.force_path_style);

        let c = cfg_from_url(
            "s3://mybucket/path?endpoint=http://localhost:9000&force-path-style=false",
        );
        assert!(!c.force_path_style, "explicit force-path-style=false wins");

        let c = cfg_from_url("s3://mybucket/path?skipVerify=true");
        assert!(c.skip_verify);
        let c = cfg_from_url("s3://mybucket/path?skip-verify=true");
        assert!(c.skip_verify);

        let c = cfg_from_url("s3://mybucket/path?concurrency=3");
        assert_eq!(c.concurrency, 3);

        let c = cfg_from_url("s3://mybucket/path?part-size=10485760");
        assert_eq!(c.part_size, 10_485_760);
        let c = cfg_from_url("s3://mybucket/path?partSize=10485760");
        assert_eq!(c.part_size, 10_485_760);

        let c = cfg_from_url(
            "s3://mybucket/path?force-path-style=true&skip-verify=true&concurrency=4&part-size=8388608",
        );
        assert!(c.force_path_style);
        assert!(c.skip_verify);
        assert_eq!(c.concurrency, 4);
        assert_eq!(c.part_size, 8_388_608);
    }

    // ── Endpoint env var (port of TestNewReplicaClientFromURL_EndpointEnvVar,
    //    test:2023). These mutate a process-global env var, so they run
    //    sequentially under one #[test] with save/restore to avoid cross-test
    //    interference. ───────────────────────────────────────────────────────────
    #[test]
    fn endpoint_env_var() {
        let saved = std::env::var("LITESTREAM_S3_ENDPOINT").ok();

        let set = |v: &str| {
            if v.is_empty() {
                std::env::remove_var("LITESTREAM_S3_ENDPOINT");
            } else {
                std::env::set_var("LITESTREAM_S3_ENDPOINT", v);
            }
        };

        set("http://localhost:9000");
        let c = cfg_from_url("s3://mybucket/path");
        assert_eq!(c.endpoint, "http://localhost:9000");
        assert!(c.force_path_style, "env endpoint forces path-style");

        set("s3.example.com");
        let c = cfg_from_url("s3://mybucket/path");
        assert_eq!(
            c.endpoint, "https://s3.example.com",
            "env endpoint gets https"
        );
        assert!(c.force_path_style);

        set("http://localhost:9000");
        let c = cfg_from_url("s3://mybucket/path?endpoint=http://other:9000");
        assert_eq!(
            c.endpoint, "http://other:9000",
            "query endpoint overrides env"
        );
        assert!(c.force_path_style);

        set("http://localhost:9000");
        let c = cfg_from_url("s3://mybucket/path?force-path-style=false");
        assert_eq!(c.endpoint, "http://localhost:9000");
        assert!(
            !c.force_path_style,
            "explicit force-path-style=false respected with env endpoint"
        );

        set("");
        let c = cfg_from_url("s3://mybucket/path");
        assert_eq!(c.endpoint, "");
        assert!(!c.force_path_style);

        // Restore.
        match saved {
            Some(v) => std::env::set_var("LITESTREAM_S3_ENDPOINT", v),
            None => std::env::remove_var("LITESTREAM_S3_ENDPOINT"),
        }
    }

    // ── Bucket validation (port of TestReplicaClient_Init_BucketValidation,
    //    test:468). Empty bucket → build error. ────────────────────────────────
    #[tokio::test]
    async fn empty_bucket_errors_on_init() {
        let client = ObjectStoreClient::new(ObjectStoreConfig::default());
        assert!(client.init().await.is_err(), "empty bucket must error");
    }

    // ── Key construction (wire-compat requirement D-1, test:629/677/1040) ─────
    #[test]
    fn ltx_key_scheme() {
        let client = ObjectStoreClient::new(ObjectStoreConfig {
            bucket: "b".into(),
            path: "replica".into(),
            ..Default::default()
        });
        assert_eq!(
            client.ltx_key(0, TXID(1), TXID(1)),
            "replica/0000/0000000000000001-0000000000000001.ltx"
        );
        assert_eq!(
            client.ltx_key(0, TXID(1), TXID(6)),
            "replica/0000/0000000000000001-0000000000000006.ltx"
        );
        assert_eq!(client.level_prefix(0), "replica/0000/");
        assert_eq!(client.root_prefix(), "replica/");
    }

    // ── isNotExists mapping (port of TestIsNotExists, test:53) ────────────────
    #[test]
    fn not_found_maps_to_io_not_found() {
        let e = map_os_error(object_store::Error::NotFound {
            path: "k".into(),
            source: "missing".into(),
        });
        match e {
            Error::Io(io) => assert_eq!(io.kind(), std::io::ErrorKind::NotFound),
            other => panic!("expected Io(NotFound), got {other:?}"),
        }
    }

    // ── RFC3339Nano round-trip (the timestamp metadata path, test:671/1435) ───
    #[test]
    fn rfc3339_nano_round_trip() {
        // 2021-01-01T00:00:00Z = 1609459200000 ms.
        assert_eq!(
            format_rfc3339_nano(1_609_459_200_000),
            "2021-01-01T00:00:00Z"
        );
        // With a millisecond fraction (trailing zeros trimmed).
        assert_eq!(
            format_rfc3339_nano(1_609_459_200_500),
            "2021-01-01T00:00:00.5Z"
        );
        assert_eq!(format_rfc3339_nano(0), "1970-01-01T00:00:00Z");

        for ms in [
            0i64,
            1_000,
            1_609_459_200_000,
            1_609_459_200_123,
            1_700_000_000_777,
        ] {
            let s = format_rfc3339_nano(ms);
            let back = parse_rfc3339_nano(&s).expect("parse our own format");
            let got = back
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis() as i64;
            assert_eq!(got, ms, "round trip for {ms} via {s:?}");
        }

        // Reject malformed inputs.
        assert!(parse_rfc3339_nano("not-a-date").is_none());
        assert!(parse_rfc3339_nano("2021-01-01T00:00:00").is_none()); // missing Z
        assert!(parse_rfc3339_nano("2021-13-01T00:00:00Z").is_none()); // bad month
    }

    #[test]
    fn type_name_is_s3() {
        let client = ObjectStoreClient::new(ObjectStoreConfig {
            bucket: "b".into(),
            ..Default::default()
        });
        assert_eq!(client.type_name(), "s3");
    }
}
