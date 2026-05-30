#!/usr/bin/env bash
# The equality oracle (PLAN.md §6.1): is a restored DB correct vs. a source DB?
#
#   db_equal.sh A <db1> <db2>   # logical equality (default, version-robust)
#   db_equal.sh B <db1> <db2>   # physical equality (byte-identical main file)
#
# Oracle A: both pass PRAGMA integrity_check; identical sqlite_master schema;
#           identical per-table content hash (rows ordered by every column).
# Oracle B: after a TRUNCATE checkpoint, the main DB files are byte-identical
#           (only valid when both were produced by the same SQLite version).
#
# Exit 0 = equal; non-zero = not equal (reason printed to stderr).
set -euo pipefail

mode="${1:?usage: db_equal.sh <A|B> <db1> <db2>}"
db1="${2:?missing db1}"
db2="${3:?missing db2}"
command -v sqlite3 >/dev/null || { echo "sqlite3 not on PATH" >&2; exit 2; }

sha() { shasum -a 256 "$1" 2>/dev/null | awk '{print $1}'; }

integrity() {
  local out; out=$(sqlite3 "$1" 'PRAGMA integrity_check;' 2>&1 || true)
  [ "$out" = "ok" ] || { echo "integrity_check failed for $1: $out" >&2; return 1; }
}

schema_hash() {
  sqlite3 "$1" \
    "SELECT type,name,tbl_name,sql FROM sqlite_master
     WHERE name NOT LIKE 'sqlite_%' ORDER BY type,name,tbl_name;" | shasum -a 256 | awk '{print $1}'
}

# Hash of all user-table content, each table's rows ordered by every column so
# the digest is independent of physical row/page placement.
content_hash() {
  local db="$1" t cols
  : > /tmp/.dbeq_content.$$
  while IFS= read -r t; do
    [ -z "$t" ] && continue
    cols=$(sqlite3 "$db" "SELECT group_concat('\"'||name||'\"', ',')
                          FROM pragma_table_info('$t');")
    {
      echo "## TABLE $t"
      sqlite3 -noheader -newline $'\n' "$db" \
        ".mode list" \
        "SELECT * FROM \"$t\" ORDER BY ${cols:-1};"
    } >> /tmp/.dbeq_content.$$
  done < <(sqlite3 "$db" "SELECT name FROM sqlite_master
                          WHERE type='table' AND name NOT LIKE 'sqlite_%' ORDER BY name;")
  shasum -a 256 /tmp/.dbeq_content.$$ | awk '{print $1}'
  rm -f /tmp/.dbeq_content.$$
}

case "$mode" in
  A|a)
    integrity "$db1"; integrity "$db2"
    s1=$(schema_hash "$db1"); s2=$(schema_hash "$db2")
    [ "$s1" = "$s2" ] || { echo "schema differs ($s1 != $s2)" >&2; exit 1; }
    c1=$(content_hash "$db1"); c2=$(content_hash "$db2")
    [ "$c1" = "$c2" ] || { echo "content differs ($c1 != $c2)" >&2; exit 1; }
    echo "OK (Oracle A: schema=$s1 content=$c1)"
    ;;
  B|b)
    for d in "$db1" "$db2"; do
      sqlite3 "$d" 'PRAGMA wal_checkpoint(TRUNCATE);' >/dev/null 2>&1 || true
    done
    h1=$(sha "$db1"); h2=$(sha "$db2")
    [ "$h1" = "$h2" ] || { echo "bytes differ ($h1 != $h2)" >&2; exit 1; }
    echo "OK (Oracle B: sha256=$h1)"
    ;;
  *) echo "unknown mode '$mode' (want A or B)" >&2; exit 2 ;;
esac
