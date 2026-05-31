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

echo "== Guard 2: existing golden fixtures not modified/deleted alongside src/ =="
base="${1:-}"
if [ -n "$base" ] && git rev-parse --verify "$base" >/dev/null 2>&1; then
  range="$base...HEAD"
else
  range="HEAD~1...HEAD"
fi
# ADDING captured fixtures (e.g. the initial import that introduces both the
# golden vectors and src/, or a later legitimate capture) is allowed. The
# immutability rule (AGENTS.md rule 3) protects EXISTING fixtures from being
# edited/deleted/renamed to match buggy code — so flag only Modified, Deleted,
# or Renamed golden files (--diff-filter=MDR), never plain Additions.
touched_golden=$(git diff --name-status --diff-filter=MDR "$range" -- 'tests/fixtures/golden/' 2>/dev/null || true)
touched_src=$(git diff --name-only "$range" -- 'src/' 2>/dev/null || true)
if [ -n "$touched_golden" ] && [ -n "$touched_src" ]; then
  echo "ERROR: existing golden fixtures modified/deleted in the same change as src/ — fixtures are immutable (AGENTS.md rule 3)."
  echo "golden touched:"; echo "$touched_golden"
  fail=1
else
  echo "ok"
fi

exit "$fail"
