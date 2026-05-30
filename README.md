# rustyriver

A litestream like-library written in Rust.

## Contents
| File | Role |
|------|------|
| `CLAUDE.md` | Non-negotiable guardrails + the gate command. The agent's operating manual. |
| `PROGRESS.md` | Task checklist (T0–T17), waves, and gates G1–G5. Kept truthful by the agent. |
| `OPEN_QUESTIONS.md` | Settled decisions (D-1…D-10) + known unknowns (U-1, U-2) + escalations log. |
| `README.md` | This file. |
| `PLAN.md` | the full spec |

## Setup (Remove this list once it is going)
1. Vendor upstream into `reference/litestream-go/` at tag **`v0.5.11`** (read-only).
2. Add the CI workflow described in `PLAN.md` §6.5 (clean-checkout gate; MinIO
   service; installs the `litestream v0.5.11` binary for differential jobs;
   fails on `todo!`/`unimplemented!`/`#[ignore]`/`assert!(true)` in `src/` and on
   a golden fixture changed alongside `src/`).
3. Run `scripts/capture-golden.sh` once (needs the real binary) and **commit** the
   fixtures under `tests/fixtures/golden/`.
4. Configure the agent tool allowlist for `cargo`, `git`, `docker`/MinIO, and the
   `litestream` binary (`PLAN.md` §7.3).

- **Versions are plausible-as-of-seed, not gospel.** Run `cargo update` and bump
  pins where needed; comments flag the ones most likely to drift
  (`rusqlite`, `object_store`, `thiserror`, the Rust channel).
- **Confirm the litestream release asset filename** in `ci.yml` and
  `capture-golden.sh` for `v0.5.11` (the `*-linux-amd64.tar.gz` name).
- `capture-golden.sh` must be run **once** on a machine with the real
  `litestream v0.5.11` + `sqlite3`; commit the resulting `tests/fixtures/golden/`.
- CI (`ci.yml`) is the §6.5 referee: full gate from a clean checkout, MinIO +
  real litestream provisioned, plus `guards.sh` anti-gaming checks.


## Pre-launch human checklist
- [ ] Allowlist configured so subagents don't stall on permission prompts.
- [ ] CI can run the pinned `litestream` binary **and** MinIO (D-3).
- [ ] Golden vectors captured and committed.

The agent works wave-by-wave, stopping at any gate or any
correctness-critical ambiguity it logs in `OPEN_QUESTIONS.md`.
