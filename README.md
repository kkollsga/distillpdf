# distillPDF

**Turn any PDF into clean, LLM-ready HTML or Markdown — structure-aware, pure-Rust, MIT-licensed.**

[![PyPI](https://img.shields.io/pypi/v/distillpdf.svg)](https://pypi.org/project/distillpdf/)
[![Python versions](https://img.shields.io/pypi/pyversions/distillpdf.svg)](https://pypi.org/project/distillpdf/)
[![Docs](https://img.shields.io/readthedocs/distillpdf)](https://distillpdf.readthedocs.io/)
[![License: MIT](https://img.shields.io/badge/License-MIT-green.svg)](https://github.com/kkollsga/distillpdf/blob/main/LICENSE)
[![CI](https://github.com/kkollsga/distillpdf/actions/workflows/ci.yml/badge.svg)](https://github.com/kkollsga/distillpdf/actions/workflows/ci.yml)
[![Built with Rust](https://img.shields.io/badge/built%20with-Rust-orange.svg)](https://www.rust-lang.org)

`distillpdf` does structure-aware **PDF extraction**: it reconstructs a document's *structure* —
reading order, headings, paragraphs, lists, tables, and figures — then emits compact, semantic
**HTML or Markdown** (or plain text) ready to feed to an LLM or a **RAG** pipeline. No styling
noise, no layout junk. It's pure Rust on [`lopdf`](https://github.com/J-F-Liu/lopdf), shipped to
Python via [PyO3](https://pyo3.rs) + [maturin](https://www.maturin.rs) as a small, self-contained
wheel — a lightweight, permissively licensed alternative to AGPL/heavyweight extractors (PyMuPDF,
pdfminer, Unstructured), with **no system dependencies and no Python runtime deps**.

> **OCR for scanned PDFs:** detect image-only pages and turn them into clean HTML or a compact
> **searchable PDF**, with a bundled offline engine or an optional accurate VLM —
> [OCR guide »](https://distillpdf.readthedocs.io/en/latest/guide/ocr/) ·
> [example notebook »](https://github.com/kkollsga/distillpdf/blob/main/examples/ocr.ipynb)

## Install

```bash
pip install distillpdf
```

Prebuilt wheels — no compiler or system libraries required. Installing also puts a `distillpdf`
command on your PATH.

## Quickstart

```python
import distillpdf

doc = distillpdf.open("paper.pdf")        # or distillpdf.from_bytes(data)

doc.to_html()                             # writes paper.html (one self-contained file)
doc.to_markdown()                         # writes paper.md + an img/ folder of figures

html     = doc.to_html(return_string=True)   # ...or get the rendered string back instead
text     = doc.extract_text()                # plain text, in reading order
abstract = doc.section("abstract")           # one section's HTML (or None if no match)
```

By default these methods write a file and return `1`; pass `return_string=True` for the string.
[PDF to HTML/Markdown — full guide »](https://distillpdf.readthedocs.io/en/latest/guide/rendering/)

```bash
distillpdf paper.pdf                  # HTML to stdout
distillpdf paper.pdf -o paper.html    # ...or to a file (.md infers Markdown)
distillpdf paper.pdf --markdown       # Markdown to stdout
distillpdf *.pdf -o out/              # batch: out/<name>.html per input
distillpdf scan.pdf --ocr             # OCR a scan → scan.searchable.pdf (bundled, no extra)
```

[Command-line guide »](https://distillpdf.readthedocs.io/en/latest/guide/cli/)

## What's in the box

- **Rendering modes & image handling** — section-first or page-first HTML/Markdown, an optional
  table of contents, and `embed` / `external` (an `img/` folder) / `drop` image modes.
  [Rendering »](https://distillpdf.readthedocs.io/en/latest/guide/rendering/)
- **Raw extraction** — `extract_tables()`, `extract_images()`, `extract_links()`,
  `extract_fonts()` give you the structured pieces, not rendered output.
  [Extraction »](https://distillpdf.readthedocs.io/en/latest/guide/extraction/)
- **The `.dpdf` document model** — `distill()` once to a durable file, then re-render and query
  it forever with no source PDF and no re-analysis.
  [.dpdf »](https://distillpdf.readthedocs.io/en/latest/guide/dpdf/)
- **Honest search** — coverage-reporting lexical `find()` (never a silent miss) and opt-in
  semantic `search()` over BAAI/bge-m3 vectors stored inside the `.dpdf`.
  [Search »](https://distillpdf.readthedocs.io/en/latest/guide/search/)
- **OCR, two tiers** — a bundled fast Tesseract engine (~0.8 s/page, offline, no download) and an
  optional accurate granite-docling VLM (structure + tables), folded into the same HTML /
  Markdown / searchable-PDF outputs.
  [OCR »](https://distillpdf.readthedocs.io/en/latest/guide/ocr/) ·
  [setup »](https://distillpdf.readthedocs.io/en/latest/ocr-setup/)
- **An agent shell over `.dpdf`** — any shell with `distillpdf` installed can drive `info / toc /
  read / find / search` against a distilled model, no SDK or server.
  [CLI »](https://distillpdf.readthedocs.io/en/latest/guide/cli/)

## Comparison

| | distillPDF | PyMuPDF | pdfminer.six | Unstructured |
|---|---|---|---|---|
| License | **MIT** | AGPL / commercial | MIT | Apache (heavy deps) |
| Structure-aware HTML | ✅ | partial | ❌ | ✅ |
| System deps | none | none | none | many |
| Implementation | Rust | C | Python | Python |

## Documentation

Full documentation — guides, the Python API reference, and the `.dpdf` format spec — lives at
**[distillpdf.readthedocs.io](https://distillpdf.readthedocs.io/)**. New here? Start with
[Getting started](https://distillpdf.readthedocs.io/en/latest/getting-started/), then the
[Python API reference](https://distillpdf.readthedocs.io/en/latest/reference/python/) and the
[.dpdf format reference](https://distillpdf.readthedocs.io/en/latest/reference/dpdf-format/).

## Early release — testers wanted

This is a young project and the API is small and may still change. If you have PDFs that come out
wrong, please [open an issue](https://github.com/kkollsga/distillpdf/issues) with the file (or a
description) — real-world documents are exactly what this needs to get better. The `.dpdf`
document model is also experimental (`schema_version 0`) and not yet a stable contract.

## Contributing

Feedback is the fastest way to improve this. Try it on your PDFs and tell me where the output is
wrong, star the repo if it's useful, and PRs are welcome. See
[Contributing and development](https://distillpdf.readthedocs.io/en/latest/about/contributing/)
to build from source or run the tests.

## License

MIT — see [LICENSE](https://github.com/kkollsga/distillpdf/blob/main/LICENSE). Use it anywhere,
including commercial and closed-source projects.
