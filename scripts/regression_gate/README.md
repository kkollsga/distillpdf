# Regression gate

Did this build change what distillPDF does? One command, one verdict, compared with the behaviour
frozen in `accuracy/regression_gate_baseline.json` (main at b8d6d69, lopdf 0.45 fork at 909771af).

```sh
scripts/regression-gate.sh check --fast    # every suite and goalline, 13 documents
scripts/regression-gate.sh check --full    # also the judged-page floors, all 141 documents, 3 repeats
```

On the shared 16 GB Mac, run the whole gate under one lock (`heavy.sh scripts/regression-gate.sh
check --fast`). Measured on this machine, excluding lock wait, with load around 15: build 60-100 s;
fast 59 s after the build; full 305 s after the build. A repeat of a full run without a source
change needs `--skip-build`, and the report says it was skipped.

## Exit codes

| exit | meaning |
|---|---|
| 0 | every output byte-identical, or every change named |
| 1 | a score below baseline on any document, a lost document or page, or a failed suite |
| 2 | the gate could not run: no corpus, a changed scorer, a moved PyMuPDF reference |
| 3 | output changed, no score dropped, nobody has named it |

Exit 3 exists because scores are coarse. A reorder between two equally recalled words moves none of
them, so "no drop" is not "no change".

## What it compares

Each of the 141 PDFs in `benchmarking/bench100` (5,872 pages), on both engines, in its own process
per operation: `extract_text`, `extract_page_text` on every page, Markdown, HTML with embedded
images, `extract_links`.

- **Output:** a hash per operation and per page.
- **Scores, per document, with no tolerance:** `recall`, `jaccard`, `order` from
  `benchmarking/score_quality.py` against PyMuPDF; `uri_recall`, `internal_recall`,
  `internal_resolved` from `corpus_tests/test_links.py`; `gt_headings`, `gt_tables`,
  `gt_paragraphs`, `gt_lists`, `gt_images` from `bench100_score.py`'s blind-judge page scores
  on 88 documents, full mode only. `order` is pooled per page rather than over the joined document,
  because difflib with autojunk off is too slow on 383 pages. `headings_kept` and `tables_kept` are
  the share of what the baseline emitted that this build still emits: they cover documents no judge
  sampled, and are not truth scores.
- **Lost:** a failing operation, a shrinking page count, an output now empty, a page now empty.
- **Footprint and time:** reported beyond a stated tolerance, never deciding the verdict. Peak
  `phys_footprint` per process, and retired instructions rather than wall clock. Baseline repeat
  noise, (max-min)/median p50/p95: instructions 0.5%/2.9%, footprint 2.4%/14.9%, CPU 8.3%/41.9%,
  wall 18.1%/100%. Treat a footprint move under about 15% on one document as noise.

Existing gates it runs rather than duplicates: `cargo test -p distillpdf --lib`, `pytest tests/`,
`corpus_tests/test_regression.py`, and `bench100_gate.py --rescore` (full mode, in a private copy of
its state, because the real run overwrites the shared record in `dev-docs/bench/results/`).

## A deliberate change

The report lists every changed document, the pages, words gone and new, and a diff. If the change
is meant:

```sh
scripts/regression-gate.sh name --run <run folder> --name "short name" --reason "why"
```

This records the exact old and new hash per document and operation in
`accuracy/regression_gate_named_changes.json`, and commits with the change. A later different change
to that document surfaces again. Naming a score drop also needs `--accept-drops`, because it accepts
a worse number. Lost documents cannot be named. Replacing the baseline needs
`freeze --owner-note "why"`.

## Where things are

The baseline holds hashes, scores and resource medians only: 571 KiB, no document text. Each run
writes to `target/regression-gate/runs/<stamp>-<mode>/`: `report.md`, `results.json`, `diffs/`,
`outputs/`, `logs/`, about 570 MiB in full mode, so delete old runs. Readable diffs need the baseline's
outputs in `GATE_BASELINE_OUTPUTS`, default `target/regression-gate/baseline-outputs`. Without them
the gate still detects every change by hash and says so. To regenerate them, build this baseline's
`commit` and run `check --full --run DIR` there: `DIR/outputs` is the folder. The PyMuPDF
reference is cached in `target/regression-gate/pymupdf-ref` and rebuilt when absent.

## Proof it catches regressions

Three deliberate breaks, each undone afterwards, on 2026-11-07:

| break | mode | exit | what fired |
|---|---|---|---|
| numbered-section heading rule removed in `detect_header` | full | 1 | 119 documents changed, 102 with `headings_kept` below 1.0; corpus goalline `header_recall`, `header_level`, `header_precision`; bench100 floors, 2 FP ceilings |
| vertical column cuts disabled in `xy_cut` | fast | 1 | 13 of 13 changed, 11 with a drop (`order`, `headings_kept`) |
| `extract_page_text` loses page 3 | fast | 1 | 12 of 13 changed, 12 lost: "pages text lost on page(s) [3]" |

The first break passed all 474 Rust unit tests and all 1,912 Python tests: only corpus gates saw it.
An unchanged build reports 141 of 141 identical in full mode, exit 0. The verdict logic has its own
corpus-free tests in `tests/test_regression_gate.py`.

## Not covered

`order` covers `extract_page_text` only: Markdown and HTML reading order are caught by hash and diff,
not scored. `gt_` scores run on eager slices only. OCR and `to_pdf` are not swept. Footprint and
instructions are macOS-only; elsewhere the gate compares CPU time.
