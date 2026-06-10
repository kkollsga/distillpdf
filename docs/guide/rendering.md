# Rendering HTML & Markdown

`distillpdf.open()` (and `distillpdf.from_bytes()`) return a `Document`. Opening only loads
and parses the PDF container — the actual extraction and rendering happen on the
`Document`, where all the options live.

```python
import distillpdf

doc = distillpdf.open("paper.pdf")
doc.to_html("paper.html")               # writes a file, returns 1
md = doc.to_markdown(return_string=True)  # returns the Markdown string instead
```

`from_bytes` is the same surface for an in-memory PDF:

```python
doc = distillpdf.from_bytes(open("paper.pdf", "rb").read())
```

A `Document` from bytes has no source path, so writing output without an explicit `path`
is an error — pass `path=` or `return_string=True`.

## The rendering surface

| Method | Returns by default | Purpose |
|---|---|---|
| `to_html(...)` | `1` (wrote a file) | clean, AI-ready HTML |
| `to_markdown(...)` | `1` (wrote a file) | Markdown, derived from the same HTML |
| `extract_text()` | `str` | plain text, all pages concatenated in page order |
| `toc(mode=...)` | `list[(level, title, page, anchor_id)]` | the detected outline |
| `section(name, ...)` | `str` or `None` | the HTML of one section |

`to_html` and `to_markdown` also accept OCR keywords (`ocr=`, `engine=`, `backend=`) for
scanned PDFs — see [OCR for scanned PDFs](ocr.md). This page covers the born-digital
rendering path.

## File output vs. returning a string

By default `to_html` and `to_markdown` **write a file and return `1`**. The destination
follows `path=`:

- `path=None` (default) — write `<source>.html` / `<source>.md` next to the opened PDF
  (`paper.pdf` → `paper.html`).
- `path="out.html"` — write that file.
- `path="some/dir"` — a directory; the file is placed there as `<source-stem>.html` / `.md`.

Set `return_string=True` to get the rendered text back as a `str` and write nothing. When
`return_string=True`, `path=` is ignored.

```python
doc.to_html()                          # writes paper.html, returns 1
doc.to_html("out.html")                # writes out.html, returns 1
doc.to_html("build/")                  # writes build/paper.html
html = doc.to_html(return_string=True) # returns the HTML, writes nothing
```

## Section mode vs. page mode

`mode=` chooses how the body is structured. The default is `"section"`.

### `mode="section"` (default)

Logical sections are first-order: every detected heading becomes its own nested
`<section id="sec-…">`, and page numbers are dropped. This is the structure you want for
RAG / LLM chunking, because you can pull a whole section as one balanced element.

```html
<section id="sec-abstract"><h2>Abstract</h2><p>…</p></section>
```

The `sec-…` id is slugged from the heading text. It is the same id `toc()` reports as the
`anchor_id` and the same id [`section()`](#one-section-at-a-time) matches against.

### `mode="page"`

Page-faithful structure: each page is wrapped in `<section data-page="N" id="page-N">`,
and the TOC entries carry their page number.

```html
<section data-page="1" id="page-1"> … </section>
```

```python
doc.to_html(mode="page", return_string=True)
```

In both modes heading anchors (`sec-…`) are minted from the same heading text, so
`#sec-…` links and `section()` keep working regardless of mode.

## The table of contents

By default the rendered HTML is prefixed with an auto table-of-contents `<nav>`. Pass
`toc=False` to omit it. Heading anchors are still emitted either way, so `#sec-…` links
and `doc.section(...)` keep working.

```python
doc.to_html(toc=False, return_string=True)
```

You can also read the outline directly without rendering a full document:

```python
doc.toc()              # mode="section" — page is 0 for every entry
doc.toc(mode="page")   # page carries the real 1-indexed page number
```

`toc()` returns a list of `(level, title, page, anchor_id)` tuples in reading order.
`level` 1 is the title, 2 a section, 3 a subsection, and so on. In `"section"` mode the
`page` field is `0` (page info is dropped in that mode); in `"page"` mode it is the real
page number.

!!! note
    `toc()` is the outline **detected** from headings. The PDF's own author-supplied
    bookmarks (the `/Outlines` tree) are a separate method, `doc.outline()`, documented in
    the [Python API reference](../reference/python.md).

## One section at a time

`section(name, mode="section", image_mode="embed")` returns the HTML of a single section:
the matching heading plus its content up to the next same-or-higher heading. `name`
matches a `sec-…` slug, an id prefix, or a case-insensitive substring of the title.

```python
doc.section("methods")     # → the <section id="sec-methods"> block, or None
doc.section("Abstract")    # case-insensitive title substring also works
```

Returns `None` when nothing matches. Because the result is a returned string with no
folder to write into, `image_mode="external"` behaves like `"embed"` here (see below).

## Images: `image_mode`

`image_mode=` controls how figures are emitted, identically in meaning for `to_html` and
`to_markdown` — only the default differs.

| `image_mode` | result |
|---|---|
| `"embed"` | inline base64 `data:` URIs — one self-contained string or file |
| `"external"` | extract each figure to a sibling `img/` folder (`img/fig_NN_slug.ext`, vector figures as `.svg`) and reference it; only when writing to a file |
| `"drop"` | replace each image with a lightweight placeholder, keeping captions and figure anchors |

Defaults:

- `to_html` defaults to `image_mode="embed"` — a single self-contained file or string.
- `to_markdown` defaults to `image_mode="external"` — inline `data:` URIs are impractical
  in Markdown.

### The `img/` folder layout

With `image_mode="external"` and a file destination, figures are extracted next to the
output file:

```text
out.html
img/
  fig_01_chart.png
  fig_02_diagram.svg
```

Because both formats run through the same converter, `"external"` produces the **same**
`img/` layout whether you write `.html` or `.md`.

### The falls-back-to-embed rule

`"external"` needs a folder to write into. A returned string has none, so:

- `to_html(..., return_string=True, image_mode="external")` falls back to `"embed"` —
  you get a self-contained string.
- `to_markdown(..., return_string=True, image_mode="external")` has no folder either and
  falls back to caption-only placeholders (Markdown cannot inline raster bytes cleanly).

`section()` returns a string, so `"external"` there always behaves like `"embed"`.

### The `"drop"` placeholder

`image_mode="drop"` keeps the figure structure (anchor and caption) but replaces the
image bytes with a placeholder:

```html
<figure id="fig-1"><image 1><figcaption>…</figcaption></figure>
```

!!! warning
    Embedding page rasters inline (`image_mode="embed"`, the HTML default) can balloon a
    scanned document to hundreds of MB. When the output exceeds 50 MB, `to_html` emits a
    warning pointing you at `image_mode="external"` or `image_mode="drop"`.

## How Markdown is derived

There is no separate Markdown renderer. `to_markdown` transforms the **same HTML**
`to_html` emits, so every extraction or layout improvement flows into both formats
automatically. `mode=` and `toc=` mean exactly what they do for `to_html`; only the
`image_mode` default changes (`"external"` instead of `"embed"`).

```python
doc.to_markdown("paper.md")                       # paper.md + paper's img/fig_NN_slug.ext
doc.to_markdown("paper.md", image_mode="embed")   # inline data: URIs
doc.to_markdown(image_mode="drop", return_string=True)  # caption-only placeholders
```

## Plain text

`extract_text()` returns the plain text of every page, concatenated in page order, with one
trailing newline per page. It takes no arguments — there is no `mode` or `image_mode`. It is a
hybrid extractor: a position- and width-aware, ToUnicode-aware content-stream pass
(handling CID fonts and diacritics), falling back to lopdf's per-page extractor when the
primary pass recovers little.

```python
text = doc.extract_text()
```

## The module-level render functions

`distillpdf` also exposes three module-level functions — `render_html`, `render_markdown`,
and `render_text` — that render directly from a distilled `.dpdf` document model on disk,
with no source PDF present. They mirror the `to_html` / `to_markdown` / `extract_text`
options (`mode`, `toc`, and `image_mode` on `render_markdown`).

```python
import distillpdf
html = distillpdf.render_html("paper.dpdf", mode="section", toc=True)
```

These belong to the `.dpdf` workflow rather than the live-PDF path. For the document model
and its higher-level `Doc` reading surface, see
[The .dpdf document model](dpdf.md); full signatures are in the
[Python API reference](../reference/python.md).

## See also

- [Raw extraction](extraction.md) — get structured data (images, tables, fonts, links)
  instead of rendered HTML.
- [OCR for scanned PDFs](ocr.md) — render scanned pages by recovering their text first.
- [Python API reference](../reference/python.md) — every method, every keyword.
- [distillPDF on GitHub](https://github.com/kkollsga/distillpdf)
