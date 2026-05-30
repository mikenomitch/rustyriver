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

pub mod client;
pub mod db;
pub mod error;
pub mod leaser;
pub mod ltx;
pub mod replica;
pub mod replica_url;
pub mod store;
pub mod wal;
