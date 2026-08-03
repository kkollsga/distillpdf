#!/usr/bin/env bash
#
# release-check.sh — the full local verification chain to run BEFORE a release.
#
# CI (.github/workflows/ci.yml) runs the parts that work on a fresh clone:
#   - cargo test -p distillpdf --lib   (pure-logic Rust unit tests, pyo3-free core)
#   - pytest tests/               (self-contained, self-generated fixtures)
# It deliberately does NOT run the corpus regression gate: that gate scores real
# third-party PDFs that are license-encumbered and cannot be published, so the
# whole benchmarking/ tree is gitignored and exists only on a maintainer's
# machine. This script is the missing piece — it runs the CI checks PLUS the
# local corpus gate, so "is this safe to release?" is one reproducible command.
#
# The corpus gate is the real regression signal across 25+ real-world documents
# (37 metrics, each held at-or-above a frozen baseline). Treat a green run here,
# not a green CI badge, as the release gate.
#
# Usage:
#   scripts/release-check.sh
#
# Override the interpreters if your setup differs from the defaults:
#   BUILD_PY=/opt/homebrew/bin/python3.12   # builds the wheel (pyo3 0.25 needs <=3.12)
#   RUN_PY=~/labenv/bin/python              # runs pytest + the gate (abi3 wheel: any >=3.8)
#   UV=~/.local/bin/uv                      # uv, for venv + installs
#
set -euo pipefail

cd "$(dirname "$0")/.."

BUILD_PY="${BUILD_PY:-/opt/homebrew/bin/python3.12}"
RUN_PY="${RUN_PY:-$HOME/labenv/bin/python}"
UV="${UV:-$HOME/.local/bin/uv}"
BUILD_VENV="${BUILD_VENV:-.build-venv}"

step() { printf '\n\033[1;34m==> %s\033[0m\n' "$1"; }

step "Rust unit tests (cargo test -p distillpdf --lib)"
# Scope to the pyo3-free core; a bare workspace `--lib` would also pull in
# distillpdf-python's lib tests, which need libpython linked.
PYO3_PYTHON="$BUILD_PY" cargo test -p distillpdf --lib

step "Clippy (core lib, warnings allowed but surfaced)"
PYO3_PYTHON="$BUILD_PY" cargo clippy -p distillpdf --lib 2>&1 | tail -3 || true

step "Build release wheel (maturin, $BUILD_PY)"
if [ ! -x "$BUILD_VENV/bin/maturin" ]; then
  "$UV" venv --python "$BUILD_PY" "$BUILD_VENV"
  "$UV" pip install --python "$BUILD_VENV/bin/python" maturin
fi
"$BUILD_VENV/bin/maturin" build --release --features extension-module -i "$BUILD_PY"

WHEEL="$(ls -t target/wheels/distillpdf-*-abi3-*.whl | head -1)"
echo "built: $WHEEL"

step "Install wheel into run env ($RUN_PY)"
"$UV" pip install --python "$RUN_PY" --force-reinstall --no-deps -q "$WHEEL"

# Everything below measures "distillpdf" — but nothing so far has checked that the name
# resolves to the wheel we just built. `PYTHONPATH=/tmp/fake` with a stub package scored
# through the whole accuracy gate without a complaint. So bind the name to this
# interpreter's site-packages, once, loudly, before any of it runs.
"$RUN_PY" - <<'PY'
import os, site, sys, sysconfig
import distillpdf
real = os.path.realpath(distillpdf.__file__)
roots = {os.path.realpath(p) for p in
         (sysconfig.get_paths()["purelib"], sysconfig.get_paths()["platlib"],
          *site.getsitepackages())}
if not any(real == r or real.startswith(r + os.sep) for r in roots):
    sys.exit(f"FAILED — distillpdf resolves to {real}, outside the site-packages of "
             f"{sys.executable} ({sorted(roots)}). That is a PYTHONPATH shim or a source "
             f"tree, not the wheel just built: every test and gate below would be measuring "
             f"a different program wearing the name.")
print(f"bound: distillpdf {distillpdf.__version__} at {real}")
PY

step "Owned test suite (pytest tests/ — CI parity)"
"$RUN_PY" -m pytest tests/ -q

step "Corpus regression gate (THE release gate)"
if [ -f benchmarking/corpus_tests/test_regression.py ]; then
  ( cd benchmarking/corpus_tests && "$RUN_PY" test_regression.py )
else
  echo "SKIPPED — benchmarking/corpus_tests/ not present on this machine."
  echo "         The corpus is local-only (gitignored, license-encumbered)."
  echo "         A release must NOT proceed without a green gate run."
  exit 2
fi

step "Accuracy floor gate (per-table-type x per-dimension, bench100)"
# The 37-metric corpus gate above is a *mechanical* gate: it holds our own extraction
# counters at-or-above a frozen baseline. It cannot see the thing that matters most here —
# that a change trades one table population for another. Measured over 84 ground-truth
# table pages we score 0.529 against pymupdf's 0.036 on booktabs (horizontal rules only)
# and 0.412 against its 0.688 on full ruled grids; a looser detector wins the grids,
# invents tables on papers, and leaves the aggregate almost unmoved. The floor gate holds
# every table-type x dimension cell at its own floor and names the pages that moved.
#
# Those table numbers are a GRID score, not a detection count: since 2026-08-03 the gated
# `tables` accuracy is 0.5 * table-count agreement + 0.5 * row/col agreement on matched
# tables. It has to be. A count-only floor cannot see a change that keeps every <table>
# and halves its columns — measured, that attack moves the count-only score by 0.0000 and
# the current one by -0.053, three times the aggregate cell's whole slack budget. The
# count-only value is still printed per table type (--summary, "TABLES, LAYERED") so the
# detection trendline survives the change.
#
# It re-scores first (~65 s over 256 pages / 88 docs) because this script has just
# reinstalled the wheel: scoring the shipped build is the whole point, and the gate refuses
# to run on measurements older than the installed module. Half that time is one extra
# extraction of each document IN FULL: every page above is scored from a one-page slice,
# which cannot suppress repeated page furniture, and the gap between the two is reported
# (--summary, "SLICE vs FULL DOCUMENT") so a fix phase is scoped against the product's
# residue rather than the harness's artefact — for headings that is a false-positive rate
# of 0.222, not the 0.815 the slices show.
#
# Same local-corpus caveat as above — bench100 lives under the gitignored benchmarking/
# tree with its ground truth in dev-docs/. The ONLY sanctioned skip is a machine with no
# benchmarking/ tree AT ALL, and that machine has already exited 2 at the corpus gate
# above. Everything else is a hard failure, because the failure modes are identical from
# here: "the floors file is gone" and "the floors file says we are green" cannot be told
# apart by a step that treats a missing baseline as a skip. Deleting the baseline is not a
# way to pass this script — and neither is `mv benchmarking/bench100 /tmp`, which used to
# print SKIPPED, then "All checks passed", then exit 0. The justification printed there
# ("the corpus gate above already blocks a release from such a machine") was FALSE: that
# gate reads benchmarking/corpus_tests/, a different directory, and a machine can have one
# without the other.
GATE="dev-docs/bench/scripts/bench100_gate.py"
FLOORS="dev-docs/bench/results/bench100_floors.json"
ANCHORS="accuracy/bench100_anchors.json"
if [ -d benchmarking/bench100 ]; then
  missing=""
  [ -f "$GATE" ]    || missing="$missing\n           - the gate script $GATE"
  [ -f "$FLOORS" ]  || missing="$missing\n           - the frozen floors $FLOORS"
  [ -f "$ANCHORS" ] || missing="$missing\n           - the committed anchors $ANCHORS"
  "$RUN_PY" -c "import fitz" 2>/dev/null \
                    || missing="$missing\n           - PyMuPDF (fitz) in $RUN_PY"
  if [ -n "$missing" ]; then
    echo "FAILED — the bench100 corpus is present, so this gate MUST run, but it cannot."
    printf '         missing:%b\n' "$missing"
    echo "         Restore it — from git ($ANCHORS is committed), an archived predecessor"
    echo "         (dev-docs/bench/results/bench100_floors_*.json), or a signed re-baseline:"
    echo "           $GATE --rescore --rebaseline --owner-note \"<why>\""
    echo "         Deleting the baseline is not a skip. A release must NOT proceed."
    exit 2
  fi
  # set -e makes a non-zero gate exit (1 breached, 2 could not run) fail the script here.
  "$RUN_PY" "$GATE" --rescore --summary
elif [ ! -d benchmarking ]; then
  echo "SKIPPED — there is no benchmarking/ tree on this machine at all."
  echo "         The corpora are local-only (gitignored, license-encumbered)."
  echo "         (Unreachable in practice: the corpus gate above already exited 2.)"
  echo "         Behaviour phases MUST run it: $GATE --rescore"
else
  echo "FAILED — benchmarking/ exists on this machine but benchmarking/bench100/ does not."
  echo "         A corpus that can be moved aside to turn a red gate into 'All checks"
  echo "         passed' is not a gate. The bench100 corpus is part of this machine's"
  echo "         release capability, so its absence here is a broken setup, not a skip:"
  echo "         restore benchmarking/bench100/ (88 documents, digest recorded in"
  echo "         $ANCHORS). A release must NOT proceed."
  exit 2
fi

step "Table-corpus source-fidelity parity gate (the honesty deliverable)"
# The table torture corpus (tests/table_corpus/, 74 self-generated PDFs / 72 tables) is the
# CELL-TRUTH gate, and it runs in CI with the rest of pytest: its truth is exact and
# committed, which is the one thing bench100's judged truth can never be.
#
# What CI cannot run is this: the corpus's own honesty check. A generated corpus's failure
# mode is being CLEANER than reality — a suite of tables we pass says nothing unless each
# mimics a real page faithfully enough to fail the same way. So every case names a real
# source page in the local bench100 corpus, and this step scores the extractor on the
# generated PDF and on that page WITH THE SAME FUNCTION (bench100_score.table_score).
# A case is oversimplified iff C_gen - C_real > 0.15; harder than reality always passes.
#
# `C` is the position-blind metric — the one that reads 1.0 when the grid is right and every
# value sits under the wrong header. It is used here only because it is the one metric
# computable on real pages, where no cell truth exists. Parity on C is NECESSARY, NOT
# SUFFICIENT; the cell-positional gates in tests/test_table_corpus.py remain the measurement.
#
# STANDING RULE (spec §10.1.2): this corpus gates REGRESSION and CELL-LEVEL CORRECTNESS;
# bench100 gates WILD CAPABILITY. **A change that raises torture-corpus scores while dropping
# any bench100 floor above is a FAILED change**, not a trade-off. The two are mutually
# load-bearing on purpose.
#
# An unexplained OVERSIMPLIFIED row exits non-zero and blocks the release: it means a
# torture-corpus number is being cited that the real page does not support. An oversimplified
# case that CANNOT be enriched keeps its flag in truth.json, is excluded from the coverage
# claim for its type, and is listed with its reason in the report — that is a disposition, not
# a pass.
if [ -d benchmarking/bench100 ]; then
  "$RUN_PY" tests/gen_table_corpus.py --parity
  echo "report: dev-docs/bench/results/table_corpus_parity.md"
  # The parity run regenerates the corpus; a diff here means the committed bytes and the
  # generator have parted company, which the CI drift check would also catch.
  if ! git diff --quiet -- tests/table_corpus; then
    echo "FAILED — the parity run's regeneration changed tests/table_corpus/."
    echo "         Commit the regenerated corpus and its truth together, or fix the"
    echo "         non-determinism. See scripts/check-fixture-drift.sh."
    exit 2
  fi
else
  echo "SKIPPED — no benchmarking/bench100 on this machine, so no real page to score against."
  echo "         (Unreachable in practice: the gate above already exited 2.)"
fi

step "All checks passed — safe to bump + push (with the user's go-ahead)."
