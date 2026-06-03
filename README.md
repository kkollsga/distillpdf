# distillPDF

**Turn any PDF into clean, LLM-ready HTML — structure-aware, pure-Rust, MIT-licensed.**

[![PyPI](https://img.shields.io/pypi/v/distillpdf.svg)](https://pypi.org/project/distillpdf/)
[![Python versions](https://img.shields.io/pypi/pyversions/distillpdf.svg)](https://pypi.org/project/distillpdf/)
[![License: MIT](https://img.shields.io/badge/License-MIT-green.svg)](LICENSE)
[![CI](https://github.com/kkollsga/distillpdf/actions/workflows/ci.yml/badge.svg)](https://github.com/kkollsga/distillpdf/actions/workflows/ci.yml)
[![Built with Rust](https://img.shields.io/badge/built%20with-Rust-orange.svg)](https://www.rust-lang.org)

`distillpdf` reads a PDF and reconstructs its *structure* — reading order, headings,
paragraphs, lists, tables, and figures — then emits compact, semantic **HTML** (or plain
text) ready to feed to an LLM or a RAG pipeline. No styling noise, no layout junk: just the
content a model needs.

It's built on [`lopdf`](https://github.com/J-F-Liu/lopdf) and shipped to Python via
[PyO3](https://pyo3.rs) + [maturin](https://www.maturin.rs) as a small, self-contained
wheel — a lightweight, **permissively licensed alternative to AGPL/heavyweight extractors**
(PyMuPDF, pdfminer, Unstructured), with no system dependencies and no Python runtime deps.

> 🧪 **Early release (`0.0.2`) — testers wanted.** The API is small and may still change.
> If you have PDFs that come out wrong, please
> [open an issue](https://github.com/kkollsga/distillpdf/issues) with the file (or a
> description) — real-world documents are exactly what this needs to get better.

## Install

```bash
pip install distillpdf
```

Prebuilt wheels; no compiler or system libraries required.

## Quickstart

```python
import distillpdf

doc = distillpdf.open("paper.pdf")        # or distillpdf.from_bytes(data)

html     = doc.to_html()                  # clean, semantic HTML for an LLM
text     = doc.extract_text()             # plain text, in reading order
toc      = doc.toc()                      # [(level, title, page, anchor_id), ...]
abstract = doc.section("abstract")        # targeted section extraction
```

Need the raw pieces instead of HTML?

```python
doc.extract_tables()   # cell grids (handles multi-level / colspan headers)
doc.extract_images()   # embedded images, with raw bytes
doc.extract_links()    # hyperlinks with targets
doc.extract_fonts()    # font inventory
doc.page_count()       # number of pages
```

## Why distillPDF

- **Structure, not just text.** Two-column reading order, multi-level table headers mapped
  onto a single grid (`colspan`), vector figures transcoded to inline SVG (including rotated
  axis labels), an auto-generated table of contents, and named section extraction
  (`doc.section("methods")`).
- **LLM-ready output.** Lean, class-free HTML — semantic markup a model can read directly,
  with anchor ids so `toc()` entries link straight into the document.
- **Small & permissive.** Pure Rust on `lopdf`, MIT-licensed, no system dependencies, no
  Python runtime dependencies. Drops into any pipeline without license headaches.
- **Fast.** Native Rust extraction with a release build tuned for speed (LTO, single
  codegen unit).

## Scope

**In scope:** text, table, image, and font extraction, plus an HTML/markdown output layer
for RAG and LLM ingestion.

**Out of scope (for now):** page rendering, PDF generation, OCR.

## Comparison

| | distillPDF | PyMuPDF | pdfminer.six | Unstructured |
|---|---|---|---|---|
| License | **MIT** | AGPL / commercial | MIT | Apache (heavy deps) |
| Structure-aware HTML | ✅ | partial | ❌ | ✅ |
| System deps | none | none | none | many |
| Implementation | Rust | C | Python | Python |

## Contributing & feedback

This is a young project and feedback is the fastest way to improve it. The most useful
things you can do:

1. **Try it on your PDFs** and tell me where the output is wrong —
   [open an issue](https://github.com/kkollsga/distillpdf/issues).
2. **Star the repo** if it's useful, so others can find it.
3. **PRs welcome** — see the development notes below.

### Development

The test suite lives in [`tests/`](tests/) (pytest) and runs on CI. It needs only
`distillpdf` installed. CI runs entirely on data we own — a self-contained demo PDF
(`tests/demo/`, end-to-end structure check) and a synthetic table corpus
(`tests/corpus_tables/`). The third-party PDF corpora (`tests/corpus*/`) are gitignored, so
their tests self-skip on a fresh clone and run only when the corpora are present locally for
deeper coverage.

Build from source with [maturin](https://www.maturin.rs):

```bash
git clone https://github.com/kkollsga/distillpdf
cd distillpdf
maturin develop --release    # build + install into the current venv
bash tests/run.sh            # build distillpdf + run pytest
pytest tests/ -q             # or just run the tests against an installed build
```

## License

MIT — see [LICENSE](LICENSE). Use it anywhere, including commercial and closed-source
projects.
