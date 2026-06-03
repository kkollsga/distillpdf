#!/usr/bin/env bash
# Dev loop: build distillpdf into the local venv and run the goallines.
# Needs ONLY distillpdf — goallines read frozen fixtures (header_groundtruth*.json,
# corpus_tables/ground_truth.json, fixtures/*.pymupdf_plain.txt) plus the local PDF
# corpora (bench/corpus*/, gitignored). No competitor tools required; head-to-head
# comparison lives in the gitignored ../benchmarking/ folder.
set -e
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
VENV="$ROOT/.venv"
PY="$VENV/bin/python"

[ -d "$VENV" ] || python3 -m venv "$VENV"

# Rebuild + install distillpdf into the venv when sources changed. We build a wheel
# and force-reinstall it rather than `maturin develop`: develop's editable layout
# can fail to land in this venv's site-packages, leaving a stale .so loaded (so
# source edits silently don't take effect). The wheel path is reproducible.
if [ "${1:-}" != "--no-build" ]; then
  "$VENV/bin/pip" -q install maturin >/dev/null 2>&1 || true
  ( cd "$ROOT/distillpdf" \
    && VIRTUAL_ENV="$VENV" "$VENV/bin/maturin" build --release 2>&1 | grep -iE "error|Built wheel" )
  WHEEL="$(ls -t "$ROOT/distillpdf/target/wheels/"*.whl | head -1)"
  "$VENV/bin/pip" install -q --force-reinstall --no-deps "$WHEEL" >/dev/null
  # Fail loudly if the freshly built module won't import.
  "$PY" -c "import distillpdf" || { echo "distillpdf import failed after build"; exit 1; }
fi

cd "$ROOT/bench"

# Refresh distillpdf's HTML output in out/ so it always reflects the current build.
"$PY" generate.py

fail=0
for t in test_headers test_tables test_html test_links; do
  echo "=== $t ==="
  "$PY" "$t.py" | tail -4 || fail=1
done
exit $fail
