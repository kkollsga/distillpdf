# PDF extraction benchmark — Rust options vs pymupdf4llm

Corpus: 6 PDFs (4 real-world: arXiv paper, 44pg ebook, IRS W-9 form, Romanian doc;
2 synthetic unicode-font torture tests). pymupdf4llm 1.27.2.2 baseline.
Timings = median of 5 warm runs. Quality = vs PyMuPDF raw `get_text` reference.

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
