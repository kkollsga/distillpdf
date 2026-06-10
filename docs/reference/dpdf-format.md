# .dpdf format reference

A `.dpdf` is the on-disk form of distillPDF's document model: a deterministic zip
holding one `model.json` plus a handful of binary members. This page describes the
*current* layout for anyone who wants to read, inspect, or build a tool against one. For
the design rationale behind the shape, see [Design notes, data model](../datamodel-design.md);
for how you produce and use a `.dpdf`, see [The .dpdf document model](../guide/dpdf.md).

!!! warning "schema_version 0 is experimental"
    Every `.dpdf` written today carries `schema_version: 0`. The shape is **not** a
    stability commitment — it goes to `1` only after the first downstream cutover survives
    it. Treat every field as provisional. This page is a description of the format as it
    stands, not a promise that it will not change.

## Container layout

The container is a hand-rolled **STORE-only** (uncompressed) zip — a strict subset of
PKZIP that any unzip tool or Python's `zipfile` reads. Image bytes (PNG/JPEG) are already
compressed, so deflate would buy little, and STORE keeps the writer tiny and pure-Rust.

Members:

| Member | Contents |
| --- | --- |
| `model.json` | The whole document model (always present, always first in the archive). |
| `img/…` | Embedded asset bytes — figure images, vector→SVG fragments — named by the asset id (e.g. `img/fig_03.png`). Present only for assets stored `embedded`. |
| `embeddings/<id>.bin` | One embedding space's vectors as a raw little-endian f32 matrix, row-major. Present only after the document has been embedded. |

The archive is **deterministic**: `model.json` is written first, every other member is
sorted by name, the JSON keys are sorted at every level, and the zip's date/time fields are
zeroed. Save → load → save is byte-identical, including when embedding members are present.

`external` assets are written to a sibling directory next to the `.dpdf` (default
`<stem>_assets/`) and referenced from the model rather than carried inside the container;
`dropped` assets carry no bytes at all (see [Asset records](#asset-records)).

You can read the model JSON without unzipping by hand:

```python
import json
import distillpdf

model = json.loads(distillpdf.load_model("paper.dpdf"))
print(model["schema_version"], model["source"]["file"])

# A single binary member (e.g. an embedding matrix), or None if absent:
raw = distillpdf.read_dpdf_member("paper.dpdf", "embeddings/e1.bin")
```

`load_model` returns the canonical, sorted-key `model.json` as a string. `read_dpdf_member`
returns the member's raw bytes or `None`.

## Top-level model.json shape

`model.json` is a single JSON object. Its keys:

| Key | Type | Notes |
| --- | --- | --- |
| `schema_version` | integer | Currently always `0` (experimental). |
| `source` | object | The source PDF binding (see below). |
| `metadata` | object | Document front-matter; sparse. |
| `pages` | array | One record per physical page. |
| `ocr_passes` | array | Append-only OCR history; empty for a born-digital distill. |
| `sections` | array | Flat list of heading-tree sections. |
| `blocks` | array | Content blocks in reading order — the heart of the model. |
| `indexes` | object | DERIVED views over `blocks` (page / section / kinds / coverage). |
| `assets` | array | Binary-asset records (figures, page rasters, SVG). |
| `chunks` | object | DERIVED chunk grouping; omitted until first derived. |
| `embedding_spaces` | array | Embedding-space metadata; omitted/empty until embedded. |
| `links` | array | Hyperlinks (external URI or internal destination). |
| `named_dests` | array | PDF named destinations (label → page). |
| `toc` | array | Table-of-contents entries (PDF `/Outlines` or detected headings). |

Optional fields are omitted from the JSON when empty, so a born-digital document with no
OCR, chunks, or embeddings simply has no `chunks` key and an empty (or absent)
`embedding_spaces`.

### source

Binds the model to its source PDF by hash and records the extractor version and the one
timestamp in the file.

| Field | Type | Notes |
| --- | --- | --- |
| `file` | string | The source PDF's file name. |
| `sha256` | string | Hash of the source PDF (the regen contract for dropped assets). |
| `pages` | integer | Page count. |
| `distillpdf` | string | The distillPDF version that produced the model. |
| `generated_at` | string | ISO timestamp, taken once at distill time. |

### metadata

Sparse by design — only what the extractor understood. All fields optional:
`title` (string), `authors` (array of strings), `language` (string),
`abstract_text` (string), `keywords` (array of strings).

### pages

| Field | Type | Notes |
| --- | --- | --- |
| `n` | integer | 1-based physical page number. |
| `width_pts`, `height_pts` | float | Page dimensions in PDF points. |
| `labels` | object | Map of label scheme → label; the core fills `"pdf"` from `/PageLabels` when present. Omitted when empty. |
| `ocr_decision` | string | `NotNeeded` / `NeedsOcr` / `DropAndOcr`, when an OCR decision was made. Optional. |
| `active_ocr_pass` | string | Id of the OCR pass feeding this page's blocks (`null`/absent = native text layer). Optional. |

A page's content is **not** stored on the page record — it lives entirely in `blocks`,
addressable via `indexes.pages`. Nothing is duplicated.

### sections

A flat list; `parent` links rebuild the tree. The `id` is the same `sec-…` slug the HTML
renderer mints, so model ids equal HTML ids equal the CLI/agent addresses.

| Field | Type | Notes |
| --- | --- | --- |
| `id` | string | `sec-…` slug. |
| `level` | integer | Heading depth. |
| `title` | string | Section title. |
| `parent` | string | Parent section id, or absent for a top-level section. |
| `page_start`, `page_end` | integer | Page span. |

### blocks

One content block per record, in reading order. `id` is a `b0001`-style ordinal scoped to
the file.

| Field | Type | Notes |
| --- | --- | --- |
| `id` | string | `b0001`-style reading-order id. |
| `kind` | string | One of `heading`, `para`, `list_item`, `table`, `figure`, `caption`, `footnote`, `code`, `header`, `dest_anchors`. |
| `text` | string | The block's minimal inline HTML (`<b>/<i>/<a>/<sup>/<sub>/<code>`). Omitted when empty. |
| `page` | integer | Physical page number. |
| `section` | string | Owning section id, or absent (unsectioned). |
| `bbox` | array | `[x0, y0, x1, y1]` in PDF user space (origin bottom-left, points). Optional. |
| `confidence` | float | `1.0` = native text layer; lower means OCR-derived. |
| `ocr_pass` | string | Id of the OCR pass that produced this text (absent = native). |
| `heading_level` | integer | 1–6, for `kind = heading`. |
| `cells` | array | Row-major cell grid, for `kind = table` (the query view). |
| `image` | string | Asset id (`img/…`) of a `figure`'s image, when extracted. |
| `label` | string | Parsed element label (`"Table 3"`, `"Figure 1"`). |
| `caption` | string | Figure/table caption text. |

`text` carries inline markup, never block-level tags; the query views in
[`distillpdf.dpdf`](../reference/python.md) strip it to plain text.

Blocks also carry **fidelity-reconstruction** fields used to rebuild the byte-identical
HTML from the model alone — `list_ordered`, `el_group`, `table_header`, `table_grid`,
`table_caption`, and `el_html`. These are render data, distinct from the query fields above;
the `header` and `dest_anchors` kinds are fidelity-only page-chrome carriers, not content.

### indexes

DERIVED views over `blocks`, regenerable by the extractor's deterministic pass. They are
stored so the JSON is self-describing, but they can never drift: a save that finds the
stored indexes disagree with a fresh derive is a loud error, not a silent coverage hole.

| Key | Shape | Notes |
| --- | --- | --- |
| `pages` | object | Page number (string key) → block ids on that page. |
| `sections` | object | Section id → block ids. |
| `kinds` | object | Kind name → labelled entries `{id, page, label}`, only for `table` / `figure` / `footnote`. |
| `coverage` | object | `{sectioned: float, unsectioned_blocks: [ids]}` — the sectioned fraction plus the explicit unsectioned bucket (front-matter before the first heading is legitimately unsectioned, not an error). |

### ocr_passes

An append-only history; OCR text is cheap, so every pass is kept and `page.active_ocr_pass`
selects which feeds the blocks. Each pass: `id`, `engine`, optional `params` (string map),
optional `generated_at`, and `results` — one per page with `page`, `outcome`
(`ocr_ok` / `ocr_partial` / `ocr_illegible`), `legible_chars` (the trustworthy
alphanumeric count, distinct from raw string length), optional `confidence`, and the model
output under `text` or `doctags`. See [OCR for scanned PDFs](../guide/ocr.md).

## Asset records

Each `asset` record carries a `storage` mode so size is always a deliberate choice. Every
profile keeps the asset **stub** (hash, dims, a regen recipe) — only the bytes differ, so a
dropped asset is a named, reversible hole rather than missing content.

| Field | Type | Notes |
| --- | --- | --- |
| `id` | string | Asset id, which doubles as the in-container path (e.g. `img/fig_03.png`). |
| `kind` | string | `figure`, `page_raster`, or `svg`. |
| `storage` | string | `embedded`, `external`, or `dropped`. |
| `sha256` | string | Content hash. Optional. |
| `bytes` | integer | Byte length. Optional. |
| `width`, `height` | integer | Pixel dimensions. Optional. |
| `regen` | object | How to rebuild from the source PDF: `{page, dpi?}`. Optional. |

The `assets="…"` profile chosen at distill time decides which bytes are kept:

- `none` — text and structure only; all asset bytes dropped (stubs remain). A few MB even
  for a 1,500-page scan.
- `figures` (default) — figure image bytes embedded; page rasters stay dropped-with-stub
  (they are regenerable).
- `full` — figures and page rasters. The born-digital path has no page-raster capture yet,
  so this currently equals `figures`; the variant exists so the surface is stable.

A dropped or external asset can be regenerated from the source PDF: the `source.sha256`
verifies you have the right file, and the asset's `regen` recipe (`page` + `dpi`) says how
to redraw it.

## Chunks: derived from blocks

`chunks` groups consecutive blocks **within one section** toward a token target so a
semantic-search hit lands on a coherent passage rather than a bare sentence. Chunks are
DERIVED, like the indexes — a chunk stores only block-id addresses and page spans, never
text; chunk text is recomposed from the blocks at embed/search time.

```json
{
  "policy": "sec-contig-v1:tgt400",
  "items": [
    {
      "id": "c0001",
      "block_ids": ["b0001", "b0002"],
      "section": "sec-intro",
      "page_start": 1,
      "page_end": 1
    }
  ]
}
```

The grouping rule: a contiguous run within one section accumulates until it reaches roughly
400 tokens (a cheap chars/4 proxy — the real tokenizer is never loaded just to chunk), then
a new chunk starts. A section boundary always starts a new chunk, and a single block larger
than the target is its own chunk (a block is never split). Chunk ids are `c0001`-style.

The `policy` string makes the derivation reproducible and checkable.

!!! note "Staleness contract"
    Chunking is deterministic: the same blocks always derive the same chunks. So a stored
    `chunks` set can be diffed against a fresh derive to detect drift. If the blocks changed
    since chunking (different policy, or a different item list), the stored chunks are
    **stale** — and any embedding space built on them is stale too. Re-embedding re-derives
    chunks as its single regeneration point, so a freshly written space's `chunk_ids` always
    match the current blocks.

## Embedding spaces

Vectors are **not** stored in `model.json`. Each embedding space records only metadata; the
vectors live in a container member as a raw little-endian f32 matrix, row-major, with
`len(chunk_ids)` rows of `dimension` columns each, rows in `chunk_ids` order.

A space record (`embedding_spaces` is an array; a typical file has one):

| Field | Type | Notes |
| --- | --- | --- |
| `id` | string | Space id (default `e1`). |
| `model` | string | The embedding model, recorded verbatim — currently `BAAI/bge-m3`. |
| `dimension` | integer | Vector dimension (1024 for bge-m3). |
| `normalized` | boolean | `true` — vectors are L2-normalized, so cosine similarity is a plain dot product. |
| `member` | string | Container member holding the f32 matrix, e.g. `embeddings/e1.bin`. |
| `chunk_ids` | array | Chunk ids in row order — also the staleness key (must equal the current chunk ids). |
| `generated_at` | string | ISO timestamp. |
| `distillpdf_version` | string | The version that wrote the space. |

The bytes ride verbatim through save/load alongside asset bytes. A space is written only if
its member is present and exactly `len(chunk_ids) × dimension × 4` bytes — a missing or
mis-sized member is a loud save error, never a silently-shipped half-record. Multiple spaces
may coexist (re-embedding with a different model id); a space whose `chunk_ids` no longer
match the document's chunks is dropped on the next embed.

To read a matrix back, fetch the member with `read_dpdf_member` and unpack it as
little-endian f32 rows of `dimension` floats. See [Searching documents](../guide/search.md)
for the search path that uses it.

## See also

- [The .dpdf document model](../guide/dpdf.md) — producing and working with `.dpdf` files.
- [Searching documents](../guide/search.md) — lexical and semantic search.
- [Python API](../reference/python.md) — the `Doc` / `Model` accessors over a parsed model.
- [Design notes, data model](../datamodel-design.md) — the rationale behind the shape.
- Source on GitHub: <https://github.com/kkollsga/distillpdf>
