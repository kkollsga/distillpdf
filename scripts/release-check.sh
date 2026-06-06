#!/usr/bin/env bash
#
# release-check.sh — the full local verification chain to run BEFORE a release.
#
# CI (.github/workflows/ci.yml) runs the parts that work on a fresh clone:
#   - cargo test --lib            (pure-logic Rust unit tests)
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

step "Rust unit tests (cargo test --lib)"
PYO3_PYTHON="$BUILD_PY" cargo test --lib

step "Clippy (lib, warnings allowed but surfaced)"
PYO3_PYTHON="$BUILD_PY" cargo clippy --lib 2>&1 | tail -3 || true

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

step "All checks passed — safe to bump + push (with the user's go-ahead)."
