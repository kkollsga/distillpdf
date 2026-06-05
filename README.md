# distillPDF

**Turn any PDF into clean, LLM-ready HTML or Markdown — structure-aware, pure-Rust, MIT-licensed.**

[![PyPI](https://img.shields.io/pypi/v/distillpdf.svg)](https://pypi.org/project/distillpdf/)
[![Python versions](https://img.shields.io/pypi/pyversions/distillpdf.svg)](https://pypi.org/project/distillpdf/)
[![License: MIT](https://img.shields.io/badge/License-MIT-green.svg)](LICENSE)
[![CI](https://github.com/kkollsga/distillpdf/actions/workflows/ci.yml/badge.svg)](https://github.com/kkollsga/distillpdf/actions/workflows/ci.yml)
[![Built with Rust](https://img.shields.io/badge/built%20with-Rust-orange.svg)](https://www.rust-lang.org)

`distillpdf` reads a PDF and reconstructs its *structure* — reading order, headings,
paragraphs, lists, tables, and figures — then emits compact, semantic **HTML** or
**Markdown** (or plain text) ready to feed to an LLM or a RAG pipeline. No styling noise, no
layout junk: just the content a model needs. Markdown is produced from the same HTML, so
both formats benefit from every extraction improvement.

It's built on [`lopdf`](https://github.com/J-F-Liu/lopdf) and shipped to Python via
[PyO3](https://pyo3.rs) + [maturin](https://www.maturin.rs) as a small, self-contained
wheel — a lightweight, **permissively licensed alternative to AGPL/heavyweight extractors**
(PyMuPDF, pdfminer, Unstructured), with no system dependencies and no Python runtime deps.

> 🧪 **Early release (`0.0.3`) — testers wanted.** The API is small and may still change.
> If you have PDFs that come out wrong, please
> [open an issue](https://github.com/kkollsga/distillpdf/issues) with the file (or a
> description) — real-world documents are exactly what this needs to get better.

## Install

```bash
pip install distillpdf
```

Prebuilt wheels; no compiler or system libraries required. Installing also puts a
`distillpdf` command on your PATH.

## Command line

Convert a PDF to clean HTML or Markdown in one command:

```bash
distillpdf paper.pdf                  # HTML to stdout
distillpdf paper.pdf -o paper.html    # ...or to an HTML file
distillpdf paper.pdf -o paper.md      # ...or Markdown (inferred from the .md extension)
distillpdf paper.pdf --markdown       # Markdown to stdout
distillpdf *.pdf -o out/              # batch: out/<name>.html per input

distillpdf paper.pdf -o p.html --image-mode external  # lean HTML + an img/ folder
distillpdf paper.pdf --image-mode drop           # replace images with placeholder text

distillpdf paper.pdf --mode page      # page-first HTML (default is section-first)
distillpdf paper.pdf --no-toc         # omit the table-of-contents nav
distillpdf paper.pdf --text           # plain text instead of HTML
distillpdf paper.pdf --toc            # print the table of contents
distillpdf paper.pdf --section abstract
```

(Also available as `python -m distillpdf`.)

## Quickstart

```python
import distillpdf

doc = distillpdf.open("paper.pdf")        # or distillpdf.from_bytes(data)

# By default, these WRITE a file and return 1 (deriving the name from the PDF):
doc.to_html()                             # → paper.html  (one self-contained file)
doc.to_markdown()                         # → paper.md  + an img/ folder of figures
doc.to_html("out.html")                   # ...or to a specific path / directory
doc.to_html("out.html", image_mode="external")  # ...lean HTML + an img/ folder

# Pass return_string=True to get the rendered text back instead of writing:
html = doc.to_html(return_string=True)    # self-contained HTML string (inline images)
md   = doc.to_markdown(return_string=True) # ...or Markdown (built from the same HTML)

# rendering options work the same on both:
doc.to_html(mode="page", toc=False)
text     = doc.extract_text()             # plain text, in reading order
toc      = doc.toc()                      # [(level, title, page, anchor_id), ...]
abstract = doc.section("abstract")        # targeted section extraction (returns a string)
```

### Markdown

`to_markdown()` is a transform of the very HTML `to_html()` produces — so every
processor improvement (clipping, heading detection, tables, front-matter) flows into
Markdown automatically, with no second renderer to keep in sync.

```python
doc.to_markdown()                                  # string (images embedded inline)
doc.to_markdown("paper.md")                        # writes paper.md
doc.to_markdown("paper.md", image_mode="external") # paper.md + paper's img/fig_NN_slug.ext
doc.to_markdown(image_mode="drop")                 # drop images (caption-only placeholders)
```

`image_mode` controls figures — identically for `to_html()` and `to_markdown()`:

| `image_mode` | result |
|---|---|
| `"embed"` (default) | inline base64 `data:` URIs — one self-contained string/file |
| `"external"` | extract each figure to `img/fig_NN_slug.ext` (vectors as `.svg`) and reference it; only when writing to a file (a returned string falls back to `"embed"`) |
| `"drop"` | replace images with placeholder text |

Because both formats run through the same converter, `"external"` produces the **same**
`img/` layout whether you write `.html` or `.md`.

### Output modes

By default, **logical sections are first-order**: every heading becomes its own nested
`<section id="sec-…">`, so you can pull a whole section as one block (great for RAG / LLM
chunking), and page numbers are dropped.

```python
distillpdf.open("paper.pdf").to_html(return_string=True)
# <section id="sec-abstract"><h2>Abstract</h2><p>…</p></section>

distillpdf.open("paper.pdf").section("methods")   # → the <section id="sec-methods"> block
```

Pass `mode="page"` for the page-faithful structure instead — each page wrapped in
`<section data-page="N" id="page-N">`, with page numbers in the TOC:

```python
distillpdf.open("paper.pdf").to_html(mode="page", return_string=True)
```

Want compact, text-only output? `image_mode="drop"` replaces each embedded image with a
lightweight `<image N>` placeholder (captions and figure anchors are kept):

```python
distillpdf.open("paper.pdf").to_html(image_mode="drop", return_string=True)
# <figure id="fig-1"><image 1><figcaption>…</figcaption></figure>
```

Pass `toc=False` to skip the auto table-of-contents `<nav>` (heading anchors are still
emitted, so `#section` links and `doc.section(...)` keep working):

```python
distillpdf.open("paper.pdf").to_html(toc=False, return_string=True)
```

### Rendering options

`open()` only loads the PDF; the rendering options live on `to_html()` and
`to_markdown()` (and `mode` on `toc()`/`section()`), since that's where the content is
actually extracted:

| Option | Default | Effect |
|---|---|---|
| `path=` | `None` | where to write: a file, or a directory to place `<source-stem>.html`/`.md` in. `None` writes `<source>.html`/`.md` next to the PDF. (Ignored when `return_string=True`.) |
| `return_string=` | `False` | `True` returns the rendered string and writes nothing; the default writes a file and returns `1` |
| `mode=` | `"section"` | `"page"` wraps each page in `<section data-page="N">` and numbers TOC entries; the default groups content into nested `<section id="sec-…">` and drops page info |
| `image_mode=` | `"embed"` | `"embed"` inline `data:` URIs (self-contained); `"external"` an `img/` folder (when writing to a file); `"drop"` placeholder text |
| `toc=` | `True` | `False` omits the `<nav>` table of contents (section/heading anchors still emitted) |

### Raw pieces

Need the structured data instead of HTML?

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
