#!/usr/bin/env bash
# Dev loop: build distillpdf into the local venv, then run the test suite (pytest).
#
# The PDF corpora under tests/corpus*/ are gitignored (third-party docs) and exist only
# on a local checkout, so the full suite runs here; CI runs only the tracked-data tests
# (see tests/conftest.py). Competitor head-to-head benchmarks live in the gitignored
# ../benchmarking/ folder. Pass --no-build to skip the rebuild.
set -e
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
VENV="$ROOT/.venv"
PY="$VENV/bin/python"

[ -d "$VENV" ] || python3 -m venv "$VENV"

if [ "${1:-}" != "--no-build" ]; then
  # Build a wheel and force-reinstall it (NOT `maturin develop`: its editable layout can
  # leave a stale .so loaded so source edits silently don't take effect). Wheel path is
  # reproducible via the latest-mtime glob.
  "$VENV/bin/pip" -q install maturin pytest >/dev/null 2>&1 || true
  ( cd "$ROOT/distillpdf" \
    && VIRTUAL_ENV="$VENV" "$VENV/bin/maturin" build --release 2>&1 | grep -iE "error|Built wheel" )
  WHEEL="$(ls -t "$ROOT/distillpdf/target/wheels/"*.whl | head -1)"
  "$VENV/bin/pip" install -q --force-reinstall --no-deps "$WHEEL" >/dev/null
  "$PY" -c "import distillpdf" || { echo "distillpdf import failed after build"; exit 1; }
fi

exec "$PY" -m pytest "$ROOT/tests" -q
