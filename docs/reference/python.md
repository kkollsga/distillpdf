# Python API reference

The exhaustive reference for the `distillpdf` package. Every public symbol exported from
`distillpdf.__init__.__all__` is documented here, plus the public surface of the
`distillpdf.ocr` module. For task-oriented walkthroughs see the guide pages
([Rendering](../guide/rendering.md), [Raw extraction](../guide/extraction.md),
[The .dpdf model](../guide/dpdf.md), [Searching](../guide/search.md),
[OCR](../guide/ocr.md)).

The package re-exports the compiled Rust core (`Pdf` and the module-level functions) and the
Python workflow layer (`Document`, `Doc`, the `ocr` module). Source of truth: the Rust
`#[pymethods]` in `src/lib.rs` and the Python modules under `python/distillpdf/`.

!!! note "Two entry points, two surfaces"
    [`open`](#open) / [`from_bytes`](#from_bytes) read a **source PDF** and return a
    [`Document`](#document). [`load`](#load) reads a distilled **`.dpdf`** and returns a
    [`Doc`](#doc). They are different objects with different methods — a `Document` extracts
    and renders a live PDF; a `Doc` queries and re-renders a frozen model.

---

## Module functions

### open

```python
distillpdf.open(path: str) -> Document
```

Open a PDF from a filesystem path and return a [`Document`](#document). Equivalent to
`Document.open(path)`. Raises `ValueError` (`open failed: …`) when the file cannot be read or
parsed.

```python
import distillpdf
doc = distillpdf.open("paper.pdf")
doc.to_html("paper.html")
```

### from_bytes

```python
distillpdf.from_bytes(data: bytes) -> Document
```

Open a PDF from raw bytes and return a [`Document`](#document). There is no source path, so
writing output without an explicit `path` is an error — pass a `path` to the render methods.
Raises `ValueError` (`parse failed: …`) on a malformed PDF.

### load

```python
distillpdf.load(path: str) -> Doc
```

Load a distilled `.dpdf` document model and return a [`Doc`](#doc). The counterpart to `open`:
produce the `.dpdf` first with `distillpdf.open("x.pdf").distill("x.dpdf")`. Raises
[`DpdfError`](#dpdferror) when `path` is not a readable `.dpdf` container (e.g. a PDF or HTML
file passed by mistake).

### load_model

```python
distillpdf.load_model(path: str) -> str
```

Load a `.dpdf` container and return its `model.json` as a JSON string — the low-level handle
under [`Doc`](#doc). The returned JSON is the canonical, sorted-key form, so
distill → `load_model` → re-save is byte-stable. Raises `ValueError` when `path` is not a
readable container. Most callers want `load()` / `Doc` instead.

### save_dpdf

```python
distillpdf.save_dpdf(
    src_path: str,
    dst_path: str,
    model_json: str,
    extra_members: dict[str, bytes],
) -> None
```

Write a new `.dpdf` to `dst_path` from a `model_json` string, carrying over the asset members
of the container at `src_path`. `extra_members` are extra named members (e.g.
`embeddings/<id>.bin`) to add; a same-named member overwrites the carried one. This is the
write path [`Doc.embed`](#docembed) uses internally; you rarely call it directly. Raises
`ValueError` on a bad source container or unparseable `model_json`.

### read_dpdf_member

```python
distillpdf.read_dpdf_member(path: str, member: str) -> bytes | None
```

Read the raw bytes of a single container member (e.g. an `embeddings/<id>.bin` vector matrix)
from a `.dpdf`, or `None` if the member isn't present. Lets the semantic-search path pull a
space's f32 matrix without re-implementing the zip reader.

### render_html

```python
distillpdf.render_html(path: str, mode: str = "section", toc: bool = True) -> str
```

Render a `.dpdf` model to HTML with no source PDF present — the model-only re-render. `mode`
(`"section"` / `"page"`) and `toc` match [`Pdf.to_html`](#pdfto_html). The born-digital model
drops figure bytes, so figures render as the `image_mode="drop"` shape; the result is
byte-identical to `to_html(..., image_mode="drop")` on the source PDF. This is what
[`Doc.to_html`](#docto_html) delegates to.

### render_markdown

```python
distillpdf.render_markdown(
    path: str, mode: str = "section", toc: bool = True, image_mode: str = "external",
) -> str
```

Render a `.dpdf` model to Markdown with no source PDF present — the HTML→Markdown transform
over the model-only HTML. `mode` / `toc` match `to_html`; `image_mode` matches
[`Pdf.to_markdown`](#pdfto_markdown) (the model has no figure bytes, so `"external"` degrades
to caption placeholders). Backs [`Doc.to_markdown`](#docto_markdown).

### render_text

```python
distillpdf.render_text(path: str) -> str
```

Plain text of a `.dpdf` model in reading order (one page per line) — the model-only analogue
of [`Pdf.extract_text`](#pdfextract_text). Backs [`Doc.text`](#doctext).

### \_\_version\_\_

```python
distillpdf.__version__: str
```

The installed package version string, set by the compiled core.

---

## Pdf

The pure-Rust core class, defined via `#[pymethods]` in `src/lib.rs`. You normally hold a
[`Document`](#document) rather than a `Pdf` directly — `Document` wraps a `Pdf` and forwards
every method below by delegation (so `doc.page_count()`, `doc.extract_tables()`, etc. all
work). The methods are documented here once, on `Pdf`; `Document` overrides only
[`to_html`](#documentto_html) / [`to_markdown`](#documentto_markdown) /
[`to_pdf`](#documentto_pdf) to add the OCR workflow.

### Pdf.open

```python
Pdf.open(path: str) -> Pdf          # staticmethod
```

Load and parse a PDF container from a path. Rendering/extraction happens lazily in the output
methods. Raises `ValueError` on a read/parse failure.

### Pdf.from_bytes

```python
Pdf.from_bytes(data: bytes) -> Pdf  # staticmethod
```

Load and parse a PDF from raw bytes. With no source path, the output methods require an
explicit `path` (writing the default `<source>.ext` has nowhere to go).

### Pdf.page_count

```python
Pdf.page_count() -> int
```

The number of pages.

### Pdf.extract_text

```python
Pdf.extract_text() -> str
```

Plain text from all pages, concatenated in page order (one trailing newline per page). Hybrid:
the ToUnicode-aware content-stream extractor is primary (CID fonts, diacritics, reading
order), with a per-page fallback to lopdf's extractor when ours yields little.

### Pdf.extract_page_text

```python
Pdf.extract_page_text(page: int) -> str
```

Text of a single 1-indexed `page` (same hybrid strategy). Raises `ValueError` (`no page N`)
when the page does not exist.

### Pdf.extract_images

```python
Pdf.extract_images() -> list[dict]
```

Images from all pages as a list of dicts including the raw image bytes.

### Pdf.extract_fonts

```python
Pdf.extract_fonts() -> list[dict]
```

Per-page font information as a list of dicts.

### Pdf.extract_tables

```python
Pdf.extract_tables() -> list[dict]
```

Detected tables from all pages, each a dict with its cell grid.

### Pdf.extract_links

```python
Pdf.extract_links() -> list[dict]
```

Hyperlinks from all pages. Each dict:

| Key | Type | Meaning |
| --- | --- | --- |
| `page` | int | 1-based page number |
| `rect` | list[float] | `[x0, y0, x1, y1]` link rectangle |
| `kind` | str | `"uri"` or `"internal"` |
| `uri` | str \| None | target URL (URI links) |
| `dest_page` | int \| None | target page (internal links) |
| `dest_name` | str \| None | named destination (internal links) |

### Pdf.to_html

```python
Pdf.to_html(
    path: str | None = None,
    return_string: bool = False,
    mode: str = "section",
    toc: bool = True,
    image_mode: str = "embed",
)
```

Render to AI-ready HTML. By default this **writes a file and returns `1`** — to `path` if
given (a file, or a directory to place `<source-stem>.html` in), otherwise `<source>.html`
beside the opened PDF. With `return_string=True` it returns the HTML string and writes nothing.

| Parameter | Default | Notes |
| --- | --- | --- |
| `mode` | `"section"` | `"section"` or `"page"` structure |
| `toc` | `True` | `False` drops the `<nav>` table of contents |
| `image_mode` | `"embed"` | `"embed"` (inline base64 `data:` URIs), `"external"` (figures to a sibling `img/` folder), `"drop"` (`<image N>` placeholders) |

A returned string has no folder to write into, so `"external"` falls back to `"embed"`.

!!! note
    [`Document.to_html`](#documentto_html) overrides this with the same signature plus
    `ocr` / `engine` / `backend` for the OCR workflow.

### Pdf.to_markdown

```python
Pdf.to_markdown(
    path: str | None = None,
    return_string: bool = False,
    mode: str = "section",
    toc: bool = True,
    image_mode: str = "external",
)
```

Render to Markdown (a transform over the same HTML `to_html` emits). File output works like
`to_html`: by default writes `<source>.md` (or `path`) and returns `1`; `return_string=True`
returns the string. `image_mode` defaults to `"external"`; a returned string falls back to
caption-only placeholders.

### Pdf.to_pdf

```python
Pdf.to_pdf(
    path: str,
    ocr: dict[int, str] | None = None,
    remove_raster: bool = False,
) -> int
```

Write a searchable PDF from OCR results. `ocr` is a `{1-based page: DocTags}` map from one
model pass; when omitted, results cached on this object via [`set_ocr`](#pdfset_ocr) are used.

- `remove_raster=False` (default) — keep the original scan and add the OCR text as an
  **invisible**, selectable layer over it. OCR errors never destroy content (best for
  archival/legal use).
- `remove_raster=True` — clean reflow: rebuild OCR'd pages as visible text + cropped figure
  regions and drop the page raster (a much smaller file).

Non-OCR'd pages are kept verbatim. Returns `1`. Most users call
[`Document.to_pdf`](#documentto_pdf), which runs OCR for you.

### Pdf.distill

```python
Pdf.distill(path: str | None = None, assets: str = "figures") -> str
```

Distill the document into a `.dpdf` container — the durable analysis model (a zip of
`model.json` plus `img/` assets) you can re-render in milliseconds. Returns the written path.

`path`: an explicit `*.dpdf` file, a directory (→ `<source-stem>.dpdf` inside it), or `None`
(→ `<source>.dpdf` beside the opened PDF).

`assets` — the asset save profile:

| Value | Meaning |
| --- | --- |
| `"figures"` (default) | embed figure image bytes; page rasters dropped-with-stub (regenerable) |
| `"full"` | figures and (eventually) page rasters; equals `"figures"` on the born-digital path for now |
| `"none"` | text + structure only; all asset bytes dropped, regenerable stubs kept (emailable) |

!!! warning "Experimental schema"
    The model is `schema_version = 0` and experimental until the first downstream cutover. A
    dropped asset always keeps a stub (hash/dims/regen) — a reversible hole, re-extractable
    from the hash-bound source PDF. See [the .dpdf format](dpdf-format.md).

### Pdf.toc

```python
Pdf.toc(mode: str = "section") -> list[tuple[int, str, int, str]]
```

The detected-heading outline: `(level, title, page, anchor_id)` per heading, reading order.
`level` 1 is the title, 2 a section, 3 a subsection, …. `anchor_id` matches an `id=` in
`to_html` (link with `#anchor_id`). `mode="page"` carries real page numbers; `"section"`
yields `0`.

### Pdf.outline

```python
Pdf.outline() -> list[tuple[int, str, int, str]]
```

The PDF's **own** `/Outlines` bookmarks as `(level, title, page, anchor)` tuples. `level` is
1-based nesting depth; `page` is the 1-indexed target page (`0` if unresolved); `anchor` is the
`#page-N` fragment `to_html(mode="page")` exposes. Empty when the PDF has no outline. Distinct
from [`toc`](#pdftoc), which is built from detected headings.

### Pdf.section

```python
Pdf.section(name: str, mode: str = "section", image_mode: str = "embed") -> str | None
```

HTML of a single section: the heading matching `name` (its `sec-…` slug, an id prefix, or a
case-insensitive title substring) plus its content up to the next same-or-higher heading.
`None` if no match. The result is a string, so `image_mode="external"` behaves like `"embed"`.

### Pdf.metadata

```python
Pdf.metadata() -> dict
```

Structured front matter parsed from page 1:

```python
{
    "title": str,
    "authors": [{"name": str, "affiliation": str | None}],
    "affiliations": list,
    "abstract": str | None,
    "keywords": list[str],
}
```

Fields are empty/`None` when not detected.

### Pdf.ocr_plan

```python
Pdf.ocr_plan() -> list[dict]
```

Per page, whether OCR is needed and (if so) the page's main raster as image bytes for a
backend. Drives the `distillpdf.ocr` orchestrators. Each dict: `page`, `needs_ocr` (bool),
`reason` (str), `width_pts`, `height_pts`, `image` (bytes \| None).

### Pdf.set_ocr

```python
Pdf.set_ocr(ocr: dict[int, str]) -> int
```

Cache OCR results on this object: `ocr` is a `{1-based page: DocTags}` map from one model pass.
Once set, `to_pdf` and the `distillpdf.ocr` orchestrators reuse it, so the model runs **once**
regardless of how many outputs are produced. Merges into any existing cache. Returns the cached
page count.

### Pdf.get_ocr

```python
Pdf.get_ocr() -> dict[int, str]
```

The cached `{page: DocTags}` results, empty if [`set_ocr`](#pdfset_ocr) was never called.

### Pdf.has_ocr

```python
Pdf.has_ocr() -> bool
```

`True` when OCR results have been cached on this object.

---

## Document

```python
class distillpdf.Document
```

A loaded PDF — the Rust [`Pdf`](#pdf) core plus the OCR workflow as methods on the document.
Returned by [`open`](#open) / [`from_bytes`](#from_bytes). Every core method
(`page_count`, `extract_tables`, `toc`, `metadata`, …) is delegated to the wrapped `Pdf`; only
the three output methods below are overridden to fold OCR in. `repr` shows page count and OCR
state, e.g. `<distillpdf.Document pages=12 ocr=no>`.

### Document.open

```python
Document.open(path: str) -> Document   # classmethod
```

Construct from a path (what [`distillpdf.open`](#open) calls).

### Document.from_bytes

```python
Document.from_bytes(data: bytes) -> Document   # classmethod
```

Construct from raw bytes (what [`distillpdf.from_bytes`](#from_bytes) calls).

### Document.run_processing

```python
Document.run_processing() -> Document
```

Analyze the document — detect which pages are scanned / image-only and need OCR. Cheap and
cached; the render methods call it automatically. Chainable (returns `self`).

### Document.run_ocr

```python
Document.run_ocr(
    backend: OcrBackend | None = None,
    *,
    engine: str | None = None,
    only: set | None = None,
    progress: Callable[[int, int, int], None] | None = None,
    **config,
) -> Document
```

OCR every scanned page once, caching the result on this document. Afterwards the render methods
fold the recovered text into their output. Chainable.

| Parameter | Default | Notes |
| --- | --- | --- |
| `backend` | `None` | a constructed [`OcrBackend`](#ocrbackend) (power users); usually leave unset |
| `engine` | `None` | `None`/`"fast"` → bundled Tesseract; `"accurate"`/`"granite"` → granite-docling VLM; or a specific backend name |
| `only` | `None` | restrict to a set of 1-based page numbers |
| `progress` | `None` | `progress(page, done, total)` callback; `None` shows a tqdm bar on a TTY, `False` silences |
| `**config` | — | any [`OcrConfig`](#ocrconfig) field (`model_dir`, `hf_token`, `device`, `languages`, `prompt`, …) |

The default fast engine needs no extra, no download, and runs offline. The accurate engine
needs a runtime you install yourself — print the per-OS commands with
`distillpdf.ocr.install_help("granite")` (see [OCR runtime setup](../ocr-setup.md)).

```python
doc.run_ocr()                                   # fast (Tesseract)
doc.run_ocr(engine="granite", languages=["por"])
doc.run_ocr(engine="granite", model_dir=r"D:\models").to_html("out.html")
```

### Document.to_html

```python
Document.to_html(
    path: str | None = None,
    return_string: bool = False,
    mode: str = "section",
    toc: bool = True,
    image_mode: str = "embed",
    ocr: bool = False,
    engine: str | None = None,
    backend: OcrBackend | None = None,
)
```

Render to HTML. `path` / `return_string` / `mode` / `toc` / `image_mode` behave exactly as
[`Pdf.to_html`](#pdfto_html). Additions:

- `ocr=True` runs [`run_ocr`](#documentrun_ocr) first if it hasn't been (choose the engine with
  `engine=`, default fast/Tesseract).
- When the document has OCR results, scanned pages are rendered from the recovered text (page
  mode); otherwise this is the core extractor verbatim and un-OCR'd scanned pages emit a
  `warnings.warn` pointing at `run_ocr()`.
- A large `image_mode="embed"` output (> 50 MB) emits a warning suggesting `"external"` or
  `"drop"`.

### Document.to_markdown

```python
Document.to_markdown(
    path: str | None = None,
    return_string: bool = False,
    mode: str = "section",
    toc: bool = True,
    image_mode: str = "external",
    ocr: bool = False,
    engine: str | None = None,
    backend: OcrBackend | None = None,
)
```

Render to Markdown. As [`Pdf.to_markdown`](#pdfto_markdown), plus `ocr` / `engine` / `backend`
as on [`to_html`](#documentto_html). OCR-augmented output only when `ocr=True` or `run_ocr` was
already called.

### Document.to_pdf

```python
Document.to_pdf(
    path: str,
    remove_raster: bool = False,
    ocr: bool = False,
    engine: str | None = None,
    backend: OcrBackend | None = None,
) -> int
```

Write a searchable PDF. `ocr=True` OCRs scanned pages first (pick the engine with `engine=`,
default fast/Tesseract). `remove_raster` matches [`Pdf.to_pdf`](#pdfto_pdf): default keeps the
scan and adds an invisible text layer; `True` reflows to clean text and drops the raster. Warns
if no pages have been OCR'd (the result would otherwise just be the original).

---

## Doc

```python
class distillpdf.Doc
```

A loaded `.dpdf` document model — the single-document reading surface. Returned by
[`load`](#load) (or `Doc.load`). A thin wrapper over the internal `Model`; reading logic lives
in one place, so the Python API and the `distillpdf <file>.dpdf …` CLI cannot drift. `Doc` is
**immutable** (re-distill the source PDF to upgrade), except for [`embed`](#docembed), which
writes a new embedding space back into the file. `repr` shows the source, page/section/block
counts, and schema version.

!!! note "Query view vs fidelity view"
    [`section`](#docsection) / [`block_markdown`](#docblock_markdown) are the **query** view —
    block slices as simple markdown, addressable per section / block / page (what the CLI
    `read` verb prints). [`to_markdown`](#docto_markdown) / [`to_html`](#docto_html) /
    [`text`](#doctext) are the **fidelity** view — the whole-document model-only re-render,
    byte-identical to `render_*`. See [The .dpdf model](../guide/dpdf.md).

### Doc.load

```python
Doc.load(path: str) -> Doc   # classmethod
```

Load a `.dpdf` from disk. Raises [`DpdfError`](#dpdferror) with an actionable message when
`path` is not a readable `.dpdf` container.

### Doc.from_model

```python
Doc.from_model(model: Model, path: str | None = None) -> Doc   # classmethod
```

Wrap an already-parsed [`Model`](#model) (tests, in-memory pipelines). Pass `path` to keep the
fidelity renders ([`to_html`](#docto_html) / [`to_markdown`](#docto_markdown) /
[`text`](#doctext)) and [`embed`](#docembed) / [`search`](#docsearch) working; without it they
raise [`DpdfError`](#dpdferror).

### Doc.model

```python
Doc.model -> Model   # property
```

The underlying [`Model`](#model). An escape hatch for ad-hoc traversal; the accessors below
cover the documented surface.

### Doc.schema_version

```python
Doc.schema_version -> int   # property
```

The model schema version. `0` is experimental.

### Doc.source

```python
Doc.source -> dict   # property
```

The source binding: `{file, sha256, pages, distillpdf, generated_at}` — which PDF this model
was distilled from, by which distillpdf version, when.

### Doc.metadata

```python
Doc.metadata -> dict   # property
```

Parsed front matter: `{title, authors, abstract_text, keywords, …}` (fields the extractor
found; empty when none).

### Doc.pages

```python
Doc.pages -> list[dict]   # property
```

Per-page records: `n` (1-based), `width_pts` / `height_pts`, `labels` (the `/PageLabels` map,
e.g. `{"pdf": "xii"}`), and OCR fields when present.

### Doc.sections

```python
Doc.sections -> list[dict]   # property
```

The flat section list in document order: each `{id, level, title, parent, page_start,
page_end}`. The `id` threads into [`section`](#docsection) / [`find`](#docfind).

### Doc.toc

```python
Doc.toc() -> list[tuple[int, str, int, str]]
```

The section tree as `(level, title, page_start, id)` tuples — the `.dpdf` analogue of
[`Pdf.toc`](#pdftoc), but the trailing element is the **section id** (not an HTML anchor),
because that is what reads back through [`section`](#docsection).

### Doc.page_label

```python
Doc.page_label(n: int, scheme: str = "pdf") -> str | None
```

The label physical page `n` carries under `scheme` (default the `/PageLabels` `"pdf"` label,
e.g. `"xii"`), or `None` when the page has no such label.

### Doc.resolve_pages

```python
Doc.resolve_pages(spec: str) -> list[int]
```

Resolve a page spec (`"5"`, `"3-7"`, a label like `"xii"`, or a label range `"xii-xv"`) to
physical page numbers. Raises [`DpdfError`](#dpdferror) on an unresolvable token — a page that
isn't here is a loud error, never a silent empty.

### Doc.section

```python
Doc.section(sid: str) -> str
```

The section (and its whole subtree) as **query-markdown** — byte-identical to what
`distillpdf <file>.dpdf read <sid>` prints. Headings by level, paragraphs, list items, tables
as pipe-form, figure/caption placeholders. Raises [`DpdfError`](#dpdferror) (with the closest
section ids) on an unknown id.

### Doc.blocks

```python
Doc.blocks(
    kind: str | None = None,
    section: str | None = None,
    pages: list[int] | None = None,
) -> list[dict]
```

The blocks (reading-order units — the source of truth) matching every filter given. No filter →
every block. Each block is the raw model dict (`id`, `kind`, `text`, `page`, `section`,
`confidence`, `ocr_pass`, …).

| Parameter | Notes |
| --- | --- |
| `kind` | `"para"` / `"heading"` / `"table"` / `"figure"` / … |
| `section` | a section id; includes its subtree. Raises [`DpdfError`](#dpdferror) on an unknown id |
| `pages` | physical page numbers |

### Doc.block

```python
Doc.block(bid: str) -> dict
```

One block by id. Raises [`DpdfError`](#dpdferror) on an unknown id.

### Doc.block_markdown

```python
Doc.block_markdown(bid: str) -> str
```

A single block as query-markdown (what `read <block-id>` prints).

### Doc.tables

```python
Doc.tables() -> list[dict]
```

The table index: one `{id, label, page, page_label, caption}` per table, document order. The
`id` reads back via [`block`](#docblock) / [`block_markdown`](#docblock_markdown).

### Doc.figures

```python
Doc.figures() -> list[dict]
```

The figure index: one `{id, label, page, page_label, caption}` per figure.

### Doc.find

```python
Doc.find(
    query: str,
    *,
    regex: bool = False,
    section: str | None = None,
    kind: str | None = None,
    pages: list[int] | None = None,
    limit: int | None = None,
) -> FindResult
```

Lexical search over block text — case-insensitive substring by default, `regex=True` for a
regex. Scope with `section` / `kind` / `pages`. Returns a [`FindResult`](#findresult) carrying
the hits **and** the coverage accounting, so "not found" is a measured statement. `limit`
truncates the returned `hits` list (the coverage counts still reflect the full search); pass
`limit=None` (default) for every hit. Raises [`DpdfError`](#dpdferror) on an unknown `section`
id or an invalid regex. See [Searching documents](../guide/search.md).

### Doc.embed

```python
Doc.embed(
    space_id: str = "e1",
    *,
    cache_dir: str | None = None,
    batch_size: int = 32,
    progress: bool = True,
) -> dict
```

Derive chunks (if needed), embed every chunk's text with `BAAI/bge-m3`, write the vectors into
this `.dpdf` as embedding space `space_id`, then **save the file**. The document must have been
loaded from disk (it needs a path to write back). Returns
`{space, model, chunks, dimension, backend, dropped_stale}`.

- Atomic: vectors are computed in full before anything is written, so a mid-batch failure leaves
  the file untouched.
- Re-embedding an existing `space_id` overwrites it. A stale space (chunks changed since a prior
  embed) is dropped as the chunks are re-derived.
- Raises `distillpdf.embed.EmbedDependencyError` (with the install line) when the ONNX runtime
  is missing; raises [`DpdfError`](#dpdferror) when the document was built in memory (no path)
  or has no text blocks to embed.

The ~2.3 GB weights download on first use; see
[`distillpdf.embed.install_help`](#embedding-dependency-helpers).

### Doc.search

```python
Doc.search(query: str, *, k: int = 8, space: str | None = None) -> SearchResult
```

Semantic search: embed `query` with the same model and rank this document's chunks by cosine
similarity, returning the top `k` as a [`SearchResult`](#searchresult). `space` selects an
embedding space by id (default: the first). Raises [`DpdfError`](#dpdferror) (pointing at
[`embed`](#docembed)) when no embedding space exists or the named space is unknown, and
`distillpdf.embed.EmbedDependencyError` when the runtime is missing.

### Doc.info

```python
Doc.info() -> dict
```

A compact roll-up of the model — the `info` CLI verb as data: `schema_version`, `source`
(`file`, `sha256_prefix`, `distillpdf`, `generated_at`), counts (`pages`, `sections`, `blocks`,
`tables`, `figures`, `footnotes`), `ocr` state, the `assets` storage profile, index `coverage`,
and an `embeddings` summary (per-space model + chunk count, and whether the chunking is
`stale`).

### Doc.ocr_passes

```python
Doc.ocr_passes() -> list[dict]
```

The recorded OCR passes (append-only history): each `{id, engine, params, results, …}`. Empty
for a born-digital model.

### Doc.ocr_status

```python
Doc.ocr_status() -> dict
```

Per-page OCR decisions and per-pass outcomes — the `ocr-status` CLI verb as data:

```python
{
    "pages":  [{"page", "decision", "active_pass"}],
    "passes": [{"pass", "engine", "page", "outcome", "legible_chars", "confidence"}],
}
```

Both lists empty for a born-digital model.

### Doc.to_html

```python
Doc.to_html(
    path: str | None = None,
    return_string: bool = False,
    *,
    mode: str = "section",
    toc: bool = True,
) -> str
```

Re-render the **fidelity** HTML from the model — byte-identical to
[`render_html`](#render_html). `return_string=True` returns the HTML and writes nothing;
otherwise it writes a file (`path`, or `<stem>.html` beside the `.dpdf`) and returns the path.
Figures render as drop-placeholders (the born-digital model carries no figure bytes). Raises
[`DpdfError`](#dpdferror) if the `Doc` was built in memory with no path.

### Doc.to_markdown

```python
Doc.to_markdown(
    path: str | None = None,
    return_string: bool = False,
    *,
    mode: str = "section",
    toc: bool = True,
    image_mode: str = "external",
) -> str
```

Re-render the **fidelity** Markdown from the model — byte-identical to
[`render_markdown`](#render_markdown). `return_string=True` returns the string; otherwise writes
`<stem>.md` (or `path`) and returns the path. The model has no figure bytes, so `"external"`
degrades to caption placeholders. Raises [`DpdfError`](#dpdferror) if built in memory.

### Doc.text

```python
Doc.text() -> str
```

The document's plain text in reading order (one page per line) — byte-identical to
[`render_text`](#render_text). Raises [`DpdfError`](#dpdferror) if built in memory.

---

## SearchResult

```python
class distillpdf.doc.SearchResult
```

The result of [`Doc.search`](#docsearch) — the ranked chunk hits plus honest-coverage
accounting. Attributes (`__slots__`):

| Attribute | Type | Meaning |
| --- | --- | --- |
| `query` | str | the search query |
| `hits` | list[dict] | ranked hits (see below) |
| `space` | str | the embedding space id searched |
| `model` | str | the embedding model (`BAAI/bge-m3`) |
| `searched` | int | number of chunks searched |
| `stale` | bool | `True` when the chunking drifted from the current blocks since embedding — scores reflect an out-of-date chunking; refresh with [`embed`](#docembed) |

Each hit is a dict:

```python
{
    "chunk_id": str, "score": float,          # cosine, rounded to 6 dp
    "section": str | None,
    "page_start": int, "page_end": int,
    "page_label_start": str | None, "page_label_end": str | None,
    "block_ids": list[str],                   # thread into read / Doc.block
    "snippet": str,                           # recomposed chunk text
}
```

---

## FindResult

```python
class distillpdf.FindResult
```

The result of [`Doc.find`](#docfind) — the matches plus the coverage accounting, so `find`
never silently truncates and "not found" is paired with how much was searched. Attributes
(`__slots__`):

| Attribute | Type | Meaning |
| --- | --- | --- |
| `hits` | list[Hit] | the matches (see [`Hit`](#hit)) |
| `searched_blocks` | int | how many blocks were searched |
| `searched_pages` | int | how many distinct pages were spanned |
| `no_text_pages` | list[int] | pages that were in scope but carried no searchable text |

### Hit

```python
class distillpdf.dpdf.Hit
```

One `find` match (in `FindResult.hits`). Attributes (`__slots__`):

| Attribute | Type | Meaning |
| --- | --- | --- |
| `block` | dict | the block the match occurred in |
| `page` | int \| None | the block's page |
| `start` | int | match start offset in the block's searchable text |
| `end` | int | match end offset |
| `text` | str | the block's searchable text |
| `block_id` | str | property → `block["id"]` |

---

## Model and DpdfError

`distillpdf.dpdf` is the lower-level reader under [`Doc`](#doc). `FindResult` (above) lives
here too. Most code should use `Doc`; reach for `Model` only for direct model traversal.

### Model

```python
class distillpdf.dpdf.Model
```

A parsed `.dpdf` model. Construct with `Model.load(path)` (calls the Rust loader; raises
[`DpdfError`](#dpdferror) on a bad container) or `Model.from_dict(data)` (an already-parsed
dict; raises [`DpdfError`](#dpdferror) when required keys — `source`, `pages`, `sections`,
`blocks`, `indexes` — are missing).

Public properties: `raw`, `schema_version`, `source`, `metadata`, `pages`, `sections`,
`blocks`, `indexes`, `assets`, `ocr_passes`, `chunks`, `embedding_spaces`.

Public methods (selected — these back the [`Doc`](#doc) accessors):

| Method | Returns | Notes |
| --- | --- | --- |
| `section_by_id(sid)` | dict \| None | |
| `block_by_id(bid)` | dict \| None | |
| `section_ids()` / `block_ids()` | list[str] | |
| `blocks_for_section(sid)` | list[dict] | the subtree's blocks, reading order |
| `blocks_for_pages(pages)` | list[dict] | |
| `page_label(n, scheme="pdf")` | str \| None | |
| `resolve_page_range(spec)` | list[int] | raises [`DpdfError`](#dpdferror) on an unresolvable token |
| `find(query, *, regex=False, section=None, kind=None, pages=None)` | FindResult | |
| `derive_chunks()` | dict | a fresh `{policy, items}` chunk derivation |
| `chunks_stale()` | bool | stored chunks differ from a fresh derive |
| `chunk_text(chunk)` | str | recompose a chunk's text from its blocks |
| `space_by_id(space_id)` | dict \| None | |
| `section_markdown(sid)` / `blocks_markdown(blocks)` | str | the query-markdown emitter |
| `closest_section_ids(sid, n=3)` | list[str] | for actionable errors |

Module-level helpers: `derive_chunks(blocks)`, and the constants `CHUNK_TARGET_TOKENS` (`400`)
and `CHUNK_POLICY` (`"sec-contig-v1:tgt400"`).

### DpdfError

```python
class distillpdf.DpdfError(ValueError)
```

Raised when a `.dpdf` could not be read as a model (not a container, wrong shape) or a lookup
fails (unknown section/block id, unresolvable page spec, semantic search with no embedding
space). Subclass of `ValueError`.

---

## The ocr module

`distillpdf.ocr` is always importable; the heavy per-engine dependencies load lazily inside a
backend, so a base install gives a precise, actionable error the moment OCR is used. The
package re-exports [`OcrBackend`](#ocrbackend), [`OcrConfig`](#ocrconfig),
[`OcrDependencyError`](#ocrdependencyerror), [`available_backends`](#available_backends), and
[`get_backend`](#get_backend) at the top level; [`install_help`](#install_help) lives on the
module. See [OCR for scanned PDFs](../guide/ocr.md) and
[OCR runtime setup](../ocr-setup.md).

### OcrConfig

```python
@dataclass
class distillpdf.ocr.OcrConfig
```

Configuration common to every backend. Fields an engine doesn't use are ignored, so this stays
one shared dataclass.

| Field | Type | Default | Meaning |
| --- | --- | --- | --- |
| `model_id` | str \| None | `None` | backend-specific model id (e.g. a HF repo id) |
| `model_dir` | str \| None | `None` | download directory; `None` → engine default (the granite GGUF engine uses a visible `./ocr_model`) |
| `hf_token` | str \| None | `None` | HF token for gated/private models; default public models need none |
| `env_file` | str \| None | `None` | `.env` file to read `HF_TOKEN` from; `None` → auto-detect `./.env` (only `HF_TOKEN` is read) |
| `store_token` | bool | `False` | if `True` with an `hf_token`, persist it to the `.env` (perms 0600, plaintext) |
| `device` | str | `"auto"` | `"auto"` / `"cpu"` / `"metal"` / `"cuda"` |
| `prompt` | str | `"Convert this page to docling."` | model instruction |
| `max_tokens` | int | `4096` | generation cap per page (a backstop; `stop_strings` normally terminate) |
| `stop_strings` | list[str] | `[]` | strings that end generation |
| `languages` | list[str] | `[]` | ISO codes (e.g. `["eng", "por"]`, Tesseract); empty = engine default |
| `dpi` | int \| None | `None` | render-resolution hint; `None` = engine default |

### OcrBackend

```python
class distillpdf.ocr.OcrBackend
```

Abstract OCR backend: a page image → DocTags string. The Rust core renders the DocTags. Build
your own by subclassing and registering it. Class-level capability attributes are deliberately
cheap to read (inspecting them never triggers the engine's heavy imports):

| Attribute | Type | Default | Meaning |
| --- | --- | --- | --- |
| `name` | str | `"abstract"` | registry name, shown in errors |
| `output` | str | `"doctags"` | the wire contract dialect |
| `tier` | str | `"accurate"` | `"fast"` or `"accurate"` |
| `structure_aware` | bool | `True` | emits tables (OTSL) / tagged headings vs flat text (advisory only) |
| `bundled` | bool | `False` | works on a base install — no extra, no download |
| `offline` | bool | `False` | no network / model download at use time |
| `languages` | tuple | `()` | ISO codes it can read; `()` = any/unspecified |
| `detail` | str | `""` | one-line human description |

Constructor and methods:

```python
OcrBackend(config: OcrConfig | None = None, **kwargs)   # **kwargs build an OcrConfig
backend.ocr_page(image: bytes) -> str        # DocTags for one page image (override this)
backend.prepare(samples: list[bytes]) -> None  # one-time setup (default no-op)
backend.close() -> None                       # release held resources
backend.is_available() -> bool                # classmethod; can it run here?
backend.descriptor() -> OcrCapabilities       # classmethod; capabilities without heavy imports
```

`OcrBackend` is a context manager (`__enter__` / `__exit__` calls `close`).

### OcrCapabilities

```python
@dataclass(frozen=True)
class distillpdf.ocr.OcrCapabilities
```

What an engine is and can do, read straight off the backend class (no heavy imports, no model
download). Fields: `name`, `tier`, `structure_aware`, `bundled`, `offline`, `languages`,
`available`, `output` (`"doctags"`), `detail`. Returned by `OcrBackend.descriptor()` and
`distillpdf.ocr.backend_descriptors()`.

### OcrDependencyError

```python
class distillpdf.ocr.OcrDependencyError(ImportError)
```

Raised when an OCR backend's optional dependencies are not installed. The message carries the
exact per-OS install commands (see [`install_help`](#install_help)).

### available_backends

```python
distillpdf.ocr.available_backends() -> list[str]
```

The sorted names of every registered backend.

### get_backend

```python
distillpdf.ocr.get_backend(
    name: str | None = None, *, tier: str | None = None, **kwargs,
) -> OcrBackend
```

Construct a backend. Precedence: explicit `name` > `tier` > the platform default (the fast
tier). `**kwargs` build an [`OcrConfig`](#ocrconfig). Raises
[`OcrDependencyError`](#ocrdependencyerror) when the chosen engine's optional deps are missing
(with the exact install command), or `ValueError` on an unknown backend name.

### backend_for

```python
distillpdf.ocr.backend_for(engine=None, **kwargs) -> OcrBackend
```

Resolve an `engine` into a backend — the entry point behind `to_html(ocr=True, engine=...)` and
`run_ocr(engine=...)`. `engine` may be `None`/`"fast"` (bundled Tesseract, the default),
`"accurate"` or `"granite"` (the granite-docling VLM), a specific registered backend name, or
an already-constructed [`OcrBackend`](#ocrbackend) (returned as-is).

### install_help

```python
distillpdf.ocr.install_help(engine: str | None = None) -> str
```

What to install to use an OCR engine — the user-facing helper. Accepts a selector:
`None`/`"fast"` (bundled, nothing to install), `"accurate"`/`"granite"`, or a backend name.
Returns the pip commands for your platform (accurate tiers) or a "nothing to install" line (the
bundled fast Tesseract engine).

```python
print(distillpdf.ocr.install_help("granite"))
```

### Orchestration functions

The `distillpdf.ocr` module also exposes the orchestrators that [`Document`](#document) uses
(call these directly only if you hold a raw [`Pdf`](#pdf)):

```python
distillpdf.ocr.run(pdf, backend=None, *, only=None, progress=None) -> dict[int, str]
distillpdf.ocr.to_html(pdf, backend=None, *, path=None, return_string=True, image_mode="embed") -> str
distillpdf.ocr.to_markdown(pdf, backend=None, *, path=None, return_string=True, toc=True, image_mode="drop") -> str
distillpdf.ocr.to_pdf(pdf, backend=None, path=None, *, remove_raster=False) -> str
```

`run` OCRs every scanned page once and caches the `{page: DocTags}` map on the `pdf`; the
others reuse that cache (running `backend` once if nothing is cached). For the accurate tier the
bundled Tesseract engine acts as a cheap **gate** — the heavy granite model only runs on pages
Tesseract confirms hold text.

!!! note "Fast tier is flat text only"
    The bundled Tesseract engine emits flat text (no table structure or tagged headings). For
    structure-aware output (OTSL tables) use the accurate granite engine.

### Embedding dependency helpers

Semantic search ([`Doc.embed`](#docembed) / [`Doc.search`](#docsearch)) uses
`distillpdf.embed`:

```python
distillpdf.embed.install_help() -> str           # pip line for the ONNX embedding runtime
distillpdf.embed.runtime_available() -> bool      # is the runtime importable?
class distillpdf.embed.EmbedDependencyError(ImportError)
distillpdf.embed.MODEL_ID = "BAAI/bge-m3"
distillpdf.embed.DIMENSION = 1024
```

`EmbedDependencyError` is raised (with the exact `pip install` line) when the optional runtime
(`onnxruntime` / `tokenizers` / `huggingface_hub`) is missing.
