# rustyriver

A litestream like-library written in Rust.

## Contents
| File | Role |
|------|------|
| `AGENTS.md` | Non-negotiable guardrails + the gate command. The agent's operating manual. |
| `PROGRESS.md` | Task checklist (T0–T17), waves, and gates G1–G5. Kept truthful by the agent. |
| `OPEN_QUESTIONS.md` | Settled decisions (D-1…D-10) + known unknowns (U-1, U-2) + escalations log. |
| `README.md` | This file. |

`PLAN.md` is the full spec — it lives one level up as
`docs/plans/litestream-rust-fork-plan.md`. **Copy it into the new repo root as
`PLAN.md`** when seeding (kept separate here to avoid duplicating the large doc).

## Seeding the new repo
1. `git init rustyriver` (Apache-2.0 license + `NOTICE` attributing Litestream — D-8).
2. Copy `AGENTS.md`, `PROGRESS.md`, `OPEN_QUESTIONS.md` to the root.
3. Copy `litestream-rust-fork-plan.md` → `PLAN.md` at the root.
4. Vendor upstream into `reference/litestream-go/` at tag **`v0.5.11`** (read-only).
5. Add the CI workflow described in `PLAN.md` §6.5 (clean-checkout gate; MinIO
   service; installs the `litestream v0.5.11` binary for differential jobs;
   fails on `todo!`/`unimplemented!`/`#[ignore]`/`assert!(true)` in `src/` and on
   a golden fixture changed alongside `src/`).
6. Run `scripts/capture-golden.sh` once (needs the real binary) and **commit** the
   fixtures under `tests/fixtures/golden/`.
7. Configure the agent tool allowlist for `cargo`, `git`, `docker`/MinIO, and the
   `litestream` binary (`PLAN.md` §7.3).

## Pre-launch human checklist
- [ ] All `OPEN_QUESTIONS.md` decisions confirmed (they reflect `PLAN.md` §0).
- [ ] CI can run the pinned `litestream` binary **and** MinIO (D-3).
- [ ] Golden vectors captured and committed.
- [ ] Allowlist configured so subagents don't stall on permission prompts.

Then start the run (e.g. a dynamic workflow per `PLAN.md` §7, saved as
`/port-rustyriver`). The agent works wave-by-wave, stopping at any gate or any
correctness-critical ambiguity it logs in `OPEN_QUESTIONS.md`.
