#!/usr/bin/env bash
# Capture IMMUTABLE golden fixtures from the REAL litestream binary + sqlite3.
# (PLAN.md §6.2, AGENTS.md rule 3.) Run ONCE; then commit tests/fixtures/golden/.
#
# These are READER fixtures: rustyriver's parser must decode them and assert
# their fields. Writer byte-fidelity is verified by differential test D1, NOT by
# byte comparison — LTX headers embed timestamps, so bytes are not reproducible.
#
# Deterministic method: drive replication with `replicate -once` (no background
# timing/sleep races). Produces an L0-only tree (the round-trip property is what
# matters, see OPEN_QUESTIONS U-2). The binary may be an official release OR built
# from the pinned tag (reference/litestream-go) — both are "the real v0.5.11".
set -euo pipefail

REQ_VERSION="0.5.11"          # PLAN.md D-2
OUT_DIR="tests/fixtures/golden"

command -v litestream >/dev/null || { echo "litestream not on PATH"; exit 1; }
command -v sqlite3    >/dev/null || { echo "sqlite3 not on PATH"; exit 1; }

ver=$(litestream version 2>/dev/null | tr -d 'v ' || true)
case "$ver" in
  "$REQ_VERSION") : ;;                       # official release
  *"development build"*|"")
    echo "WARN: litestream reports '$ver' — assuming a from-source build of tag v$REQ_VERSION." ;;
  *) echo "ERROR: need litestream $REQ_VERSION, found '$ver'"; exit 1 ;;
esac

work=$(mktemp -d); trap 'rm -rf "$work"' EXIT
db="$work/state.db"; replica="$work/replica"
mkdir -p "$OUT_DIR"

echo "== seed a deterministic SQL sequence (WAL mode) =="
sqlite3 "$db" <<'SQL'
PRAGMA journal_mode=WAL;
CREATE TABLE kv (k TEXT PRIMARY KEY, v TEXT NOT NULL);
INSERT INTO kv VALUES ('a','1'),('b','2'),('c','3');
SQL

echo "== initial snapshot =="
litestream replicate -once "$db" "file://$replica" >/dev/null

echo "== 5 distinct transactions, each flushed (=> multiple L0 LTX files) =="
for i in $(seq 1 5); do
  sqlite3 "$db" "INSERT INTO kv VALUES ('k$i','v$i'); UPDATE kv SET v='upd$i' WHERE k='a';"
  litestream replicate -once "$db" "file://$replica" >/dev/null
done

echo "== verify the real binary restores this L0 tree (sanity) =="
litestream restore -o "$work/restored.db" "file://$replica" >/dev/null
scripts/db_equal.sh A "$db" "$work/restored.db"

echo "== capture replica tree =="
rm -rf "$OUT_DIR/replica"; cp -R "$replica" "$OUT_DIR/replica"

echo "== capture a raw SQLite WAL sample (pinned open so it isn't checkpointed) =="
python3 - "$OUT_DIR" <<'PY'
import sqlite3, shutil, sys, os
gold = sys.argv[1]; work = "/tmp/.walgen"; os.makedirs(work, exist_ok=True)
db = os.path.join(work, "w.db")
for p in (db, db+"-wal", db+"-shm"):
    if os.path.exists(p): os.remove(p)
c = sqlite3.connect(db)
c.execute("PRAGMA journal_mode=WAL"); c.execute("PRAGMA wal_autocheckpoint=0")
c.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, name TEXT, n INTEGER)")
for i in range(1, 21): c.execute("INSERT INTO t(name,n) VALUES(?,?)", (f"row{i}", i*7))
c.commit(); c.execute("UPDATE t SET n=n+1 WHERE id<=10"); c.commit()
pin = sqlite3.connect(db); pin.execute("BEGIN"); pin.execute("SELECT count(*) FROM t").fetchone()
shutil.copyfile(db+"-wal", os.path.join(gold, "sample.wal"))
pin.rollback(); pin.close(); c.close()
print("sample.wal:", os.path.getsize(os.path.join(gold, "sample.wal")), "bytes")
PY

echo "Captured into $OUT_DIR — review, then commit. (See MANIFEST.md.)"
