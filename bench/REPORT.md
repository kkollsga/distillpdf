# PDF extraction benchmark — Rust options vs pymupdf4llm

Corpus: 11 PDFs. Real-world: 4 arXiv papers (attention, BERT, a math paper, NeRF —
all **2-column**), a 44pg ebook, an IRS W-9 form, a Romanian doc, a USGS geology
fact sheet (2-column + map); plus a code sample and 2 synthetic unicode-font
torture tests. pymupdf4llm 1.27.2.2 baseline. Timings = median of 5 warm runs.
Quality = vs PyMuPDF raw `get_text` reference.

## Header (heading) extraction — arXiv

Ground truth (`header_groundtruth.json`) is the ordered section/subsection headers
of each paper, extracted by 4 Sonnet subagents via font analysis (size + bold +
numbering). `test_headers.py` scores `to_html`'s `<h1-3>` against it.

Headers are **paragraph-aware**: a bold line — or a bold run-in lead at the top of
a paragraph ("Encoder:", "Task #1: Masked LM") — is a header; the LEVEL comes from
numbering depth ("2"→h1, "2.1"→h2, "2.1.1"→h3) then font size. This is needed
because arXiv heading faces (Nimbus "Medi", CM "CMBX") are only ~1.1× body or
body-size bold, so the old size-ratio detector missed almost everything.

| paper | header recall (before → after) | level acc | precision |
|---|--:|--:|--:|
| arXiv BERT | 0.03 → **1.00** | 0.92 | 0.69 |
| arXiv attention | → **0.97** | 0.89 | 0.65 |
| arXiv NeRF | → **0.93** | 1.00 | 0.74 |
| arXiv math | → **1.00** | 1.00 | 0.38 |
| **mean** | → **0.975** | **0.953** | **0.61** |

Residual precision loss is title-page lines (paper title, author block), table-of-
contents entries, and a few inline bold all-caps — title/TOC/author noise is the
ceiling, not section detection. Theorem-environment labels (Theorem/Lemma/…),
"et al." running heads and pure-number bold rows are explicitly filtered.

**Generalization — held-out set.** To check the detector wasn't overfit to the 4
tuned papers, 15 fresh arXiv PDFs were fetched across 15 fields (cs.CL/CV/DS/LG,
math.GT/AG/PR, stat.ML, physics.optics, q-bio, econ, eess, astro-ph, quant-ph,
cond-mat) — diverse heading styles: `1 ` vs `1.` numbering, Roman numerals
(`I. INTRODUCTION`), IEEE letter subsections, math run-in heads. Ground truth for
all 15 was extracted the same way (Sonnet font-analysis subagents) →
`header_groundtruth_heldout.json`. `test_headers.py` holds the tuned set to strict
per-file bars and the held-out set to aggregate generalization bars.

**Head-to-head vs the markdown tools** (`compare_headers.py`, same oracle):

| tool | recall (tuned · held-out) | precision | level acc |
|---|--:|--:|--:|
| **distillpdf** | **0.98 · 0.85** | 0.61 · 0.57 | **0.95 · 0.83** |
| pymupdf4llm | 0.84 · 0.70 | **0.90 · 0.83** | 0.45 · 0.38 |
| pdf-inspector | 0.55 · 0.16 | 0.52 · 0.21 | 0.06 · 0.17 |
| pdf_oxide | 0.11 · 0.04 | 0.38 · 0.32 | 0.40 · 0.01 |

distillpdf leads **recall** and **level accuracy** by a wide margin on *both* sets, and
generalizes (held-out recall 0.85, level 0.83) where pdf-inspector and pdf_oxide
collapse on unseen papers (recall 0.16 / 0.04). pymupdf4llm wins **precision** — it
is conservative about what it calls a heading; distillpdf's residual noise is title-
page lines, TOCs, and a few inline bold all-caps (theorem/figure labels, "et al."
running heads, publication-year reference lines, and repeated running headers are
already filtered; numbering-depth gives the level, vs pymupdf4llm's title=`#`/
section=`##` offset and pdf-inspector's flattening).

## Reading order — 2-column (the arXiv/geology target)

Word **recall** is order-blind (it's a set intersection): a tool can interleave a
two-column page line-by-line — `left-line right-line left-line …` — and still score
~0.97 recall while being unreadable. The metric that exposes this is **O = reading-
order fidelity** (token-level `difflib`, `autojunk=False`, vs PyMuPDF).

> Benchmark fix: the old `seqratio` ran `difflib` on the *joined char string* with
> `autojunk=True`, which on any doc >200 tokens discards the frequent words that
> anchor alignment — so it scored arXiv-math at 0.21 when its true word order
> matched the reference at 0.96. `score_quality.py` now reports `O` (token list,
> autojunk off). `test_html.py` recall now also de-hyphenates line-break hyphens on
> both sides, so correctly re-joined words (`represen-`+`tation`→`representation`)
> are credited, not penalised as misses.

**Implementation:** a recursive **XY-cut** column/block segmenter, shared by the
text extractor and the HTML layer (`text::xy_cut_order`). It splits a page on the
widest whitespace gutter — vertical gutters separate columns (left read fully
before right), horizontal gutters peel full-width titles/abstracts/figures — with
a validity guard so sparse/centered layouts and map labels don't trigger false
column cuts. A companion fix: 2-column **prose** averaging >4 words/cell is no
longer misdetected as a table (it was; the phantom table's full-width box straddled
the gutter and forced line-by-line interleaving).

Reading-order fidelity `O` on the 2-column set (markdown/structured-output tools):

| PDF | pymupdf4llm | pdf-inspector | pdf_oxide | **distillpdf (before → after)** |
|---|--:|--:|--:|--:|
| arXiv attention | 0.90 | 0.96 | 0.88 | 0.95 (0.95) |
| arXiv BERT      | 0.86 | 0.90 | 0.92 | **0.88 (0.53 → 0.88)** |
| arXiv math      | 0.73 | 0.85 | 0.83 | **0.85** |
| arXiv NeRF      | 0.75 | 0.90 | 0.81 | **0.94** (best) |
| geology         | —    | 0.69 | 0.69 | 0.67 (0.64 → 0.67) |

distillpdf went from interleaved (BERT O0.53) to **competitive/leading** among
structured-output tools, while keeping recall 0.95–0.97. pymupdf4llm's markdown
actually *drops* 2-column content (recall 0.80–0.85). (`pdf-extract` scores O≈1.0
but emits flat operator-order text with no structure and craters elsewhere —
Cold_Email R0.41, unicode PANIC — so it isn't a structured-output peer.)
XY-cut overhead is ~3–6% (single-digit ms/doc); arXiv text extraction stays
~25–44 ms (15–33 pp), faster than pdf_oxide and ≈50× faster than pymupdf4llm.
Known gap, out of this scope: the Cold_Email ebook scrambles order on every page
(O0.16, recall 1.0) — its content streams aren't in reading order; pre-existing.

## Speed (seconds; markdown/full-extract layer)
| PDF | pg | pymupdf4llm | oxidize | pdf-extract | pdf-inspector | pdf_oxide |
|---|--:|--:|--:|--:|--:|--:|
| Cold_Email | 44 | 4.020 | 0.045 | 0.138 | 0.079 | 0.205 |
| arXiv | 15 | 2.161 | 0.012 | 0.286 | 0.029 | 0.078 |
| fw9_form | 6 | 2.147 | 0.003 | 0.025 | 0.008 | 0.017 |
| romanian | 3 | 0.479 | FAIL | 0.023 | 0.004 | 0.013 |
| unicode_prof | 1 | 0.234 | 0.0005 | PANIC | 0.0007 | 0.0012 |
| unicode_showcase | 5 | 0.516 | 0.001 | PANIC | 0.002 | 0.004 |
All Rust options 20–700× faster than pymupdf4llm. pdf_oxide 20–195× (does full markdown).

## Quality — word recall (R) vs PyMuPDF text
| PDF | oxidize | pdf-extract | pdf-inspector | pdf_oxide |
|---|---|---|---|---|
| Cold_Email | .89 | .41 | .99 | **.99** |
| arXiv | .95 | .99 | .99 | **.98** |
| fw9_form | .99 | 1.0 | 1.0 | **.99** |
| romanian | FAIL | 1.0 | 1.0 | **.81** ⚠ diacritics |
| unicode_prof | .11 garbled | PANIC | .87 | **1.0** |
| unicode_showcase | .06 garbled | PANIC | .00 gave up | **.99** |
pdf_oxide is the only Rust tool that handles synthetic CID fonts ≈ PyMuPDF.

## Capabilities / footprint
| | pymupdf4llm | oxidize | pdf-extract | pdf-inspector | **pdf_oxide** |
|---|---|---|---|---|---|
| Text+markdown | ✓ | ✓ | flat only | ✓ | ✓ |
| Tables | ✓ | weak(1) | ✗ | good(4) | mid (fw9=5, arXiv=2) |
| Image extraction | ✓ | ✓(8) | ✗ | ✗ | ✓(8) |
| Page rendering | ✓ | partial | ✗ | ✗ | ✓ |
| Robustness (6 PDFs) | 6/6 | 5/6 | 4/6 | 6/6 | **6/6** |
| Engine | MuPDF(C) | own | lopdf | lopdf | own (386k LOC) |
| License | AGPL/comm | MIT | MIT | MIT(declared) | MIT/Apache |
| Footprint | 78.6 MB | 3.8 MB | 1.7 MB | 4.0 MB | 19.1 MB |

## Verdict
- **pdf_oxide is the closest pure-Rust drop-in PyMuPDF replacement that exists**: only Rust
  tool with text+markdown+tables+image extraction+rendering in one pip wheel, 6/6 parsed,
  best font/unicode robustness, MIT/Apache, 4× smaller than PyMuPDF, 20–195× faster than
  pymupdf4llm.
- Gaps: weaker Romanian diacritics (R.81 vs 1.0), tables mid-tier, single-author 0.3.x,
  self-reported "100% pass" (pass≠quality — verify on your corpus).
- Narrow alternative: pdf-inspector (lopdf+maturin) if you only need text/tables/markdown
  and want a tiny 4 MB wheel + swappable engine.
