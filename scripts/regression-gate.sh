#!/usr/bin/env bash
#
# regression-gate.sh: did this build change what distillPDF does? One command, one verdict, against
# the behaviour frozen in accuracy/regression_gate_baseline.json. How to read the result, the
# measured runtimes and how to name a deliberate change: scripts/regression_gate/README.md.
#
#   scripts/regression-gate.sh check --fast     every suite and goalline, 13 documents, no judged-page scores
#   scripts/regression-gate.sh check --full     every suite, every goalline, judged-page floors, all 141 documents
#   scripts/regression-gate.sh freeze --owner-note "why"    replace the baseline (full mode)
#   scripts/regression-gate.sh name --run DIR --name "..." --reason "..." [--accept-drops]
#
# Options: --skip-build (test the installed wheel), --skip-suites (sweep only), --run DIR,
#          --docs a,b (sweep only these), --repeats N, --workers N.
#
# Exit: 0 identical or all changes named; 1 a quality drop, a lost document or a failed suite;
#       2 the gate could not run; 3 output changed, no drop, nobody has named it.
#
# Environment:
#   GATE_WRAP              prefix for each heavy stage. On the shared 16 GB Mac, wrap the whole
#                          run instead, since a queue of other agents makes per-stage locking
#                          wait once per stage (measured: 27 minutes for one stage):
#                            .../PDF-reader/tools/sysguard/heavy.sh scripts/regression-gate.sh check --fast
#                          Never do both: the inner wrap would wait forever on the outer lock.
#   GATE_LOCAL_ROOT        checkout holding the gitignored benchmarking/ and dev-docs/ (default:
#                          this repo's main checkout, so worktrees work)
#   GATE_HOME              venv, wheels, runs (default target/regression-gate)
#   GATE_BASELINE_OUTPUTS  frozen full outputs, for readable diffs (default $GATE_HOME/baseline-outputs)
#   BUILD_PY, UV           as in release-check.sh
#
set -uo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"
HERE="$ROOT/scripts/regression_gate"

CMD="${1:-}"
case "$CMD" in check|freeze|name) shift ;; *) sed -n 2,14p "$0"; exit 2 ;; esac

MODE=full; BUILD=1; SUITES=1; RUN=""; DOCS=""; REPEATS=""; WORKERS=4; NOTE=""; NAME_ARGS=()
while [ $# -gt 0 ]; do
  case "$1" in
    --fast) MODE=fast ;;
    --full) MODE=full ;;
    --skip-build) BUILD=0 ;;
    --skip-suites) SUITES=0 ;;
    --run) RUN="$2"; shift ;;
    --docs) DOCS="$2"; shift ;;
    --repeats) REPEATS="$2"; shift ;;
    --workers) WORKERS="$2"; shift ;;
    --owner-note) NOTE="$2"; shift ;;
    --name|--reason) NAME_ARGS+=("$1" "$2"); shift ;;
    --accept-drops) NAME_ARGS+=("$1") ;;
    *) echo "regression-gate: unknown option $1" >&2; exit 2 ;;
  esac
  shift
done

BUILD_PY="${BUILD_PY:-/opt/homebrew/bin/python3.12}"
UV="${UV:-$HOME/.local/bin/uv}"
GATE_HOME="${GATE_HOME:-$ROOT/target/regression-gate}"; export GATE_HOME
VENV="$GATE_HOME/venv"
PY="$VENV/bin/python"
WRAP="${GATE_WRAP:-}"
PYMUPDF_PIN="pymupdf==1.27.2.3"   # the reference every recall and order score is measured against

if [ -z "${GATE_LOCAL_ROOT:-}" ]; then
  GATE_LOCAL_ROOT="$(dirname "$(git rev-parse --path-format=absolute --git-common-dir)")"
fi
export GATE_LOCAL_ROOT

if [ "$CMD" = name ]; then
  [ -n "$RUN" ] || { echo "regression-gate: name needs --run DIR" >&2; exit 2; }
  exec "$PY" -I "$HERE/gate.py" name --run-dir "$RUN" ${DOCS:+--docs "$DOCS"} "${NAME_ARGS[@]}"
fi

if [ "$CMD" = freeze ]; then
  MODE=full
fi
[ -d "$GATE_LOCAL_ROOT/benchmarking/bench100" ] || {
  echo "regression-gate: CANNOT RUN: no corpus at $GATE_LOCAL_ROOT/benchmarking/bench100" >&2; exit 2; }

if [ -z "$REPEATS" ]; then
  if [ "$MODE" = fast ]; then REPEATS=1; else REPEATS=3; fi
fi
if [ -z "$RUN" ]; then
  RUN="$GATE_HOME/runs/$(date +%Y%m%d-%H%M%S)-$MODE"
fi
mkdir -p "$RUN/logs"
RUN="$(cd "$RUN" && pwd)"
STAGES="$RUN/stages.tsv"
: > "$STAGES"

# stage NAME COMMAND...: runs one stage to its log, records PASSED or FAILED, keeps going. A later
# stage's failure is still worth seeing when an earlier one has failed.
stage() {
  local name="$1"; shift
  local log="$RUN/logs/$name.log" start=$SECONDS rc
  printf '==> %s ... ' "$name"
  "$@" >"$log" 2>&1
  rc=$?
  if [ "$rc" -eq 0 ]; then
    printf 'passed (%ss)\n' "$((SECONDS - start))"
    printf '%s\tPASSED\t%s\t%s\n' "$name" "$((SECONDS - start))" "$log" >>"$STAGES"
  else
    printf 'FAILED, exit %s (%ss). Last lines of %s:\n' "$rc" "$((SECONDS - start))" "$log"
    tail -15 "$log" | cut -c1-200
    printf '%s\tFAILED\t%s\t%s\n' "$name" "$((SECONDS - start))" "$log" >>"$STAGES"
  fi
  return "$rc"
}
skipped() { printf '%s\tskipped\t0\t%s\n' "$1" "$2" >>"$STAGES"; }

ensure_venv() {
  [ -x "$PY" ] || "$UV" venv --python "$BUILD_PY" "$VENV" || return 1
  "$UV" pip install --python "$PY" -q maturin "$PYMUPDF_PIN" -r tests/requirements-dev.txt
}

build_wheel() {
  local out; out="$(mktemp -d "$GATE_HOME/wheel.XXXXXX")" || return 1
  $WRAP "$VENV/bin/maturin" build --release --features extension-module -i "$PY" --out "$out" || { rm -rf "$out"; return 1; }
  "$UV" pip install --python "$PY" -q --force-reinstall --no-deps "$out"/distillpdf-*-abi3-*.whl
  local rc=$?
  rm -rf "$out"
  return "$rc"
}

# The name `distillpdf` must resolve to this venv's wheel, not a PYTHONPATH shim or a source tree.
bound_check() {
  "$PY" -I -c '
import os, sys, sysconfig, distillpdf
real = os.path.realpath(distillpdf.__file__)
root = os.path.realpath(sysconfig.get_paths()["purelib"])
if not real.startswith(root + os.sep):
    sys.exit(f"distillpdf resolves to {real}, outside {root}")
print("bound:", distillpdf.__version__, real)'
}

# Run the release gate floors against a private copy of its state. The real gate overwrites
# dev-docs/bench/results/bench100_accuracy.json, the shared record, and locks that folder, so two
# worktrees would otherwise overwrite each other's measurement.
floors() {
  local sb="$RUN/floors-root" src="$GATE_LOCAL_ROOT/dev-docs/bench"
  mkdir -p "$sb/dev-docs/bench/results" "$sb/dev-docs/bench/out/bench100" "$sb/accuracy" || return 1
  cp -R "$src/scripts" "$sb/dev-docs/bench/scripts" || return 1
  cp "$src/results/bench100_floors.json" "$src/results/bench100_accuracy.json" "$sb/dev-docs/bench/results/" || return 1
  ln -sfn "$src/results/bench100_groundtruth" "$sb/dev-docs/bench/results/bench100_groundtruth"
  ln -sfn "$src/out/bench100/gt" "$sb/dev-docs/bench/out/bench100/gt"
  ln -sfn "$GATE_LOCAL_ROOT/benchmarking" "$sb/benchmarking"
  cp "$ROOT/accuracy/bench100_anchors.json" "$sb/accuracy/" || return 1
  $WRAP "$PY" "$sb/dev-docs/bench/scripts/bench100_gate.py" --rescore --summary
}

corpus_goalline() {
  # Runs where the corpus lives. Its out/ cache is shared and regenerated: the wrapper serialises it.
  ( cd "$GATE_LOCAL_ROOT/benchmarking/corpus_tests" && $WRAP "$PY" test_regression.py )
}

export PYO3_PYTHON="$BUILD_PY"
unset PYTHONPATH  # a shim on it would measure a different program wearing the name

stage venv ensure_venv || exit 2
if [ "$BUILD" = 1 ]; then
  stage build build_wheel || { echo "regression-gate: the wheel did not build"; exit 1; }
else
  skipped build "--skip-build: measuring the wheel already installed in $VENV"
fi
stage bound bound_check || exit 2

GT_ARGS=()
if [ "$SUITES" = 1 ]; then
  stage cargo-lib $WRAP cargo test -p distillpdf --lib
  stage pytest $WRAP "$PY" -m pytest tests/ -q
  stage corpus-goalline corpus_goalline
fi
if [ "$MODE" = full ] && [ -z "$DOCS" ]; then
  stage bench100-floors floors
  GT_ARGS=(--gt "$RUN/floors-root/dev-docs/bench/results/bench100_accuracy.json")
else
  skipped bench100-floors "$MODE mode: no gt_ scores in this run"
fi

ARGS=("$CMD" --run-dir "$RUN" --mode "$MODE" --repeats "$REPEATS" --workers "$WORKERS" --stages "$STAGES")
[ -n "$DOCS" ] && ARGS+=(--docs "$DOCS")
[ -n "$NOTE" ] && ARGS+=(--owner-note "$NOTE")
[ "${#GT_ARGS[@]}" -gt 0 ] && ARGS+=("${GT_ARGS[@]}")

start=$SECONDS
$WRAP "$PY" -I "$HERE/gate.py" "${ARGS[@]}"
rc=$?
echo "run folder: $RUN"
echo "sweep and compare: $((SECONDS - start))s; total: ${SECONDS}s"
exit "$rc"
