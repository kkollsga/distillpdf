# distillPDF

**Turn any PDF into clean, LLM-ready HTML or Markdown — structure-aware, pure-Rust, MIT-licensed.**

[![PyPI](https://img.shields.io/pypi/v/distillpdf.svg)](https://pypi.org/project/distillpdf/)
[![Python versions](https://img.shields.io/pypi/pyversions/distillpdf.svg)](https://pypi.org/project/distillpdf/)
[![License: MIT](https://img.shields.io/badge/License-MIT-green.svg)](LICENSE)
[![CI](https://github.com/kkollsga/distillpdf/actions/workflows/ci.yml/badge.svg)](https://github.com/kkollsga/distillpdf/actions/workflows/ci.yml)
[![Built with Rust](https://img.shields.io/badge/built%20with-Rust-orange.svg)](https://www.rust-lang.org)

> 📓 **New — OCR for scanned PDFs:** detect image-only pages and OCR them into clean HTML or a
> compact **searchable PDF** — with a **bundled, offline fast engine** (no extra, no download) or
> an optional accurate VLM. See the **[OCR example notebook »](examples/ocr.ipynb)**

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

distillpdf scan.pdf  --ocr            # OCR a scanned PDF → scan.searchable.pdf (needs [ocr] extra)
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

## OCR — scanned PDFs

Image-only / scanned pages have no text to extract. distillPDF OCRs them and folds the
recovered text back into the same HTML / Markdown / **searchable PDF** outputs — born-digital
pages keep distillPDF's normal extraction. There are two tiers (full **[OCR setup
guide »](docs/ocr-setup.md)** for per-OS install + GPU):

| Tier | Engine | Install | Speed | Quality | Notes |
|---|---|---|---|---|---|
| **fast** (default) | bundled **Tesseract** | none — in the wheel | ~0.8 s/page | char ~95% | offline, no download; flat text (no tables) |
| **accurate** | **granite-docling** VLM | `pip install 'distillpdf[ocr]'` | ~6 s/page (GPU) | char ~97% | structure + **tables**; downloads a model |

The **fast** tier works out of the box on a plain `pip install distillpdf` — no extra, no
PyTorch, no model download, fully offline. **English, Portuguese and Norwegian** ship in the
wheel; the document's language is **auto-detected** from a sample at the start of processing and
OCR runs in just that language (faster, more accurate). Pin it with
`OcrConfig(languages=["eng"])`, or point `TESSDATA_PREFIX` at your own tessdata for other
languages.

The **accurate** tier auto-selects a runtime that installs with **no C++ compiler**: MLX on
Apple Silicon, and **PyTorch/transformers** on Windows/Linux/Intel-Mac (torch ships prebuilt
wheels for every platform + Python).

**GPU:** on Apple Silicon the accurate tier runs on the **Metal GPU via MLX** by default — fast,
nothing to do. On Windows/Linux, PyPI's default `torch` is **CPU-only and slow** for a VLM; for
**NVIDIA acceleration** install the CUDA build and the engine uses it automatically:
```bash
pip install torch --index-url https://download.pytorch.org/whl/cu124   # then pip install 'distillpdf[ocr]'
```
(force a device with `OcrConfig(device="cuda"|"cpu")`). Prefer a small, no-PyTorch runtime and
have a compiler / matching `llama-cpp-python` wheel? Use `pip install 'distillpdf[ocr-gguf]'` +
`engine="granite-docling-gguf"`. The speed gap is real: a 509-page scan OCRs in **~6 min** on the
fast tier vs much longer on the accurate tier — and the accurate tier on **CPU is very slow**
(minutes/page), so use a GPU for it or stick with the fast tier.

From the command line — open → OCR (progress bar shown automatically) → write, no Python:

```bash
distillpdf scan.pdf --ocr                       # → scan.searchable.pdf  (fast tier, bundled)
distillpdf scan.pdf --ocr --remove-raster       # → reflowed clean text + figures, smaller file
distillpdf scan.pdf --ocr -o out.html           # OCR'd HTML  (use a .md path for Markdown)
distillpdf scan.pdf --ocr --ocr-engine accurate # granite-docling (needs the [ocr] extra)
distillpdf --list-ocr-engines                   # show engines: name, tier, bundled, offline
```

Or from Python:

```python
import distillpdf

doc = distillpdf.open("scanned.pdf")
doc.run_ocr()                  # fast tier by default — bundled, offline; cached on the document
                               # (a progress bar shows on a terminal — pass progress=False to silence)
# accurate tier (needs the [ocr] extra):
#   doc.to_html("out.html", ocr=True, engine="granite")   # or run_ocr(engine="granite")
doc.to_pdf("out.pdf")          # searchable PDF        (reuses the cache — no second pass)
doc.to_html("out.html")        # OCR text folded into clean HTML
doc.to_markdown("out.md")      # …and Markdown
```

`run_ocr` OCRs each scanned page once and caches the result on the document, so every output
is rendered from a single pass. Trying it out? Pass `only={1, 2, 3}` to OCR just a few pages first. For a single output you can skip the explicit call and pass
`ocr=True` — e.g. `doc.to_pdf("out.pdf", ocr=True)` runs OCR (once) then writes. The render
methods (`to_html` / `to_markdown` / `to_pdf`) also work *without* OCR — they just **warn**
that scanned pages have no text and point you at `run_ocr`.
Detection handles real-world scans — images nested in Form XObjects, CCITT Group-4 fax and
Flate-wrapped JPEG encodings, and full-page rasters whose only text is an e-filing stamp.

**Searchable-PDF modes** (`doc.to_pdf`):

- **keep the scan (default)** — the original page image is preserved and the OCR text is added
  as an *invisible, selectable* layer over it. The scan always shows, so OCR errors never
  destroy content (best for archival/legal use).
- **`remove_raster=True`** — pages are reflowed to clean visible text + cropped figures and the
  raster is dropped, for a much smaller file.

**Tables** are recovered natively by the **accurate** tier: granite-docling emits OTSL table
structure, which distillPDF renders as a real `<table>` (with `<th>` / `colspan` / `rowspan`) in
HTML and a **gridded table** (cell rules + shaded header row) in the searchable PDF. The reflow
also **justifies** granite's paragraph blocks for a typeset look. The fast (Tesseract) tier
produces flat text only — use the accurate tier when table structure matters.

> Pick the tier for the job: the fast tier is great for "make this scan searchable, now"; the
> accurate tier is for structure-faithful extraction (tables, headings, reading order). See the
> **[OCR example notebook »](examples/ocr.ipynb)**.

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

**In scope:** text, table, image, and font extraction; an HTML/markdown output layer for RAG
and LLM ingestion; and optional OCR for scanned pages with a searchable-PDF writer.

**Out of scope (for now):** page rendering.

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
