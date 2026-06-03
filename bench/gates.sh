#!/usr/bin/env bash
# Goal & regression gates (see GOALS.md).
#   REGRESSION gates must stay GREEN — a failure exits non-zero (fails the build).
#   GOAL gates are TDD targets in flight — reported, but do NOT fail the build
#   (they start red and turn green as a phase lands).
# Needs only distillpdf (fixtures are frozen). Run from anywhere.
set -u
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
PY="${PY:-$ROOT/.venv/bin/python}"
[ -x "$PY" ] || PY=python3
cd "$ROOT/bench"

"$PY" generate.py >/dev/null 2>&1 || true   # refresh out/html so artifacts track the build

echo "=================  REGRESSION GATES (must stay green)  ================="
reg_fail=0
for t in test_headers test_tables test_html test_links test_content; do
  if "$PY" "$t.py" >/tmp/gate_$t.out 2>&1; then
    echo "  ✅ $t"
  else
    echo "  ❌ $t   (see /tmp/gate_$t.out)"; reg_fail=1
  fi
done
# Ratcheted fidelity checks (LOCKED set in test_fidelity.py) are build-breaking.
if "$PY" test_fidelity.py --locked >/tmp/gate_test_fidelity_locked.out 2>&1; then
  echo "  ✅ test_fidelity(locked) — $(grep -m1 'locked fidelity' /tmp/gate_test_fidelity_locked.out | sed 's/^ *//')"
else
  echo "  ❌ test_fidelity(locked)   (see /tmp/gate_test_fidelity_locked.out)"; reg_fail=1
fi

echo "=================  GOAL GATES (turn green as phases land)  ============="
for g in test_regions test_word_spans test_tables_real test_fidelity; do
  [ -f "$g.py" ] || continue
  if "$PY" "$g.py" >/tmp/gate_$g.out 2>&1; then
    echo "  ✅ $g"
  else
    echo "  🔴 $g   (in progress — $(grep -m1 'need ' /tmp/gate_$g.out | sed 's/^ *//'))"
  fi
done

echo "======================================================================="
if [ "$reg_fail" -ne 0 ]; then
  echo "REGRESSION FAILURE — build is red."; exit 1
fi
echo "Regression gates green."
