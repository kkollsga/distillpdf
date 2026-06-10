# Getting started

A short tour: install distillPDF, then convert a PDF to clean HTML/Markdown both from Python and from the command line.

## Install

distillPDF ships prebuilt wheels, so installation does not require a Rust toolchain or a C compiler:

```bash
pip install distillpdf
```

This installs the Python package and the `distillpdf` command. The CLI is also reachable as a module:

```bash
python -m distillpdf paper.pdf -o paper.html
```

!!! note
    The bundled "fast" OCR engine (Tesseract) and the core extractor are included in the base install — no extra needed. The higher-accuracy granite-docling VLM engine is opt-in and requires a runtime you install yourself; see [OCR runtime setup](ocr-setup.md).

## Python: a 10-minute tour

Open a PDF with `distillpdf.open()`, which returns a `Document`:

```python
import distillpdf

doc = distillpdf.open("paper.pdf")
```

`Document` wraps the pure-Rust core and delegates every core extraction method to it, so the methods below are all available on the object you get back.

### Render to HTML or Markdown

`to_html()` and `to_markdown()` write a file by default and return `1`:

```python
doc.to_html("paper.html")        # writes paper.html, returns 1
doc.to_markdown("paper.md")      # writes paper.md, returns 1
```

If you omit the path, the destination is derived from the source PDF name (`paper.html` / `paper.md`). To get the rendered output back as a string instead of writing a file, pass `return_string=True`:

```python
html = doc.to_html(return_string=True)
md = doc.to_markdown(return_string=True)
```

Both methods accept `mode` (`"section"`, the default, or `"page"`), `toc` (include the auto table-of-contents, default `True`), and `image_mode`. `image_mode` defaults to `"embed"` for HTML (inline `data:` URIs, one self-contained file) and `"external"` for Markdown (a sibling `img/` folder). See [Rendering HTML & Markdown](guide/rendering.md) for the full set of options.

!!! note
    When rendering HTML to a string, `image_mode="external"` falls back to `"embed"` — a returned string has no folder to write images into.

### Plain text

`extract_text()` returns the document's text content as a string:

```python
text = doc.extract_text()
```

### Outline and single sections

`toc()` returns the detected document outline as a list of `(level, title, page, anchor_id)` tuples in reading order. Level 1 is the title, 2 a section, 3 a subsection, and so on. The `anchor_id` matches an `id=` in the HTML, so you can link to it with `#anchor_id`:

```python
for level, title, page, anchor in doc.toc():
    print("  " * (level - 1) + title)
```

`section()` returns the HTML for a single section — the heading matching `name` plus its content up to the next same-or-higher heading. The `name` can be the section's `sec-…` slug, an id prefix, or a case-insensitive title substring. It returns `None` if nothing matches:

```python
abstract = doc.section("abstract")   # HTML string, or None
```

Like the render methods, `toc()` accepts `mode=` and `section()` accepts `mode=` and `image_mode=`.

## CLI: a 10-minute tour

The `distillpdf` command converts PDFs to HTML by default, streaming to stdout for a single input:

```bash
distillpdf paper.pdf                      # HTML to stdout
distillpdf paper.pdf -o paper.html        # HTML to a file
distillpdf paper.pdf --markdown           # Markdown to stdout
distillpdf paper.pdf -o paper.md          # Markdown (inferred from the .md extension)
distillpdf paper.pdf --text               # plain text instead of HTML
```

Batch several PDFs by giving `-o` a directory; each input writes `<name>.<ext>` into it:

```bash
distillpdf *.pdf -o out/                  # out/<name>.html per input
```

Other flags mirror the Python options:

```bash
distillpdf paper.pdf --mode page          # page-first (default is section-first)
distillpdf paper.pdf -o p.html --image-mode external   # lean HTML + an img/ folder
distillpdf paper.pdf --image-mode drop    # replace images with placeholder text
distillpdf paper.pdf --no-toc             # omit the table-of-contents nav
distillpdf paper.pdf --toc                # print the table of contents
distillpdf paper.pdf --section abstract   # emit only the named section
```

When writing files (rather than streaming to stdout), the command prints `distillpdf: wrote <path>` to stderr. See [Command line](guide/cli.md) for the complete reference, including OCR and `.dpdf` distillation.

## Where next

- Rendering options (modes, table of contents, image handling): [Rendering HTML & Markdown](guide/rendering.md)
- Scanned or image-only PDFs: [OCR for scanned PDFs](guide/ocr.md)
- The persistent document model and lexical + semantic search: [The .dpdf document model](guide/dpdf.md)
- The full Python surface: [Python API](reference/python.md)
