#!/usr/bin/env bash
# Assert that the committed fixtures are exactly what the generators produce.
#
# Every PDF under tests/{fixtures_pdf,corpus_tables,demo} is self-generated and committed,
# and the suite asserts against the COMMITTED bytes — so a generator that has drifted from
# them means the tests are guarding a file nothing can reproduce. This is the check that
# would have caught it.
#
# The regeneration runs in a temp copy at a DIFFERENT path on purpose: reportlab digests
# the image path string into the image XObject's name, so a generator that leaks its
# checkout directory into the PDF fails here and passes an in-place rerun.
#
# The AES fixtures (tests/fixtures_pdf/encrypted/{aes_128,aes_256,inline_encrypt_aes_128,
# inline_encrypt_userpw}.pdf) cannot be byte-deterministic — qpdf draws a random AES IV per
# object with no seed hook — so gen_fixtures.py rewrites them only when their DECRYPTED
# content changes (see AES_NONDETERMINISTIC there). The committed copies are seeded into the
# temp tree so that quarantine applies here too; a real content change still shows up.
#
# Reproducibility verified beyond this machine (2026-08-03): regenerating the committed
# tree gives byte-identical output on macOS/py3.14/reportlab 4.5.1 AND Linux/py3.11 with
# reportlab 4.5.1 *and* 5.0.0, pillow 12.2.0, pikepdf 10.11.0, zlib 1.3.1 — so
# tests/requirements-dev.txt stays unpinned. If this check ever fails ONLY in CI, compare
# those versions first: a library that changed its stream encoding is a version story, not
# a fixture story.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
PY="${PYTHON:-${ROOT}/.venv/bin/python}"
[ -x "$PY" ] || PY=python3

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
mkdir -p "$TMP/tests"
cp "$ROOT"/tests/gen_fixtures.py "$ROOT"/tests/gen_tables.py "$ROOT"/tests/gen_demo.py "$TMP/tests/"
cp -R "$ROOT"/tests/fixtures_pdf "$ROOT"/tests/corpus_tables "$ROOT"/tests/demo "$TMP/tests/"

"$PY" "$TMP/tests/gen_fixtures.py" >/dev/null
"$PY" "$TMP/tests/gen_tables.py"   >/dev/null
"$PY" "$TMP/tests/gen_demo.py"     >/dev/null

rc=0
for d in fixtures_pdf corpus_tables demo; do
  # --brief on the whole tree: catches changed bytes AND a committed file the generator no
  # longer produces (or a produced file nobody committed).
  diff -r --brief "$ROOT/tests/$d" "$TMP/tests/$d" || rc=1
done

if [ $rc -ne 0 ]; then
  cat >&2 <<'MSG'

FIXTURE DRIFT: the committed fixtures are not what the generators produce.
Either the generator changed (regenerate + commit the fixtures and the ground truth
together, with a note on which side is right), or generation is not deterministic
(a wall-clock date, a random /ID, or a path leaked into the file).
MSG
  exit 1
fi
echo "fixture drift check: clean (fixtures_pdf, corpus_tables, demo)"
