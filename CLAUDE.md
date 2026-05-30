# rustyriver

A from-scratch Rust reimplementation of **Litestream v0.5** (`v0.5.11`) as an
embeddable library: streaming SQLite replication to object storage, point-in-time
restore, and object-storage lease fencing.

**Before doing any work, read these in order:**
1. **`AGENTS.md`** — the non-negotiable operating manual (correctness honesty, the
   gate, scope, escalation rules). These rules override convenience, always.
2. **`PLAN.md`** — the full spec: scope (§2), architecture (§4), task DAG (§5),
   verification strategy (§6), gates G1–G5.
3. **`PROGRESS.md`** — current task status (keep it truthful).
4. **`OPEN_QUESTIONS.md`** — settled decisions + the escalation log.

`reference/` holds the vendored upstream (litestream @ v0.5.11, ltx @ v0.5.1),
**read-only** — it is the only source of "expected" behavior alongside the golden
fixtures under `tests/fixtures/golden/`.

The gate that must be green before any task is done:
```sh
cargo fmt --all --check
cargo clippy --all-targets --all-features -- -D warnings
cargo build --all-targets
cargo test --all
```
