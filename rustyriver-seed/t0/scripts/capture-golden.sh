#!/usr/bin/env bash
# Capture IMMUTABLE golden fixtures from the REAL litestream binary + sqlite3.
# Run ONCE on a machine with litestream v0.5.11 and sqlite3; then commit the
# resulting tests/fixtures/golden/. (PLAN.md §6.2, AGENTS.md rule 3.)
#
# These are READER fixtures: rustyriver's parser must decode them and assert
# their fields. Writer byte-fidelity is verified by differential test D1, NOT by
# byte comparison — LTX headers embed timestamps, so the bytes are not
# reproducible by a later re-run.
set -euo pipefail

REQ_VERSION="0.5.11"          # PLAN.md D-2
OUT_DIR="tests/fixtures/golden"

command -v litestream >/dev/null || { echo "litestream not on PATH"; exit 1; }
command -v sqlite3    >/dev/null || { echo "sqlite3 not on PATH"; exit 1; }

ver=$(litestream version 2>/dev/null | tr -d 'v ')
if [ "$ver" != "$REQ_VERSION" ]; then
  echo "ERROR: need litestream $REQ_VERSION, found '$ver'"; exit 1
fi

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
db="$work/state.db"
replica="$work/replica"       # file-replica root
mkdir -p "$OUT_DIR"

echo "== seeding a deterministic SQL sequence =="
sqlite3 "$db" <<'SQL'
PRAGMA journal_mode=WAL;
CREATE TABLE kv (k TEXT PRIMARY KEY, v TEXT NOT NULL);
INSERT INTO kv VALUES ('a','1'),('b','2'),('c','3');
SQL

echo "== replicating with the real litestream (background) =="
# NOTE: adjust the replica arg if the pinned CLI expects a different form.
litestream replicate "$db" "$replica" &
ls_pid=$!
sleep 2

echo "== generating several distinct transactions (=> multiple L0 LTX files) =="
for i in $(seq 1 5); do
  sqlite3 "$db" "INSERT INTO kv VALUES ('k$i','v$i'); UPDATE kv SET v='upd$i' WHERE k='a';"
  sleep 1.2
done
sleep 2
kill "$ls_pid" 2>/dev/null || true
wait "$ls_pid" 2>/dev/null || true

echo "== capturing fixtures =="
# Raw WAL sample for wal.rs (T1).
[ -f "$db-wal" ] && cp "$db-wal" "$OUT_DIR/sample.wal" || echo "(no -wal present)"
# Full replica tree (LTX files + snapshot) verbatim, for ltx.rs (T2).
rm -rf "$OUT_DIR/replica"
cp -R "$replica" "$OUT_DIR/replica"

cat > "$OUT_DIR/MANIFEST.md" <<EOF
# Golden fixtures — provenance (IMMUTABLE)

Captured by scripts/capture-golden.sh from the REAL litestream v$REQ_VERSION.
DO NOT edit or regenerate from rustyriver. A mismatch means rustyriver is wrong
(AGENTS.md rule 3).

- replica/    : full file-replica tree litestream produced (LTX files + snapshot).
- sample.wal  : a raw SQLite WAL sample for wal.rs parser tests.

SQL sequence: WAL mode; create kv(k,v); seed 3 rows; then 5 iterations each
inserting k\$i/v\$i and updating row 'a', ~1.2s apart, yielding multiple L0 LTX
files (no higher compaction).

Reader tests decode these and assert structural fields (txids, page numbers,
checksums). Writer fidelity is covered by differential D1 (PLAN.md §6.3), not by
byte comparison against these files.
EOF

echo "Captured into $OUT_DIR — review, then commit."
