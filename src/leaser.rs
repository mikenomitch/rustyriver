//! leaser.rs — object-storage lease fencing: acquire / renew / release + the
//! expiry→failover protocol, built on a compare-and-swap (conditional write)
//! primitive over ordinary object storage.
//!
//! Ported from litestream@v0.5.11:
//!   - `leaser.go:1-44` — the `Leaser` interface, the `Lease` value type, and the `ErrLeaseNotHeld` / `LeaseExistsError` sentinels.
//!   - `heartbeat.go:1-84` — the `HeartbeatClient` liveness-ping companion.
//!   - `s3/leaser.go:1-306` — the concrete S3 implementation (`readLease`, `writeLease`, `AcquireLease`, `RenewLease`, `ReleaseLease`, `lockKey`).
//!
//! # Fencing model
//!
//! A node must hold a live lease before writing LTX data to the remote store. The
//! lease is a tiny JSON object (`lock.json`) under the replica prefix. All
//! acquire/renew/release operations are guarded by HTTP conditional writes —
//! `If-None-Match: *` for creation, `If-Match: <etag>` for renewal — which give
//! compare-and-swap semantics over plain object storage with no extra primitives.
//! If a node cannot acquire or renew, it stands by (read-only) until the current
//! lease expires and the key becomes writable again.
//!
//! # `object_store` mapping (see the porting brief T15.md §2/§3/§5)
//!
//! | Go (aws-sdk + smithy)        | Rust (`object_store` 0.11)                       |
//! |------------------------------|--------------------------------------------------|
//! | `PutObject{IfNoneMatch:"*"}` | `PutMode::Create` → `Error::AlreadyExists` on loss |
//! | `PutObject{IfMatch:etag}`    | `PutMode::Update(UpdateVersion{e_tag})` → `Error::Precondition` |
//! | `GetObject`                  | `get` → `GetResult { meta.e_tag, .. }`           |
//! | `os.ErrNotExist`             | `Error::NotFound`                                |
//! | `PreconditionFailed` (412)   | `Error::Precondition` / `Error::AlreadyExists`   |
//!
//! # DEVIATION from Go — conditional DELETE
//!
//! Go's `ReleaseLease` issues `DeleteObject` with `IfMatch:<etag>` (s3/leaser.go:180-184)
//! so the delete is atomic. `object_store 0.11`'s `delete(&path)` takes **no**
//! options and has no conditional-delete variant (lib.rs:660). We therefore
//! implement release as **read-then-delete**: read the current lock object (and
//! its ETag), compare it to the held lease's ETag, and only then delete. The three
//! Go outcomes are preserved exactly:
//!
//!   - key absent → `LeaseAlreadyReleased` (Go's 404 path, s3/leaser.go:186-188).
//!   - ETag mismatch → `LeaseNotHeld` (Go's 412 path, s3/leaser.go:189-191).
//!   - ETag matches → unconditional delete; a NotFound during the delete is treated as success (the object is already gone).
//!
//! This opens a ~1-RTT TOCTOU window between the read and the delete, but it is
//! benign under the single-owner-handle model (brief §5.11): the lock object can
//! only move off our ETag if *we* renew it, and a node about to release is not
//! concurrently renewing the same handle. No other node can mutate the object from
//! our ETag because ETags are unique per write and only we hold ours. `// DECISION`
//! logged in OPEN_QUESTIONS.md.

use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

// ── Constants (s3/leaser.go:21-25, heartbeat.go:11-15) ────────────────────────

/// Default lease time-to-live. Ported from `DefaultLeaseTTL` (s3/leaser.go:22).
pub const DEFAULT_LEASE_TTL: Duration = Duration::from_secs(30);

/// Lock-object filename appended to the replica prefix.
/// Ported from `DefaultLeasePath` (s3/leaser.go:23).
pub const DEFAULT_LEASE_PATH: &str = "lock.json";

/// Backend type tag for the S3 leaser. Ported from `LeaserType` (s3/leaser.go:24).
pub const LEASER_TYPE: &str = "s3";

/// Default heartbeat interval. Ported from `DefaultHeartbeatInterval` (heartbeat.go:12).
pub const DEFAULT_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5 * 60);

/// Default heartbeat HTTP timeout. Ported from `DefaultHeartbeatTimeout` (heartbeat.go:13).
pub const DEFAULT_HEARTBEAT_TIMEOUT: Duration = Duration::from_secs(30);

/// Floor on the heartbeat interval, enforced by the constructor regardless of the
/// caller's value. Ported from `MinHeartbeatInterval` (heartbeat.go:14).
pub const MIN_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(60);

// ── Sentinel errors (leaser.go:10-22, s3/leaser.go:30-33) ─────────────────────

/// The caller no longer holds the lease — a concurrent writer took over, or the
/// conditional write failed on an ETag mismatch.
///
/// Maps to `litestream.ErrLeaseNotHeld` (leaser.go:10).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LeaseNotHeld;

impl std::fmt::Display for LeaseNotHeld {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("lease not held")
    }
}

impl std::error::Error for LeaseNotHeld {}

/// A lease was supplied with no ETag, so it cannot be renewed or released.
///
/// Maps to `s3.ErrLeaseETagRequired` (s3/leaser.go:31).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LeaseETagRequired;

impl std::fmt::Display for LeaseETagRequired {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("lease etag required")
    }
}

impl std::error::Error for LeaseETagRequired {}

/// The lock object was already gone when release attempted to delete it.
///
/// Maps to `s3.ErrLeaseAlreadyReleased` (s3/leaser.go:32).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LeaseAlreadyReleased;

impl std::fmt::Display for LeaseAlreadyReleased {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("lease already released")
    }
}

impl std::error::Error for LeaseAlreadyReleased {}

/// Another node currently holds a live, unexpired lease.
///
/// Maps to `litestream.LeaseExistsError` (leaser.go:12-22). The `Display` form is
/// a byte-for-byte port of the Go `Error()` method (leaser.go:17-21), including
/// the RFC-3339 rendering of `expires_at` and the with/without-owner branch.
#[derive(Debug, Clone)]
pub struct LeaseExistsError {
    /// Identity string of the current holder; may be empty.
    pub owner: String,
    /// Absolute wall-clock expiry of the current holder's lease.
    pub expires_at: SystemTime,
}

impl std::fmt::Display for LeaseExistsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Go: if e.Owner != "" { "lease already held by %s until %s" }
        //     else            { "lease already held until %s" }
        // with %s == e.ExpiresAt.Format(time.RFC3339).
        let until = rfc3339::format(self.expires_at);
        if self.owner.is_empty() {
            write!(f, "lease already held until {until}")
        } else {
            write!(f, "lease already held by {} until {until}", self.owner)
        }
    }
}

impl std::error::Error for LeaseExistsError {}

// Allow `?` to lift the concrete sentinels into the crate-wide `Error` via the
// `Other` boxed-error variant. This mirrors Go returning the bare sentinel values
// from the `Leaser` interface methods; callers recover the concrete type with the
// `Error::is_lease_*` / `Error::as_lease_exists` helpers below (the analogue of
// Go's `errors.Is` / `errors.As`).
impl From<LeaseNotHeld> for Error {
    fn from(e: LeaseNotHeld) -> Self {
        Error::Other(Box::new(e))
    }
}
impl From<LeaseETagRequired> for Error {
    fn from(e: LeaseETagRequired) -> Self {
        Error::Other(Box::new(e))
    }
}
impl From<LeaseAlreadyReleased> for Error {
    fn from(e: LeaseAlreadyReleased) -> Self {
        Error::Other(Box::new(e))
    }
}
impl From<LeaseExistsError> for Error {
    fn from(e: LeaseExistsError) -> Self {
        Error::Other(Box::new(e))
    }
}

impl Error {
    /// Returns `true` when this error is (or wraps) [`LeaseNotHeld`].
    ///
    /// The analogue of Go's `errors.Is(err, ErrLeaseNotHeld)` (s3/leaser.go:156).
    pub fn is_lease_not_held(&self) -> bool {
        matches!(self, Error::Other(b) if b.downcast_ref::<LeaseNotHeld>().is_some())
    }

    /// Returns `true` when this error is (or wraps) [`LeaseAlreadyReleased`].
    ///
    /// The analogue of `errors.Is(err, ErrLeaseAlreadyReleased)` (s3/leaser.go:187).
    pub fn is_lease_already_released(&self) -> bool {
        matches!(self, Error::Other(b) if b.downcast_ref::<LeaseAlreadyReleased>().is_some())
    }

    /// Returns `true` when this error is (or wraps) [`LeaseETagRequired`].
    pub fn is_lease_etag_required(&self) -> bool {
        matches!(self, Error::Other(b) if b.downcast_ref::<LeaseETagRequired>().is_some())
    }

    /// Borrows the inner [`LeaseExistsError`] if this error is one.
    ///
    /// The analogue of Go's `errors.As(err, &leaseErr)` (s3/leaser.go:117, 154).
    pub fn as_lease_exists(&self) -> Option<&LeaseExistsError> {
        match self {
            Error::Other(b) => b.downcast_ref::<LeaseExistsError>(),
            _ => None,
        }
    }
}

// ── Lease value type (leaser.go:31-44) ────────────────────────────────────────

/// A snapshot of a held lease.
///
/// Maps to `litestream.Lease` (leaser.go:31-36). The JSON shape matches Go's
/// struct tags exactly: `generation` (i64), `expires_at` (RFC-3339 string),
/// `owner` (omitted when empty), and `e_tag` is **not** serialised (`json:"-"`).
///
/// `expires_at` is a wall-clock [`SystemTime`] (not a monotonic `Instant`) because
/// it is serialised into the lock object and compared *across nodes* whose clocks
/// may differ (brief §5.7).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Lease {
    /// Monotonically increasing acquisition counter (split-brain detector).
    pub generation: i64,
    /// Absolute wall-clock expiry, serialised as RFC-3339 (Go `time.RFC3339Nano`).
    #[serde(with = "rfc3339")]
    pub expires_at: SystemTime,
    /// Owner identity (`hostname:pid`); omitted from JSON when empty.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub owner: String,
    /// Object-store ETag of the lock object this lease was read/written from.
    /// Not serialised (matches Go's `json:"-"`).
    #[serde(skip)]
    pub e_tag: String,
}

impl Lease {
    /// Returns `true` once the wall clock has passed `expires_at`.
    ///
    /// Ported from `Lease.IsExpired` (leaser.go:38-40): `time.Now().After(ExpiresAt)`.
    /// Note Go uses strict `After`, so a lease is *not* expired at the exact instant
    /// `now == expires_at`; we match that with `SystemTime::now() > self.expires_at`.
    pub fn is_expired(&self) -> bool {
        SystemTime::now() > self.expires_at
    }

    /// Returns the remaining time until expiry, or `None` once expired.
    ///
    /// Ported from `Lease.TTL` (leaser.go:42-44): `time.Until(ExpiresAt)`. Go
    /// returns a *negative* duration past expiry; Rust `Duration` is unsigned, so
    /// we surface the past-expiry case as `None` rather than a wrapped value.
    pub fn ttl(&self) -> Option<Duration> {
        self.expires_at.duration_since(SystemTime::now()).ok()
    }
}

// ── Leaser trait (leaser.go:24-29) ────────────────────────────────────────────

/// Object-storage lease provider.
///
/// Maps to the `litestream.Leaser` interface (leaser.go:24-29). Object-safe so
/// alternate backends can be added behind feature flags later.
#[async_trait::async_trait]
pub trait Leaser: Send + Sync {
    /// Short identifier for the backend (`"s3"`, …). Ported from `Type()`.
    fn leaser_type(&self) -> &str;

    /// Attempt to acquire the lease. Returns an [`Error`] wrapping
    /// [`LeaseExistsError`] when another node holds a live, unexpired lease.
    async fn acquire_lease(&self) -> Result<Lease>;

    /// Renew a held lease. Returns an [`Error`] wrapping [`LeaseNotHeld`] when the
    /// conditional write fails (ETag mismatch — another node took over).
    async fn renew_lease(&self, lease: &Lease) -> Result<Lease>;

    /// Release a held lease. Returns [`LeaseNotHeld`] on ETag mismatch and
    /// [`LeaseAlreadyReleased`] when the lock object is already gone.
    async fn release_lease(&self, lease: &Lease) -> Result<()>;
}

// ── S3 implementation (s3/leaser.go:42-266) ───────────────────────────────────

/// S3-backed lease using the `object_store` crate as the transport.
///
/// Maps to `s3.Leaser` (s3/leaser.go:42-50). Backend-agnostic: any
/// `Arc<dyn ObjectStore>` that honours `PutMode::Create` / `PutMode::Update`
/// (S3/MinIO/R2 via `object_store/aws`, or the in-memory store in tests) works.
pub struct S3Leaser {
    store: std::sync::Arc<dyn object_store::ObjectStore>,
    /// Prefix under which the lock object lives; the key is `path/lock.json`
    /// (or just `lock.json` when empty). See [`S3Leaser::lock_key`].
    path: String,
    /// Lease time-to-live; `expires_at = now() + ttl` is stamped at write time.
    ttl: Duration,
    /// Owner identity written into the lease (default `hostname:pid`).
    owner: String,
}

impl S3Leaser {
    /// Creates a leaser over the given object store with the defaults from
    /// `NewLeaser` (s3/leaser.go:52-65): empty path, 30 s TTL, owner `hostname:pid`.
    pub fn new(store: std::sync::Arc<dyn object_store::ObjectStore>) -> Self {
        S3Leaser {
            store,
            path: String::new(),
            ttl: DEFAULT_LEASE_TTL,
            owner: default_owner(),
        }
    }

    /// Sets the object-store prefix the lock object lives under.
    pub fn with_path(mut self, path: impl Into<String>) -> Self {
        self.path = path.into();
        self
    }

    /// Overrides the lease TTL (default [`DEFAULT_LEASE_TTL`]).
    pub fn with_ttl(mut self, ttl: Duration) -> Self {
        self.ttl = ttl;
        self
    }

    /// Overrides the owner identity (default `hostname:pid`).
    pub fn with_owner(mut self, owner: impl Into<String>) -> Self {
        self.owner = owner.into();
        self
    }

    /// The owner identity this leaser stamps into acquired leases.
    pub fn owner(&self) -> &str {
        &self.owner
    }

    /// The configured lease TTL.
    pub fn ttl(&self) -> Duration {
        self.ttl
    }

    /// Builds the lock-object key: `lock.json` when `path` is empty, else
    /// `<path>/lock.json`. Ported byte-for-byte from `lockKey` (s3/leaser.go:83-88).
    fn lock_key(&self) -> object_store::path::Path {
        let key = if self.path.is_empty() {
            DEFAULT_LEASE_PATH.to_string()
        } else {
            format!("{}/{}", self.path, DEFAULT_LEASE_PATH)
        };
        object_store::path::Path::from(key)
    }

    /// Reads the current lock object. Returns `Ok(None)` when the object is absent
    /// (the Go `os.ErrNotExist` path, s3/leaser.go:210-212); otherwise the decoded
    /// [`Lease`] with its `e_tag` populated from the object metadata.
    ///
    /// Ported from `readLease` (s3/leaser.go:202-229).
    async fn read_lease(&self) -> Result<Option<Lease>> {
        let key = self.lock_key();
        let result = match self.store.get(&key).await {
            Ok(r) => r,
            Err(object_store::Error::NotFound { .. }) => return Ok(None),
            Err(e) => {
                return Err(Error::Other(format!("get lock file: {e}").into()));
            }
        };

        // Capture the ETag before consuming the body (s3/leaser.go:222-226).
        let etag = result.meta.e_tag.clone().unwrap_or_default();
        let bytes = result
            .bytes()
            .await
            .map_err(|e| Error::Other(format!("read lock file: {e}").into()))?;

        let mut lease: Lease = serde_json::from_slice(&bytes)
            .map_err(|e| Error::Other(format!("decode lock file: {e}").into()))?;
        lease.e_tag = etag;
        Ok(Some(lease))
    }

    /// Writes `lease` to the lock object under compare-and-swap:
    ///
    ///   - `current_e_tag == ""` → `PutMode::Create` (`If-None-Match: *`).
    ///   - `current_e_tag == etag` → `PutMode::Update(If-Match: etag)`.
    ///
    /// Returns the new ETag on success. A failed CAS (`AlreadyExists` from a losing
    /// Create, or `Precondition` from a losing Update) is surfaced as a *blank*
    /// [`LeaseExistsError`] for the caller to enrich via a re-read — exactly as
    /// Go's `writeLease` returns `&LeaseExistsError{}` on 412 (s3/leaser.go:254-256).
    ///
    /// Ported from `writeLease` (s3/leaser.go:231-266).
    async fn write_lease(&self, lease: &Lease, current_e_tag: &str) -> Result<String> {
        let key = self.lock_key();
        let data = serde_json::to_vec(lease)
            .map_err(|e| Error::Other(format!("marshal lock file: {e}").into()))?;

        // s3/leaser.go:246-250: empty etag → IfNoneMatch:"*" (Create), else
        // IfMatch:<etag> (Update).
        let mode = if current_e_tag.is_empty() {
            object_store::PutMode::Create
        } else {
            object_store::PutMode::Update(object_store::UpdateVersion {
                e_tag: Some(current_e_tag.to_string()),
                version: None,
            })
        };

        // Match the Go uploader's `ContentType: application/json` (s3/leaser.go:243)
        // via the object_store attribute of the same name.
        let mut attributes = object_store::Attributes::new();
        attributes.insert(
            object_store::Attribute::ContentType,
            object_store::AttributeValue::from("application/json"),
        );

        let opts = object_store::PutOptions {
            mode,
            attributes,
            ..Default::default()
        };

        let result = match self
            .store
            .put_opts(&key, object_store::PutPayload::from(data), opts)
            .await
        {
            Ok(r) => r,
            // Both map to Go's `isPreconditionFailed` → `&LeaseExistsError{}`:
            //   * `AlreadyExists`  — losing `Create` (S3 returns 412 for a
            //                        conflicting If-None-Match:*; object_store's
            //                        InMemory store surfaces it as AlreadyExists).
            //   * `Precondition`   — losing `Update` (If-Match mismatch / 412).
            // The brief (§5.1) requires mapping BOTH (s3/leaser.go:254-256).
            Err(object_store::Error::AlreadyExists { .. })
            | Err(object_store::Error::Precondition { .. }) => {
                return Err(Error::Other(Box::new(LeaseExistsError {
                    owner: String::new(),
                    expires_at: SystemTime::UNIX_EPOCH,
                })));
            }
            Err(e) => {
                return Err(Error::Other(format!("put lock file: {e}").into()));
            }
        };

        Ok(result.e_tag.unwrap_or_default())
    }
}

#[async_trait::async_trait]
impl Leaser for S3Leaser {
    fn leaser_type(&self) -> &str {
        LEASER_TYPE
    }

    /// Ported from `AcquireLease` (s3/leaser.go:90-136).
    async fn acquire_lease(&self) -> Result<Lease> {
        // Read any existing lease. Absence (None) is fine; any other read error
        // propagates (s3/leaser.go:91-94).
        let existing = self.read_lease().await?;
        let existing_etag = existing
            .as_ref()
            .map(|l| l.e_tag.clone())
            .unwrap_or_default();

        // A live unexpired lease blocks acquisition (s3/leaser.go:96-101).
        if let Some(ref ex) = existing {
            if !ex.is_expired() {
                return Err(Error::Other(Box::new(LeaseExistsError {
                    owner: ex.owner.clone(),
                    expires_at: ex.expires_at,
                })));
            }
        }

        // generation = existing+1, or 1 when there was no prior lease
        // (s3/leaser.go:103-107).
        let generation = match existing {
            Some(ref ex) => ex.generation + 1,
            None => 1,
        };

        let new_lease = Lease {
            generation,
            expires_at: SystemTime::now() + self.ttl,
            owner: self.owner.clone(),
            e_tag: String::new(),
        };

        // CAS-write. On a losing CAS, re-read to populate the winner's identity in
        // the returned LeaseExistsError (s3/leaser.go:114-126) — never surface the
        // blank error writeLease produced.
        let new_etag = match self.write_lease(&new_lease, &existing_etag).await {
            Ok(etag) => etag,
            Err(e) if e.as_lease_exists().is_some() => {
                if let Ok(Some(current)) = self.read_lease().await {
                    return Err(Error::Other(Box::new(LeaseExistsError {
                        owner: current.owner,
                        expires_at: current.expires_at,
                    })));
                }
                return Err(e);
            }
            Err(e) => return Err(e),
        };

        let mut acquired = new_lease;
        acquired.e_tag = new_etag;
        tracing::debug!(
            generation = acquired.generation,
            owner = %acquired.owner,
            etag = %acquired.e_tag,
            "lease acquired"
        );
        Ok(acquired)
    }

    /// Ported from `RenewLease` (s3/leaser.go:138-169).
    async fn renew_lease(&self, lease: &Lease) -> Result<Lease> {
        // Go guards nil-lease before ETag; our `&Lease` cannot be null, so the
        // `ErrLeaseRequired` guard (s3/leaser.go:139-141) is unrepresentable here.
        // The empty-ETag guard remains load-bearing (s3/leaser.go:142-144).
        if lease.e_tag.is_empty() {
            return Err(LeaseETagRequired.into());
        }

        // Renewal keeps the same generation, only extends the TTL (s3/leaser.go:146-150).
        let new_lease = Lease {
            generation: lease.generation,
            expires_at: SystemTime::now() + self.ttl,
            owner: self.owner.clone(),
            e_tag: String::new(),
        };

        // A failed CAS (someone wrote the key since we last saw it) → LeaseNotHeld
        // (s3/leaser.go:152-159).
        let new_etag = match self.write_lease(&new_lease, &lease.e_tag).await {
            Ok(etag) => etag,
            Err(e) if e.as_lease_exists().is_some() => return Err(LeaseNotHeld.into()),
            Err(e) => return Err(e),
        };

        let mut renewed = new_lease;
        renewed.e_tag = new_etag;
        tracing::debug!(
            generation = renewed.generation,
            owner = %renewed.owner,
            etag = %renewed.e_tag,
            "lease renewed"
        );
        Ok(renewed)
    }

    /// Ported from `ReleaseLease` (s3/leaser.go:171-200), via the read-then-delete
    /// strategy documented in the module header (object_store has no conditional
    /// DELETE).
    async fn release_lease(&self, lease: &Lease) -> Result<()> {
        if lease.e_tag.is_empty() {
            return Err(LeaseETagRequired.into());
        }

        // DEVIATION from Go: read-then-delete to emulate DeleteObject{IfMatch}.
        // Absent object → already released (Go 404 path, s3/leaser.go:186-188).
        let current = match self.read_lease().await? {
            Some(c) => c,
            None => return Err(LeaseAlreadyReleased.into()),
        };

        // ETag mismatch → another node owns the key now (Go 412 path,
        // s3/leaser.go:189-191).
        if current.e_tag != lease.e_tag {
            return Err(LeaseNotHeld.into());
        }

        // ETag matches: delete. A NotFound during the delete means the object was
        // removed in the TOCTOU window — treat as success (the lock is gone, which
        // is the post-condition release wants).
        let key = self.lock_key();
        match self.store.delete(&key).await {
            Ok(()) => {}
            Err(object_store::Error::NotFound { .. }) => {}
            Err(e) => return Err(Error::Other(format!("delete lease: {e}").into())),
        }

        tracing::debug!(
            generation = lease.generation,
            owner = %lease.owner,
            "lease released"
        );
        Ok(())
    }
}

/// Default owner identity `hostname:pid`, falling back to `pid-<pid>` when the
/// hostname is unavailable. Ported from `NewLeaser` (s3/leaser.go:53-58).
fn default_owner() -> String {
    let pid = std::process::id();
    match hostname() {
        Some(h) if !h.is_empty() => format!("{h}:{pid}"),
        _ => format!("pid-{pid}"),
    }
}

/// Best-effort hostname lookup with no external crate. Returns `None` when
/// unavailable, mirroring Go's `os.Hostname()` returning an error
/// (s3/leaser.go:53-54), which the caller treats as the empty string.
///
/// The owner string is purely informational (it populates `LeaseExistsError` for
/// operators); it is never used in a correctness decision, so an env-based
/// best-effort source is acceptable and avoids a libc/`hostname` dependency
/// (AGENTS.md §7).
fn hostname() -> Option<String> {
    std::env::var("HOSTNAME")
        .ok()
        .filter(|h| !h.is_empty())
        .or_else(|| std::env::var("COMPUTERNAME").ok().filter(|h| !h.is_empty()))
}

// ── HeartbeatClient (heartbeat.go:17-84) ──────────────────────────────────────

/// Optional HTTP liveness-ping companion for the renew loop.
///
/// Maps to `litestream.HeartbeatClient` (heartbeat.go:17-26). It fires a throttled
/// HTTP GET at a configurable URL; an empty URL makes [`HeartbeatClient::ping`] a
/// no-op. It does not interact with the lease protocol itself (brief §1).
///
/// The throttle state machine ([`should_ping`](Self::should_ping) /
/// [`record_ping`](Self::record_ping) / [`last_ping_at`](Self::last_ping_at)) and
/// the [`MIN_HEARTBEAT_INTERVAL`] clamp are always available; the actual HTTP GET
/// in `ping()` is compiled only when the `s3` feature is on (that is where
/// `reqwest` — already a transitive dependency of `object_store/aws` — is present).
/// Under `--no-default-features` `ping()` still exists and still honours the
/// empty-URL no-op; a non-empty URL there returns an error rather than silently
/// doing nothing, so the contract is never weakened.
pub struct HeartbeatClient {
    /// Target URL; empty = disabled.
    url: String,
    /// Minimum spacing between pings (clamped to `>= MIN_HEARTBEAT_INTERVAL`).
    interval: Duration,
    /// HTTP request timeout.
    timeout: Duration,
    /// Monotonic instant of the last recorded ping (`None` until first).
    last_ping_at: Mutex<Option<Instant>>,
}

impl HeartbeatClient {
    /// Constructs a heartbeat client, clamping `interval` up to
    /// [`MIN_HEARTBEAT_INTERVAL`]. Ported from `NewHeartbeatClient` (heartbeat.go:28-43).
    pub fn new(url: impl Into<String>, interval: Duration) -> Self {
        let interval = if interval < MIN_HEARTBEAT_INTERVAL {
            MIN_HEARTBEAT_INTERVAL
        } else {
            interval
        };
        HeartbeatClient {
            url: url.into(),
            interval,
            timeout: DEFAULT_HEARTBEAT_TIMEOUT,
            last_ping_at: Mutex::new(None),
        }
    }

    /// The configured target URL.
    pub fn url(&self) -> &str {
        &self.url
    }

    /// The (clamped) ping interval.
    pub fn interval(&self) -> Duration {
        self.interval
    }

    /// The request timeout.
    pub fn timeout(&self) -> Duration {
        self.timeout
    }

    /// Returns `true` once at least `interval` has elapsed since the last ping
    /// (always `true` before the first ping). Ported from `ShouldPing`
    /// (heartbeat.go:68-72): `time.Since(lastPingAt) >= Interval`.
    pub fn should_ping(&self) -> bool {
        let last = *self.last_ping_at.lock().expect("heartbeat mutex poisoned");
        match last {
            None => true,
            Some(t) => t.elapsed() >= self.interval,
        }
    }

    /// The instant of the last recorded ping, if any. Ported from `LastPingAt`
    /// (heartbeat.go:74-78).
    pub fn last_ping_at(&self) -> Option<Instant> {
        *self.last_ping_at.lock().expect("heartbeat mutex poisoned")
    }

    /// Records the current instant as the last ping time. Ported from `RecordPing`
    /// (heartbeat.go:80-84).
    pub fn record_ping(&self) {
        *self.last_ping_at.lock().expect("heartbeat mutex poisoned") = Some(Instant::now());
    }

    /// Fires the liveness GET. A no-op (returns `Ok`) when the URL is empty.
    /// A non-2xx response is an error. Ported from `Ping` (heartbeat.go:45-66).
    ///
    /// This does **not** call [`record_ping`](Self::record_ping); the renew loop
    /// records the ping itself, matching the Go split between `Ping` and
    /// `RecordPing`.
    #[cfg(feature = "s3")]
    pub async fn ping(&self) -> Result<()> {
        if self.url.is_empty() {
            return Ok(());
        }
        let client = reqwest::Client::builder()
            .timeout(self.timeout)
            .build()
            .map_err(|e| Error::Other(format!("create request: {e}").into()))?;
        let resp = client
            .get(&self.url)
            .send()
            .await
            .map_err(|e| Error::Other(format!("http request: {e}").into()))?;
        let status = resp.status().as_u16();
        // heartbeat.go:61-63: anything outside [200,300) is an error.
        if !(200..300).contains(&status) {
            return Err(Error::Other(
                format!("unexpected status code: {status}").into(),
            ));
        }
        Ok(())
    }

    /// Fallback when the `s3` feature (and therefore `reqwest`) is absent: the
    /// empty-URL no-op is preserved; a non-empty URL is an explicit error so the
    /// liveness contract is never silently dropped.
    #[cfg(not(feature = "s3"))]
    pub async fn ping(&self) -> Result<()> {
        if self.url.is_empty() {
            return Ok(());
        }
        Err(Error::Other(
            "heartbeat ping requires the `s3` feature (HTTP client unavailable)".into(),
        ))
    }
}

// ── RFC-3339 SystemTime (de)serialisation ─────────────────────────────────────
//
// Go marshals `Lease.ExpiresAt` (a `time.Time`) with the default `time.Time`
// JSON encoder, i.e. RFC-3339 with nanosecond precision (`time.RFC3339Nano`,
// e.g. "2026-05-30T12:34:56.789012345Z"). The lock object is rustyriver-internal
// (read back only by this same code, brief §3), so the requirement is faithful
// round-tripping at nanosecond resolution — which `SystemTime::now()` carries and
// the millisecond-only helper in `client/object_store.rs` would truncate. We keep
// this module self-contained (no `chrono`/`time` dependency, matching the T7
// precedent logged in OPEN_QUESTIONS.md) and emit/parse a `…Z` UTC RFC-3339 with
// up-to-9-digit trailing-zero-trimmed fractional seconds, which Go's
// `time.RFC3339Nano` parser accepts verbatim.
mod rfc3339 {
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use serde::{de::Error as _, Deserialize, Deserializer, Serializer};

    /// serde `serialize_with` entry point for a `SystemTime` field.
    pub fn serialize<S: Serializer>(t: &SystemTime, s: S) -> std::result::Result<S::Ok, S::Error> {
        s.serialize_str(&format(*t))
    }

    /// serde `deserialize_with` entry point for a `SystemTime` field.
    pub fn deserialize<'de, D: Deserializer<'de>>(
        d: D,
    ) -> std::result::Result<SystemTime, D::Error> {
        let s = String::deserialize(d)?;
        parse(&s).ok_or_else(|| D::Error::custom(format!("invalid RFC-3339 timestamp: {s:?}")))
    }

    /// Formats a `SystemTime` as an RFC-3339 UTC string (`…Z`), trimming trailing
    /// zeros from the fractional-second field (Go `time.RFC3339Nano` behaviour).
    /// Pre-epoch times are clamped to the epoch (lease expiries are always future
    /// wall-clock values, so this only guards the degenerate UNIX_EPOCH sentinel).
    pub fn format(t: SystemTime) -> String {
        let dur = t.duration_since(UNIX_EPOCH).unwrap_or(Duration::ZERO);
        let secs = dur.as_secs() as i64;
        let nanos = dur.subsec_nanos();
        let (year, month, day, hour, min, sec) = civil_from_unix_secs(secs);
        if nanos == 0 {
            format!("{year:04}-{month:02}-{day:02}T{hour:02}:{min:02}:{sec:02}Z")
        } else {
            // 9-digit nanos, trailing zeros trimmed (matches RFC3339Nano).
            let mut frac = format!("{nanos:09}");
            while frac.ends_with('0') {
                frac.pop();
            }
            format!("{year:04}-{month:02}-{day:02}T{hour:02}:{min:02}:{sec:02}.{frac}Z")
        }
    }

    /// Parses a `…Z` RFC-3339 UTC string with optional fractional seconds back
    /// into a `SystemTime`. Returns `None` on any shape we don't recognise.
    pub fn parse(s: &str) -> Option<SystemTime> {
        let body = s.strip_suffix('Z')?;
        let (date, time) = body.split_once('T')?;

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
        if secs < 0 {
            return None;
        }
        let nanos: u32 = match frac {
            None => 0,
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
        };
        Some(UNIX_EPOCH + Duration::new(secs as u64, nanos))
    }

    /// Unix seconds (>= 0) → civil `(year, month, day, hour, min, sec)` in UTC.
    /// Howard Hinnant's `civil_from_days` (public-domain), exact for all dates.
    /// Mirrors the helper in `client/object_store.rs` (kept local to avoid a
    /// cross-module private coupling).
    fn civil_from_unix_secs(secs: i64) -> (i64, u32, u32, u32, u32, u32) {
        let days = secs.div_euclid(86_400);
        let rem = secs.rem_euclid(86_400);
        let hour = (rem / 3600) as u32;
        let min = ((rem % 3600) / 60) as u32;
        let sec = (rem % 60) as u32;

        let z = days + 719_468;
        let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
        let doe = z - era * 146_097;
        let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
        let y = yoe + era * 400;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
        let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
        let year = if m <= 2 { y + 1 } else { y };
        (year, m, d, hour, min, sec)
    }

    /// Inverse of [`civil_from_unix_secs`]: civil UTC → Unix seconds. Returns
    /// `None` for an out-of-range month/day. Howard Hinnant's `days_from_civil`.
    fn unix_secs_from_civil(
        year: i64,
        month: u32,
        day: u32,
        hour: u32,
        min: u32,
        sec: u32,
    ) -> Option<i64> {
        if !(1..=12).contains(&month)
            || !(1..=31).contains(&day)
            || hour > 23
            || min > 59
            || sec > 60
        {
            return None;
        }
        let y = if month <= 2 { year - 1 } else { year };
        let era = if y >= 0 { y } else { y - 399 } / 400;
        let yoe = y - era * 400;
        let m = month as i64;
        let d = day as i64;
        let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
        let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
        let days = era * 146_097 + doe - 719_468;
        Some(days * 86_400 + hour as i64 * 3600 + min as i64 * 60 + sec as i64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use object_store::memory::InMemory;
    use object_store::ObjectStore;
    use std::sync::Arc;

    // ── Lease value-type & serde ──────────────────────────────────────────────

    #[test]
    fn lease_json_shape_matches_go() {
        // Go marshals `{generation, expires_at (RFC-3339), owner?}` and drops ETag.
        let lease = Lease {
            generation: 7,
            // 2021-01-01T00:00:00Z exactly (no fractional part).
            expires_at: SystemTime::UNIX_EPOCH + Duration::from_secs(1_609_459_200),
            owner: "host-a:42".to_string(),
            e_tag: "\"should-not-appear\"".to_string(),
        };
        let json = serde_json::to_string(&lease).unwrap();
        assert_eq!(
            json,
            r#"{"generation":7,"expires_at":"2021-01-01T00:00:00Z","owner":"host-a:42"}"#
        );
        // ETag must not be serialised (Go json:"-").
        assert!(!json.contains("should-not-appear"));
        assert!(!json.contains("e_tag"));
    }

    #[test]
    fn lease_owner_omitted_when_empty() {
        let lease = Lease {
            generation: 1,
            expires_at: SystemTime::UNIX_EPOCH + Duration::from_secs(1_609_459_200),
            owner: String::new(),
            e_tag: String::new(),
        };
        let json = serde_json::to_string(&lease).unwrap();
        assert_eq!(
            json,
            r#"{"generation":1,"expires_at":"2021-01-01T00:00:00Z"}"#
        );
    }

    #[test]
    fn lease_serde_round_trips_with_nanos() {
        // SystemTime::now() carries sub-millisecond precision; ensure it survives.
        let original = Lease {
            generation: 99,
            expires_at: SystemTime::UNIX_EPOCH + Duration::new(1_700_000_000, 123_456_789),
            owner: "n:1".to_string(),
            e_tag: "\"etag-x\"".to_string(),
        };
        let json = serde_json::to_string(&original).unwrap();
        assert!(json.contains("2023-11-14T22:13:20.123456789Z"));
        let mut decoded: Lease = serde_json::from_str(&json).unwrap();
        // ETag is not serialised, so re-attach before comparing the rest.
        assert_eq!(decoded.e_tag, "");
        decoded.e_tag = original.e_tag.clone();
        assert_eq!(decoded, original);
    }

    #[test]
    fn lease_parses_go_style_timestamp() {
        // A timestamp Go would emit (RFC3339Nano, fractional trimmed) must parse.
        let json = r#"{"generation":3,"expires_at":"2023-11-14T22:13:20.5Z","owner":"x"}"#;
        let lease: Lease = serde_json::from_str(json).unwrap();
        let expected = SystemTime::UNIX_EPOCH + Duration::new(1_700_000_000, 500_000_000);
        assert_eq!(lease.expires_at, expected);
        assert_eq!(lease.generation, 3);
    }

    #[test]
    fn lease_is_expired_and_ttl() {
        let past = Lease {
            generation: 1,
            expires_at: SystemTime::now() - Duration::from_secs(60),
            owner: String::new(),
            e_tag: String::new(),
        };
        assert!(past.is_expired());
        assert!(past.ttl().is_none());

        let future = Lease {
            generation: 1,
            expires_at: SystemTime::now() + Duration::from_secs(60),
            owner: String::new(),
            e_tag: String::new(),
        };
        assert!(!future.is_expired());
        let ttl = future.ttl().expect("future lease has positive ttl");
        assert!(ttl <= Duration::from_secs(60) && ttl > Duration::from_secs(50));
    }

    // ── LeaseExistsError Display (port of leaser.go:17-21) ─────────────────────

    #[test]
    fn lease_exists_error_display() {
        let t = SystemTime::UNIX_EPOCH + Duration::from_secs(1_609_459_200);
        let with_owner = LeaseExistsError {
            owner: "host-a:7".to_string(),
            expires_at: t,
        };
        assert_eq!(
            with_owner.to_string(),
            "lease already held by host-a:7 until 2021-01-01T00:00:00Z"
        );
        let no_owner = LeaseExistsError {
            owner: String::new(),
            expires_at: t,
        };
        assert_eq!(
            no_owner.to_string(),
            "lease already held until 2021-01-01T00:00:00Z"
        );
    }

    // ── lockKey (port of TestLeaser_LockKey, leaser_test.go:536-569) ───────────

    #[test]
    fn lock_key_table() {
        let cases = [
            ("", "lock.json"),
            ("replica", "replica/lock.json"),
            ("my/db/replica", "my/db/replica/lock.json"),
        ];
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        for (path, want) in cases {
            let leaser = S3Leaser::new(store.clone()).with_path(path);
            assert_eq!(leaser.lock_key().as_ref(), want, "path={path:?}");
        }
    }

    // ── Type (port of TestLeaser_Type, leaser_test.go:571-577) ─────────────────

    #[test]
    fn leaser_type_is_s3() {
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let leaser = S3Leaser::new(store);
        assert_eq!(leaser.leaser_type(), "s3");
    }

    // ── Acquire (ports of the four AcquireLease tests) ─────────────────────────

    /// Helper: read the lock object's stored lease (panics if absent).
    async fn stored_lease(store: &Arc<dyn ObjectStore>, key: &str) -> Lease {
        let r = store
            .get(&object_store::path::Path::from(key))
            .await
            .expect("lock object present");
        let bytes = r.bytes().await.unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    /// Port of TestLeaser_AcquireLease_NewLease (leaser_test.go:19-76): first
    /// acquire writes generation=1 via Create (If-None-Match:*) and returns a TTL
    /// close to the configured value.
    #[tokio::test]
    async fn acquire_new_lease() {
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let leaser = S3Leaser::new(store.clone())
            .with_owner("me")
            .with_ttl(Duration::from_secs(10));

        let lease = leaser.acquire_lease().await.expect("acquire");
        assert_eq!(lease.generation, 1);
        assert!(!lease.e_tag.is_empty(), "ETag returned from PUT");
        assert_eq!(lease.owner, "me");
        let ttl = lease.ttl().expect("positive ttl");
        assert!(
            ttl > Duration::from_secs(9) && ttl <= Duration::from_secs(10),
            "ttl={ttl:?}"
        );

        // The persisted lock object carries the same generation.
        let persisted = stored_lease(&store, "lock.json").await;
        assert_eq!(persisted.generation, 1);
        assert_eq!(persisted.owner, "me");
    }

    /// Port of TestLeaser_AcquireLease_ExpiredLease (leaser_test.go:78-122): an
    /// expired lease (gen 5) is taken over via Update (If-Match:<old-etag>) with
    /// generation incremented to 6.
    #[tokio::test]
    async fn acquire_over_expired_lease() {
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let key = object_store::path::Path::from("lock.json");

        // Seed an expired lease (no CAS, just place it).
        let expired = Lease {
            generation: 5,
            expires_at: SystemTime::now() - Duration::from_secs(3600),
            owner: "previous-owner".to_string(),
            e_tag: String::new(),
        };
        store
            .put(&key, serde_json::to_vec(&expired).unwrap().into())
            .await
            .unwrap();

        let leaser = S3Leaser::new(store.clone())
            .with_owner("me")
            .with_ttl(Duration::from_secs(30));
        let lease = leaser.acquire_lease().await.expect("acquire over expired");
        assert_eq!(lease.generation, 6, "previous generation + 1");
        assert_eq!(lease.owner, "me");
        assert!(!lease.is_expired());

        let persisted = stored_lease(&store, "lock.json").await;
        assert_eq!(persisted.generation, 6);
    }

    /// Port of TestLeaser_AcquireLease_ActiveLease (leaser_test.go:124-165): an
    /// active (unexpired) lease blocks acquisition with a LeaseExistsError that
    /// carries the holder's owner and a future ExpiresAt; no PUT occurs.
    #[tokio::test]
    async fn acquire_blocked_by_active_lease() {
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let key = object_store::path::Path::from("lock.json");

        let active = Lease {
            generation: 3,
            expires_at: SystemTime::now() + Duration::from_secs(300),
            owner: "active-owner".to_string(),
            e_tag: String::new(),
        };
        store
            .put(&key, serde_json::to_vec(&active).unwrap().into())
            .await
            .unwrap();
        // Capture the current ETag so we can assert the object was NOT rewritten.
        let etag_before = store.head(&key).await.unwrap().e_tag;

        let leaser = S3Leaser::new(store.clone()).with_owner("me");
        let err = leaser
            .acquire_lease()
            .await
            .expect_err("active lease blocks");
        let le = err.as_lease_exists().expect("LeaseExistsError");
        assert_eq!(le.owner, "active-owner");
        assert!(le.expires_at > SystemTime::now(), "future ExpiresAt");

        // No PUT should have happened (ETag unchanged).
        let etag_after = store.head(&key).await.unwrap().e_tag;
        assert_eq!(
            etag_before, etag_after,
            "active lease must not be rewritten"
        );
    }

    /// Port of TestLeaser_AcquireLease_RaceCondition412 (leaser_test.go:167-211):
    /// a losing CAS (the lock object already holds another, unexpired lease) must
    /// re-read to surface the winner's identity in the LeaseExistsError, never a
    /// blank one. The deterministic in-process analogue of the 412-then-reread
    /// branch; the live N-way race is `two_contenders_exactly_one_primary`.
    #[tokio::test]
    async fn acquire_race_condition_reread_returns_winner() {
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let key = object_store::path::Path::from("lock.json");

        let winner = Lease {
            generation: 1,
            expires_at: SystemTime::now() + Duration::from_secs(30),
            owner: "race-winner".to_string(),
            e_tag: String::new(),
        };
        store
            .put(&key, serde_json::to_vec(&winner).unwrap().into())
            .await
            .unwrap();

        let leaser = S3Leaser::new(store.clone()).with_owner("loser");
        let err = leaser.acquire_lease().await.expect_err("loser blocked");
        let le = err.as_lease_exists().expect("LeaseExistsError");
        assert_eq!(le.owner, "race-winner");
        assert!(
            le.expires_at > SystemTime::now(),
            "non-zero future ExpiresAt"
        );
    }

    // ── Renew (ports of the RenewLease tests) ──────────────────────────────────

    /// Port of TestLeaser_RenewLease (leaser_test.go:213-268): renewal keeps the
    /// generation, extends the TTL, and forwards If-Match:<old-etag>.
    #[tokio::test]
    async fn renew_lease_extends_ttl_keeps_generation() {
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let leaser = S3Leaser::new(store.clone())
            .with_owner("me")
            .with_ttl(Duration::from_secs(30));

        // Acquire first to get a real ETag for the lock object.
        let lease = leaser.acquire_lease().await.expect("acquire");
        let old_etag = lease.e_tag.clone();
        assert_eq!(lease.generation, 1);

        let renewed = leaser.renew_lease(&lease).await.expect("renew");
        assert_eq!(renewed.generation, 1, "generation unchanged on renew");
        assert_ne!(renewed.e_tag, old_etag, "new ETag after rewrite");
        let ttl = renewed.ttl().expect("positive ttl");
        assert!(ttl > Duration::from_secs(29), "ttl≈30s, got {ttl:?}");
    }

    /// Port of TestLeaser_RenewLease_LostLease (leaser_test.go:270-292): a stale
    /// ETag → the CAS Update loses → LeaseNotHeld.
    #[tokio::test]
    async fn renew_lease_lost_returns_not_held() {
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let leaser = S3Leaser::new(store.clone()).with_owner("me");
        let mut lease = leaser.acquire_lease().await.expect("acquire");
        // Corrupt the ETag so the If-Match Update fails.
        lease.e_tag = "\"stale-etag\"".to_string();

        let err = leaser
            .renew_lease(&lease)
            .await
            .expect_err("renew should fail");
        assert!(
            err.is_lease_not_held(),
            "expected LeaseNotHeld, got {err:?}"
        );
    }

    /// Port of TestLeaser_RenewLease_EmptyETag (leaser_test.go:309-328): an empty
    /// ETag is rejected before any I/O.
    #[tokio::test]
    async fn renew_lease_empty_etag_rejected() {
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let leaser = S3Leaser::new(store).with_owner("me");
        let lease = Lease {
            generation: 5,
            expires_at: SystemTime::now() + Duration::from_secs(5),
            owner: "me".to_string(),
            e_tag: String::new(),
        };
        let err = leaser
            .renew_lease(&lease)
            .await
            .expect_err("empty etag rejected");
        assert!(err.is_lease_etag_required(), "got {err:?}");
    }

    // ── Release (ports of the ReleaseLease tests) ──────────────────────────────

    /// Port of TestLeaser_ReleaseLease (leaser_test.go:330-364): a matching ETag
    /// deletes the lock object.
    #[tokio::test]
    async fn release_lease_deletes_object() {
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let key = object_store::path::Path::from("lock.json");
        let leaser = S3Leaser::new(store.clone()).with_owner("me");

        let lease = leaser.acquire_lease().await.expect("acquire");
        assert!(store.head(&key).await.is_ok(), "lock exists after acquire");

        leaser.release_lease(&lease).await.expect("release");
        let err = store
            .head(&key)
            .await
            .expect_err("lock removed after release");
        assert!(matches!(err, object_store::Error::NotFound { .. }));
    }

    /// Port of TestLeaser_ReleaseLease_StaleETag (leaser_test.go:366-388): a stale
    /// ETag → LeaseNotHeld, and the object is left intact.
    #[tokio::test]
    async fn release_lease_stale_etag_not_held() {
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let key = object_store::path::Path::from("lock.json");
        let leaser = S3Leaser::new(store.clone()).with_owner("me");

        let mut lease = leaser.acquire_lease().await.expect("acquire");
        lease.e_tag = "\"stale-etag\"".to_string();

        let err = leaser
            .release_lease(&lease)
            .await
            .expect_err("stale release fails");
        assert!(err.is_lease_not_held(), "got {err:?}");
        assert!(store.head(&key).await.is_ok(), "lock object must survive");
    }

    /// Port of TestLeaser_ReleaseLease_AlreadyDeleted (leaser_test.go:390-412): the
    /// object is already gone → LeaseAlreadyReleased.
    #[tokio::test]
    async fn release_lease_already_deleted() {
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let leaser = S3Leaser::new(store.clone()).with_owner("me");
        let lease = Lease {
            generation: 5,
            expires_at: SystemTime::now() + Duration::from_secs(300),
            owner: "me".to_string(),
            e_tag: "\"my-etag\"".to_string(),
        };
        // No lock object exists.
        let err = leaser
            .release_lease(&lease)
            .await
            .expect_err("already released");
        assert!(err.is_lease_already_released(), "got {err:?}");
    }

    /// Port of TestLeaser_ReleaseLease_EmptyETag (leaser_test.go:429-448): an empty
    /// ETag is rejected before any I/O.
    #[tokio::test]
    async fn release_lease_empty_etag_rejected() {
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let leaser = S3Leaser::new(store).with_owner("me");
        let lease = Lease {
            generation: 5,
            expires_at: SystemTime::now() + Duration::from_secs(300),
            owner: "me".to_string(),
            e_tag: String::new(),
        };
        let err = leaser
            .release_lease(&lease)
            .await
            .expect_err("empty etag rejected");
        assert!(err.is_lease_etag_required(), "got {err:?}");
    }

    // ── Concurrency: two/many contenders → exactly one primary ─────────────────

    /// Port of TestLeaser_ConcurrentAcquisition (leaser_test.go:450-534): N tasks
    /// race to acquire against one real (in-memory) store; the CAS Create must let
    /// **exactly one** win and the other N-1 see LeaseExistsError.
    ///
    /// This is also the PLAN.md T15 "two contenders → exactly one primary" gate,
    /// run here against `object_store::InMemory` (a faithful CAS store) so it needs
    /// no MinIO; the MinIO mirror is the auto-skipping integration test.
    #[tokio::test]
    async fn two_contenders_exactly_one_primary() {
        const NUM: usize = 10;
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());

        let mut handles = Vec::with_capacity(NUM);
        for i in 0..NUM {
            let store = store.clone();
            handles.push(tokio::spawn(async move {
                let leaser = S3Leaser::new(store).with_owner(format!("node-{i}"));
                leaser.acquire_lease().await
            }));
        }

        let mut success = 0usize;
        let mut failure = 0usize;
        let mut winner_owner = None;
        for h in handles {
            match h.await.expect("task join") {
                Ok(lease) => {
                    success += 1;
                    winner_owner = Some(lease.owner);
                }
                Err(e) => {
                    failure += 1;
                    assert!(
                        e.as_lease_exists().is_some(),
                        "losers must get LeaseExistsError, got {e:?}"
                    );
                }
            }
        }
        assert_eq!(success, 1, "exactly one acquirer wins");
        assert_eq!(failure, NUM - 1, "the rest stand by");
        assert!(winner_owner.is_some());
    }

    /// Expiry → failover: once the primary's lease has expired, a standby contender
    /// can take over (generation increments), and the previous holder can no longer
    /// renew (its ETag is now stale → LeaseNotHeld). This is the PLAN.md T15
    /// "expiry → failover" gate, exercised against a real CAS store with a tiny TTL.
    #[tokio::test]
    async fn expiry_failover_standby_takes_over() {
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());

        // Primary acquires with a 1 ms TTL so the lease expires almost immediately.
        let primary = S3Leaser::new(store.clone())
            .with_owner("primary")
            .with_ttl(Duration::from_millis(1));
        let primary_lease = primary.acquire_lease().await.expect("primary acquires");
        assert_eq!(primary_lease.generation, 1);

        // A standby will take over once the lease lapses.
        let standby = S3Leaser::new(store.clone())
            .with_owner("standby")
            .with_ttl(Duration::from_secs(30));

        // Wait until the primary's lease is observably expired.
        while !primary_lease.is_expired() {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }

        // Once expired, the standby takes over with generation+1.
        let standby_lease = standby
            .acquire_lease()
            .await
            .expect("standby fails over after expiry");
        assert_eq!(
            standby_lease.generation, 2,
            "failover increments generation"
        );
        assert_eq!(standby_lease.owner, "standby");

        // The deposed primary can no longer renew: its ETag is stale now.
        let err = primary
            .renew_lease(&primary_lease)
            .await
            .expect_err("deposed primary cannot renew");
        assert!(err.is_lease_not_held(), "got {err:?}");
    }

    // ── HeartbeatClient (heartbeat.go behaviour) ───────────────────────────────

    /// The constructor clamps the interval up to MIN_HEARTBEAT_INTERVAL
    /// (heartbeat.go:29-31) and leaves a larger interval untouched.
    #[test]
    fn heartbeat_interval_clamped_to_minimum() {
        let c = HeartbeatClient::new("http://x/ping", Duration::from_secs(1));
        assert_eq!(c.interval(), MIN_HEARTBEAT_INTERVAL);

        let big = HeartbeatClient::new("http://x/ping", Duration::from_secs(600));
        assert_eq!(big.interval(), Duration::from_secs(600));

        assert_eq!(c.timeout(), DEFAULT_HEARTBEAT_TIMEOUT);
    }

    /// `should_ping` is true before the first ping and false immediately after a
    /// recorded ping (heartbeat.go:68-72, 80-84).
    #[test]
    fn heartbeat_should_ping_throttle() {
        let c = HeartbeatClient::new("http://x/ping", Duration::from_secs(600));
        assert!(c.should_ping(), "no prior ping → should ping");
        assert!(c.last_ping_at().is_none());

        c.record_ping();
        assert!(c.last_ping_at().is_some());
        assert!(
            !c.should_ping(),
            "just pinged with a 10-min interval → wait"
        );
    }

    /// An empty URL makes `ping` a no-op success on all feature configurations
    /// (heartbeat.go:46-48).
    #[tokio::test]
    async fn heartbeat_empty_url_is_noop() {
        let c = HeartbeatClient::new("", Duration::from_secs(600));
        c.ping().await.expect("empty-url ping is a no-op success");
    }

    /// A successful 2xx HTTP GET satisfies `ping`; a non-2xx is an error. Exercises
    /// the real reqwest path against a local one-shot TCP server (only under `s3`,
    /// where reqwest is available).
    #[cfg(feature = "s3")]
    #[tokio::test]
    async fn heartbeat_ping_http_status_handling() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        // Spin up a trivial HTTP server that replies with a fixed status line.
        async fn serve_once(status_line: &'static str) -> String {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            tokio::spawn(async move {
                if let Ok((mut sock, _)) = listener.accept().await {
                    // Drain the request (best-effort; ignore the bytes).
                    let mut buf = [0u8; 1024];
                    let _ = sock.read(&mut buf).await;
                    let body = "ok";
                    let resp = format!(
                        "{status_line}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = sock.write_all(resp.as_bytes()).await;
                    let _ = sock.shutdown().await;
                }
            });
            format!("http://{addr}/ping")
        }

        // 200 OK → ping succeeds.
        let url_ok = serve_once("HTTP/1.1 200 OK").await;
        let c_ok = HeartbeatClient::new(url_ok, Duration::from_secs(600));
        c_ok.ping().await.expect("200 → ok");

        // 500 → ping errors.
        let url_err = serve_once("HTTP/1.1 500 Internal Server Error").await;
        let c_err = HeartbeatClient::new(url_err, Duration::from_secs(600));
        let err = c_err.ping().await.expect_err("500 → error");
        assert!(
            err.to_string().contains("unexpected status code: 500"),
            "got {err}"
        );
    }
}
