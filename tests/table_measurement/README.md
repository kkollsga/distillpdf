# Table measurement contract

This directory pins the local-only Phase 1 table measurement without adding comparator
packages to CI or the DistillPDF runtime. `lock.json` identifies the exact direct wheels used
on the registered macOS/arm64 runner; their hashes are hashes of downloaded wheel bytes, not
of installed source trees. `corpus_manifest.json` addresses every PDF and ground-truth file.

The official run uses a release wheel and five timed samples after one warm-up. The wrapper
creates a disposable venv below `dev-docs/bench/out/table-analysis`, inherits comparator
packages from the pinned `RUN_PY` environment, and force-installs the supplied wheel there.
For a full evidence run it also requires the direct comparator/runtime wheels named in
`lock.json` in `COMPARATOR_WHEEL_DIR`; installing those local bytes creates an independently
checkable `direct_url.json` archive hash. The reference configuration explicitly excludes
`pymupdf-layout`; the runner fails if it is installed:

```bash
PYTHONHASHSEED=0 RUN_PY=$HOME/labenv/bin/python \
  COMPARATOR_WHEEL_DIR=/absolute/path/to/pinned-wheels \
  WHEEL=/absolute/path/to/distillpdf-0.1.0-cp38-abi3-macosx_11_0_arm64.whl \
  scripts/bench-tables.sh
```

The wrapper first runs the owned scorer attacks and the external GriTS scorer's self-test.
It then records the exact command, git state, installed package versions, wheel hash, scorer
hash, manifest hash, corpus hashes, runner identity, and raw timings. Heavy raw JSON is written
under `dev-docs/bench/out/table-analysis/`; the compact JSON and Markdown summaries are written
under `dev-docs/bench/results/`.

`extract_tables()` does not expose bbox or span topology. The lock therefore records these as
unsupported on the legacy surface; synthetic optional-field attacks prove that the helpers
reject geometry/span corruption when such fields are present, without claiming a product
metric before the additive analysis API exists.
