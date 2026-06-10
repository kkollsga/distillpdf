# distillPDF

Turn any PDF into clean, LLM-ready HTML or Markdown — structure-aware, pure-Rust,
MIT-licensed. distillPDF reconstructs a document's *structure* and emits semantic output a
model can read directly, with no styling noise and no system dependencies.

## What it is

`distillpdf` reads a PDF and reconstructs its structure — reading order, headings,
paragraphs, lists, tables, and figures — then emits compact, semantic HTML or Markdown (or
plain text) ready to feed to an LLM or a RAG pipeline. Markdown is produced from the same
HTML the HTML renderer emits, so both formats benefit from every extraction improvement.

It is built on [`lopdf`](https://github.com/J-F-Liu/lopdf) and shipped to Python via
[PyO3](https://pyo3.rs) and [maturin](https://www.maturin.rs) as a small, self-contained
wheel: a lightweight, permissively licensed alternative to AGPL or heavyweight extractors
(PyMuPDF, pdfminer, Unstructured), with no system dependencies and no Python runtime
dependencies.

## Why it exists

- **Structure, not just text.** Two-column reading order, multi-level table headers mapped
  onto a single grid (`colspan`), vector figures transcoded to inline SVG, an auto-generated
  table of contents, and named section extraction (`doc.section("methods")`).
- **LLM-ready output.** Lean, class-free HTML — semantic markup with anchor ids so table-of-
  contents entries link straight into the document.
- **Small and permissive.** Pure Rust on `lopdf`, MIT-licensed, no system dependencies, no
  Python runtime dependencies. It drops into any pipeline without license headaches.
- **Fast.** Native Rust extraction with a release build tuned for speed (LTO, single codegen
  unit).

## Features

| Feature | What it does |
|---|---|
| [Rendering HTML & Markdown](guide/rendering.md) | `to_html()` / `to_markdown()` with section- or page-first structure, image modes, and an optional table of contents. |
| [Raw extraction](guide/extraction.md) | `extract_tables()`, `extract_images()`, `extract_links()`, `extract_fonts()` — the structured pieces, not rendered output. |
| [The .dpdf document model](guide/dpdf.md) | `distill()` persists the analysis to a durable file you can re-render and query forever, with no source PDF. |
| [Searching documents](guide/search.md) | Coverage-honest lexical `find()` and opt-in semantic `search()` over a `.dpdf`. |
| [OCR for scanned PDFs](guide/ocr.md) | A bundled offline fast tier (Tesseract) and an optional accurate VLM tier (granite-docling), folded into the same HTML / Markdown / searchable-PDF outputs. |
| [Command line](guide/cli.md) | The `distillpdf` CLI — convert, distill, search, and OCR without writing Python. |

## Install

```bash
pip install distillpdf
```

Prebuilt wheels; no compiler or system libraries required. Installing also puts a
`distillpdf` command on your PATH.

## 30-second example

```python
import distillpdf

doc = distillpdf.open("paper.pdf")        # or distillpdf.from_bytes(data)

doc.to_html()                             # writes paper.html (one self-contained file)
doc.to_markdown()                         # writes paper.md + an img/ folder of figures

html = doc.to_html(return_string=True)    # ...or get the rendered string back instead
```

By default these methods write a file (deriving the name from the PDF) and return `1`; pass
`return_string=True` to get the rendered text back and write nothing. See
[Rendering HTML & Markdown](guide/rendering.md) for modes, image handling, and targeted
section extraction.

## Comparison

| | distillPDF | PyMuPDF | pdfminer.six | Unstructured |
|---|---|---|---|---|
| License | **MIT** | AGPL / commercial | MIT | Apache (heavy deps) |
| Structure-aware HTML | yes | partial | no | yes |
| System deps | none | none | none | many |
| Implementation | Rust | C | Python | Python |

## Scope

**In scope:** text, table, image, and font extraction; an HTML / Markdown output layer for
RAG and LLM ingestion; and optional OCR for scanned pages with a searchable-PDF writer.

**Out of scope (for now):** page rendering.

## Early release, testers wanted

!!! warning "Early release — the API is small and may still change"
    This is a young project. If you have PDFs that come out wrong, please
    [open an issue](https://github.com/kkollsga/distillpdf/issues) with the file (or a
    description) — real-world documents are exactly what this needs to get better. The
    `.dpdf` document model is also experimental (`schema_version 0`) and not yet a stable
    contract; see [the .dpdf format reference](reference/dpdf-format.md) and the
    [design notes](datamodel-design.md).

distillPDF is MIT-licensed — use it anywhere, including commercial and closed-source
projects. The source lives at
[github.com/kkollsga/distillpdf](https://github.com/kkollsga/distillpdf); see
[Contributing and development](about/contributing.md) to build from source or run the tests.
New here? Start with [Getting started](getting-started.md).
