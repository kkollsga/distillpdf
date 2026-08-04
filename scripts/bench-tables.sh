#!/usr/bin/env bash
# Run the pinned scorer attacks and table-only performance measurement.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
RUN_PY="${RUN_PY:-$HOME/labenv/bin/python}"
WHEEL="${WHEEL:?set WHEEL to the exact release wheel installed in RUN_PY}"

cd "$ROOT"
export PYTHONHASHSEED="${PYTHONHASHSEED:-0}"
export RAYON_NUM_THREADS="${RAYON_NUM_THREADS:-1}"
if [ -z "${TABLE_CORPUS_ROOT:-}" ]; then
  TABLE_CORPUS_ROOT="$($RUN_PY - <<'PY'
import importlib.metadata as m
print(m.distribution("pdf-parse-bench").locate_file(
    "pdf_parse_bench/data/2026-q1-tables-only"))
PY
)"
  export TABLE_CORPUS_ROOT
fi

"$RUN_PY" -m pytest tests/test_table_corpus.py \
  -q -k 'scorer_rejects or optional_geometry_and_span_attacks'

# Isolate the measured DistillPDF install from the source tree and from any older wheel in
# RUN_PY. Comparator packages remain inherited from the explicitly selected pinned run env.
mkdir -p dev-docs/bench/out/table-analysis
BENCH_ENV="$(mktemp -d dev-docs/bench/out/table-analysis/env.XXXXXX)"
"$RUN_PY" -m venv --system-site-packages "$BENCH_ENV"
BENCH_PY="$BENCH_ENV/bin/python"
"$BENCH_PY" -m pip install --quiet --no-deps --force-reinstall "$WHEEL"
if [[ " $* " != *" --limit "* && " $* " != *" --limit="* ]]; then
  COMPARATOR_WHEEL_DIR="${COMPARATOR_WHEEL_DIR:?full evidence requires the pinned wheels in COMPARATOR_WHEEL_DIR}"
  "$BENCH_PY" -m pip install --quiet --no-deps "$COMPARATOR_WHEEL_DIR"/*.whl
  "$BENCH_PY" dev-docs/bench/scripts/table_content_metric.py --self-test
else
  "$RUN_PY" dev-docs/bench/scripts/table_content_metric.py --self-test
fi
"$BENCH_PY" tests/table_measurement/runner.py --wheel "$WHEEL" "$@"
