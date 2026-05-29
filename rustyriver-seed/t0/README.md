# rustyriver — T0 artifacts (drop-in)

Pre-baked files for **T0** (repo scaffold). Copy into the new repo root,
preserving this layout:

```
Cargo.toml
rust-toolchain.toml
.github/workflows/ci.yml
scripts/capture-golden.sh
scripts/guards.sh
```

Notes for whoever seeds the repo:
- **Versions are plausible-as-of-seed, not gospel.** Run `cargo update` and bump
  pins where needed; comments flag the ones most likely to drift
  (`rusqlite`, `object_store`, `thiserror`, the Rust channel).
- **Confirm the litestream release asset filename** in `ci.yml` and
  `capture-golden.sh` for `v0.5.11` (the `*-linux-amd64.tar.gz` name).
- `capture-golden.sh` must be run **once** on a machine with the real
  `litestream v0.5.11` + `sqlite3`; commit the resulting `tests/fixtures/golden/`.
- CI (`ci.yml`) is the §6.5 referee: full gate from a clean checkout, MinIO +
  real litestream provisioned, plus `guards.sh` anti-gaming checks.
