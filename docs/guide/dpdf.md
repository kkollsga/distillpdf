# The .dpdf document model

`.dpdf` is distillPDF's persistent document model: you analyse a PDF once into a durable
container file, then read, query, search, and re-render it forever — in milliseconds, without
the source PDF in hand.

## What distilling is

Opening a PDF and rendering it pays the full analysis cost every time: text extraction,
reading-order reconstruction, the section tree, table and figure detection, the indexes. A
`.dpdf` captures the result of that work as a typed model and stores it on disk. From then on
the renders are pure functions of the model, so re-rendering is fast and deterministic — the
re-rendered HTML / Markdown / text are byte-identical to what the source PDF produced.

The container is a zip of `model.json` (pages, the section tree, blocks in reading order,
tables, figures, links, indexes) plus an `img/` folder of asset bytes. See
[.dpdf format](../reference/dpdf-format.md) for the on-disk layout and
[Design notes](../datamodel-design.md) for the data model.

!!! warning "Experimental schema (`schema_version == 0`)"
    The model shape is `schema_version 0` and may change before the first downstream cutover
    proves it out. Check `Doc.schema_version` if you persist anything keyed on the format, and
    re-distill from the source PDF to upgrade — a `.dpdf` is a snapshot of extractor quality at
    distill time, not a thing you migrate in place.

## Distilling: `Document.distill()`

Distilling is a method on the loaded PDF ([`Document`](../reference/python.md), returned by
`distillpdf.open`):

```python
import distillpdf

doc = distillpdf.open("paper.pdf")
out = doc.distill("paper.dpdf")   # returns the written path
```

`path` chooses where to write: an explicit `*.dpdf` file, a directory (writes
`<source-stem>.dpdf` inside it), or `None` to write `<source>.dpdf` next to the opened PDF.

### Asset profiles

The `assets` keyword chooses how much image data rides along — size is a deliberate choice,
not a surprise:

| `assets` | What is stored |
| --- | --- |
| `"figures"` (default) | Figure image bytes embedded (hash + dimensions filled); page rasters stay dropped-with-stub |
| `"full"` | Figures and (eventually) page rasters; equals `"figures"` on the born-digital path until page-raster capture lands |
| `"none"` | Text + structure only; all asset bytes dropped, the regenerable stubs kept (a few MB even for a large scan; emailable) |

```python
doc.distill("paper.dpdf", assets="none")   # text + structure only, emailable
```

A dropped asset always keeps a **stub** — its hash, dimensions, and regen info. That makes
every hole a named, reversible one: because the model records the source PDF's SHA-256 (in
`source.sha256`), a dropped figure is re-extractable from the hash-bound original. Figures that
were dropped render as the `image_mode="drop"` placeholder when you re-render from the model
(the model carries no bytes for them).

!!! note "OCR and bboxes"
    OCR passes and per-block bounding boxes are filled by later distillPDF waves. A born-digital
    distill records no OCR passes; `info()` and `ocr_status()` reflect that.

## Loading: `distillpdf.load()` → `Doc`

`distillpdf.load(path)` reads a `.dpdf` and returns a [`Doc`](../reference/python.md) — the
single-document reading surface. It is the counterpart to `distillpdf.open`: `open` reads a
source PDF, `load` reads a model distilled from one.

```python
doc = distillpdf.load("paper.dpdf")
doc            # <distillpdf.Doc 'paper.pdf' pages=12 sections=7 blocks=210 schema_v0>
```

`load` raises `distillpdf.DpdfError` (with an actionable message) when `path` is not a readable
`.dpdf` container — for example a PDF or an HTML file passed by mistake.

`Doc` is immutable and a thin wrapper over the same internal reader the CLI uses, so the Python
API and the `distillpdf <file>.dpdf …` verbs cannot drift: `doc.section(id)` returns exactly
what `distillpdf <file>.dpdf read <id>` prints.

## The reading surface

### Sizing up a document: `info()`

```python
doc.info()
```

Returns a compact roll-up as a dict — the `info` CLI verb as data: the source binding, counts
(`pages` / `sections` / `blocks` / `tables` / `figures` / `footnotes`), OCR state, the asset
storage profile, index coverage, and the embedding-space summary.

### Metadata and source

```python
doc.schema_version   # 0 (experimental)
doc.source           # {file, sha256, pages, distillpdf, generated_at}
doc.metadata         # {title, authors, abstract_text, keywords, …} — fields the extractor found
```

`source` records which PDF this model was distilled from, by which distillpdf version, and
when. `metadata` is the parsed front matter (empty when none was found).

### Sections and the table of contents

`doc.sections` is the flat section list in document order; each entry is
`{id, level, title, parent, page_start, page_end}`. `doc.toc()` returns the same tree as
`(level, title, page_start, id)` tuples — note the trailing element is the **section id** (not
an HTML anchor, unlike `Pdf.toc`), because that id is what reads back through `section()`.

```python
for level, title, page_start, sid in doc.toc():
    print("  " * (level - 1), title, "→", sid)
```

### Reading sections and blocks (the query view)

```python
md = doc.section("sec-methods")   # the section and its whole subtree, as query-markdown
```

`section(sid)` returns the section plus its descendant subtree as simple markdown — headings by
level, paragraphs, list items, tables as pipe-form, figure and caption placeholders. This is
byte-identical to what `distillpdf <file>.dpdf read <sid>` prints. An unknown id raises
`DpdfError` with the closest section ids as a hint.

Blocks are the reading-order units and the source of truth. `blocks()` returns the raw block
dicts (each carries `id`, `kind`, `text`, `page`, `section`, `confidence`, `ocr_pass`, …),
filtered by any combination of arguments:

```python
doc.blocks()                          # every block
doc.blocks(kind="para")               # only paragraphs
doc.blocks(section="sec-methods")     # a section and its subtree
doc.blocks(pages=[4, 5])              # physical pages 4 and 5
doc.blocks(kind="table", section="sec-methods")
```

`kind` is the block kind (`"para"`, `"heading"`, `"table"`, `"figure"`, `"list_item"`,
`"footnote"`, `"caption"`, `"code"`, …); `section` is a section id (includes its subtree);
`pages` is a list of physical page numbers. A single block:

```python
b   = doc.block("b0042")              # one block dict by id; DpdfError if unknown
md  = doc.block_markdown("b0042")     # that block as query-markdown
```

!!! note "Query view vs fidelity view"
    `section()` / `block_markdown()` are the **query** view — addressable per section / block /
    page, emitted as simple markdown. They are *not* the whole-document fidelity render. For the
    model-only re-render that reproduces the source PDF byte-for-byte, use `doc.to_html()`,
    `doc.to_markdown()`, and `doc.text()` (see [Rendering](rendering.md)). Figures render as
    drop-placeholders there, because the born-digital model carries no figure bytes.

### Tables and figures

```python
doc.tables()    # [{id, label, page, page_label, caption}, …] in document order
doc.figures()   # [{id, label, page, page_label, caption}, …]
```

Both return one record per item in document order — the same rows as the `tables` / `figures`
CLI verbs. The `id` reads back via `block()` / `block_markdown()`.

### Page labels and page specs

A PDF's physical page numbers and its printed page labels (the `/PageLabels` map — front matter
numbered `i`, `ii`, `xii`, etc.) are different things. `doc.pages` is the per-page records:
`n` (1-based physical), `width_pts` / `height_pts`, `labels` (e.g. `{"pdf": "xii"}`), and OCR
fields when present.

```python
doc.page_label(3)              # "iii"  (the /PageLabels "pdf" label), or None
doc.page_label(3, "pdf")       # same; scheme defaults to "pdf"

doc.resolve_pages("5")         # [5]
doc.resolve_pages("3-7")       # [3, 4, 5, 6, 7]
doc.resolve_pages("xii-xv")    # label range → physical page numbers
```

`resolve_pages(spec)` accepts a physical page (`"5"`), a range (`"3-7"`), a label (`"xii"`), or
a label range (`"xii-xv"`), and raises `DpdfError` on an unresolvable token — a page that is not
in this document is a loud error, never a silent empty list. The physical numbers it returns
thread straight into `blocks(pages=…)`.

### Neighbours

For prev / next / parent navigation, reach through the underlying model via `doc.model`:

```python
doc.model.section_neighbours("sec-methods")  # (prev_id, next_id, parent_id) by section order
doc.model.block_neighbours("b0042")          # (prev_id, next_id, parent_section_id) by reading order
```

`doc.model` is the raw escape hatch (a `distillpdf.dpdf.Model`); the `Doc` accessors above cover
the documented surface, so reach for `model` only for ad-hoc traversal.

## Searching

`Doc` carries both a coverage-honest lexical `find()` and semantic `search()` (plus `embed()` to
build the vectors). Those have their own guide: see [Searching documents](search.md).

## Re-rendering

`doc.to_html()`, `doc.to_markdown()`, `doc.text()`, and the [`distillpdf.render_*`](../reference/python.md)
functions re-render the fidelity views straight from the file — covered in
[Rendering HTML & Markdown](rendering.md). For scanned PDFs and the OCR fields that show up in
`pages` / `ocr_status()`, see [OCR for scanned PDFs](ocr.md).

## See also

- [.dpdf format](../reference/dpdf-format.md) — the on-disk container layout
- [Design notes, data model](../datamodel-design.md) — why the model is shaped this way
- [Python API](../reference/python.md) — the full `Doc` and `Document` reference
- [Searching documents](search.md) — `find`, `embed`, `search`
- The repository on [GitHub](https://github.com/kkollsga/distillpdf)
