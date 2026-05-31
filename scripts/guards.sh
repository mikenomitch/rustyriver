#!/usr/bin/env bash
# Anti-gaming guards (PLAN.md §6.5 / AGENTS.md rules 1-3).
#   1) No stub/skip patterns in src/ or tests/ (reference/ is exempt).
#   2) Golden fixtures must not change in the same commit/PR as src/.
# Usage: scripts/guards.sh [BASE_REF]
set -euo pipefail

fail=0

echo "== Guard 1: forbidden stub/skip patterns =="
# Note: phrase legitimate prose differently — these literals are banned in code.
pattern='todo!|unimplemented!|panic!\("not implemented|#\[ignore\]|assert!\(true\)'
if grep -REn --include='*.rs' "$pattern" src tests 2>/dev/null; then
  echo "ERROR: forbidden pattern above (AGENTS.md rules 1-2)."
  fail=1
else
  echo "ok"
fi

echo "== Guard 2: golden fixtures not edited alongside src/ =="
base="${1:-}"
if [ -n "$base" ] && git rev-parse --verify "$base" >/dev/null 2>&1; then
  changed=$(git diff --name-only "$base"...HEAD)
else
  changed=$(git diff --name-only HEAD~1...HEAD 2>/dev/null || true)
fi
touched_golden=$(echo "$changed" | grep -E '^tests/fixtures/golden/' || true)
touched_src=$(echo "$changed"    | grep -E '^src/' || true)
if [ -n "$touched_golden" ] && [ -n "$touched_src" ]; then
  echo "ERROR: golden fixtures changed in the same change as src/ — fixtures are immutable (AGENTS.md rule 3)."
  echo "golden touched:"; echo "$touched_golden"
  fail=1
else
  echo "ok"
fi

exit "$fail"
