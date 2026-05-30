# rustyriver

An embeddable Rust library for **streaming replication of a SQLite database to
object storage**, with point-in-time restore and object-storage lease fencing —
a from-scratch reimplementation of **Litestream v0.5** (pinned `v0.5.11`) as an
in-process async API instead of a sidecar binary.

> Status: **under construction** via the task DAG in [`PLAN.md`](PLAN.md). This is
> a wire-compatible port (D-1), differentially tested against the real
> `litestream` binary. The one-shot scope is **L0-only** (no compaction), single
> replica, S3 + file clients, and lease fencing.

## Repository map

| Path | Role |
|------|------|
| [`PLAN.md`](PLAN.md) | The full build spec: scope (§2), architecture (§4), task DAG (§5), verification (§6), gates G1–G5. |
| [`AGENTS.md`](AGENTS.md) | Non-negotiable operating rules + the gate command. Read before any work. |
| [`PROGRESS.md`](PROGRESS.md) | Task checklist T0–T17, kept truthful. |
| [`OPEN_QUESTIONS.md`](OPEN_QUESTIONS.md) | Settled decisions (D-1…D-10), resolved unknowns (U-1, U-2), escalation log. |
| `reference/` | Vendored upstream, **read-only** ground truth: litestream @ `v0.5.11`, ltx @ `v0.5.1`. |
| `tests/fixtures/golden/` | Immutable byte fixtures captured from the real binary + sqlite3. |
| `scripts/` | `db_equal` equality oracle (§6.1), `capture-golden.sh`, anti-gaming `guards.sh`. |
| `src/` | The library (`wal`, `ltx`, `db`, `store`, `replica`, `replica_url`, `leaser`, `client/`). |

## The gate (must be green before any task is done)

```sh
cargo fmt --all --check
cargo clippy --all-targets --all-features -- -D warnings
cargo build --all-targets
cargo test --all
```

## License

Apache-2.0. See [`LICENSE`](LICENSE) and [`NOTICE`](NOTICE) (attributes Litestream
and the LTX format).
