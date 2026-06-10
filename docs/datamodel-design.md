# The distillPDF document model (`.dpdf`) — design proposal

*Status: **engine track Waves 1–4 implemented** (the model + container + load,
renderers-from-model, the agent CLI, and the public `Doc` API + `distillpdf.load`).
`schema_version` is still `0` — the phase-4 downstream cutover (kglite-docs on the
1,564-page legal corpus) is the gate that promotes it to `1`. OCR-pass writing (phase 2)
and per-block bboxes are not yet filled (see the notes below). Origin: 2026-06-10
cross-library analysis with kglite-docs (the first downstream consumer). Companion to the
fidelity worklist in `ROADMAP.md` — this is the "engine track": making distillPDF the
extraction engine other systems build on, not just a converter.*

> **Implementation notes.**
> * **Render fidelity — the "renderers are pure functions of the model" property.** The
>   **blocks** array is the SINGLE source of truth — both the query/index view AND the render
>   view. There is no separate stored body: `render_html` REBUILDS the post-transform
>   page-element IR from the blocks and runs the SAME emit + merge + `assemble` tail the source
>   PDF path runs, so `render_html`/`render_markdown`/`render_text` reproduce the source PDF's
>   `to_html`/`to_markdown`/`extract_text` (with `image_mode="drop"`, the born-digital model
>   carrying no figure bytes) **byte-for-byte across both page AND section mode** (verified over
>   the whole corpus). The query-lossy parts are carried as dedicated FIDELITY fields on the
>   block: a figure/caption (and the page-chrome `header`/`dest_anchors` carriers) keep their
>   exact emitted `el_html` fragment; a table keeps its header/grid/caption parts; consecutive
>   `list_item`/`footnote` blocks share an `el_group` so they regroup into the one `<ul>/<ol>` /
>   `<aside>` they came from; and `block.text` is the element's minimal inline HTML. The
>   query-markdown view (`Doc.section`, the `read` verb) strips that markup and is addressable
>   per section/block/page; the fidelity render is whole-document — each is documented as which.
> * **`bbox` is threaded.** Every content block carries its `bbox` (`[x0,y0,x1,y1]` in PDF user
>   space, origin bottom-left), threaded from the render walk's positioned items and unioned
>   through the cross-page merges — 100% coverage of content blocks on the born-digital path.
>   Citations resolve to `(block id, section, physical page, page label, bbox)`.

## Why

distillPDF already builds a typed element tree per document — reading order,
headings, the section tree, table cells, figure regions, OCR DocTags — and then
renders it to HTML and **throws it away**. Every render re-pays the full
analysis cost (font profiling, heading detection, column-band table model,
OCR). For a 1,500-page scanned PDF with VLM/agent OCR, that cost is hours and
real money, and today the only durable outputs are *lossy* renders.

Persist the model instead. Distill once → re-render HTML / Markdown /
searchable PDF / plain text from the file, with different options, in
milliseconds, forever. The model file is also:

- **the contract for downstream embedders** (kglite-docs ingests `.dpdf`
  instead of re-parsing PDFs — replaces its pymupdf4llm dependency);
- **agent-queryable on its own** — for a *single* document, an agent with file
  tools + the accessors below needs no corpus machinery at all;
- **the OCR sidecar, generalized** — OCR text, provenance, confidence, and
  legibility live on the pages/blocks they produced.

Precedent: IBM Docling's `DoclingDocument` (lossless JSON model; HTML/MD are
exports). DocTags — which distillPDF already speaks — is the per-page
serialization of that model family; this generalizes it to document level, in
pure Rust, in a small wheel.

## What "lossless" means

Lossless w.r.t. the **analysis**, not the PDF bytes. The model captures
everything the extractor understood: structure, text, tables, figures, links,
OCR provenance, confidence, bboxes. The original PDF remains the source
artifact, bound by hash. Granularity: block-level + bbox by default; optional
`detail="spans"` for line-level boxes (pinpoint citations). Glyph-level
fidelity is explicitly out of scope.

Two consequences, stated honestly:

- A model file is a **snapshot of extractor quality at generation time**.
  Extraction improvements don't retroactively improve old files — re-distill to
  upgrade. `schema_version` + the recorded distillpdf version make this
  explicit.
- Anything the model *omits* must be a **named hole, never a silent one**
  (see asset stubs below). Same honest-coverage principle kglite-docs uses.

## Container & schema sketch

`.dpdf` = a zip container (like `.docx`/`.epub`): `model.json` + `img/` assets.
A pure-JSON single-file form (assets embedded as data URIs) mirrors the
existing `image_mode="embed"` for the self-contained case.

```jsonc
{
  "schema_version": 0,                  // 0 = experimental until the first
                                        // downstream cutover survives it
  "source": {"file": "case.pdf", "sha256": "…", "pages": 1564,
             "distillpdf": "0.0.33", "generated_at": "…"},
  "metadata": {…},                      // title, author, language, front matter

  "pages": [{
    "n": 1, "width_pts": 612, "height_pts": 792,
    "labels": {"pdf": "xii"},           // extensible: downstream may add e.g. "stamp": "fls. 249"
    "ocr_decision": "NeedsOcr",         // detect.rs: NotNeeded|NeedsOcr|DropAndOcr
    "active_ocr_pass": "p2"             // which pass feeds blocks/renders
  }],

  // ---- multiple OCR passes: append-only history, comparable ----
  "ocr_passes": [{
    "id": "p1", "pages": [1, 2, …],     // pages this pass covered
    "engine": "tesseract",              // or "granite-docling", "agent:claude-sonnet-4-6", …
    "params": {"dpi": 300, "tiles": 4, "prompt": "…"},
    "generated_at": "…",
    "results": [{"page": 1, "outcome": "ocr_partial",   // ok|partial|illegible
                 "legible_chars": 412, "confidence": 0.61,
                 "text": "…", "doctags": "…"}]
  }, {
    "id": "p2", "engine": "agent:claude-opus-4-8", …    // the escalation pass
  }],

  "sections": [{"id": "sec-1-introduction", "level": 1, "title": "…",
                "parent": null, "page_start": 1, "page_end": 4}],

  "blocks": [                           // reading order — the heart of it
    {"id": "b0001", "kind": "para", "text": "…", "page": 1,
     "section": "sec-1-introduction", "bbox": [72, 540, 540, 620],
     "confidence": 1.0,                 // 1.0 = native text layer
     "ocr_pass": null},                 // or "p2" — block-level OCR provenance
    {"id": "b0421", "kind": "table", "page": 12, "cells": [...],
     "label": "Table 3", "caption": "…"},
    {"id": "b0502", "kind": "figure", "page": 13, "image": "img/fig_03.png", "caption": "…"}
  ],

  "indexes": {                          // DERIVED views over blocks — stored for
    "pages":    {"1": ["b0001", "b0009"]},        // Tier-1 self-description, but
    "sections": {"sec-1-introduction": ["b0001", "b0042"]},  // regenerable: `reindex`
    "kinds":    {"table": [{"id": "b0421", "label": "Table 3", "page": 12}]},
    "coverage": {"sectioned": 0.97, "unsectioned_blocks": ["b0001"]}
  },

  // ---- asset policy: every binary is embedded | external | dropped ----
  "assets": [{
    "id": "img/fig_03.png", "kind": "figure",           // figure | page_raster | svg
    "storage": "external",              // embedded | external | dropped
    "sha256": "…", "bytes": 184302, "width": 1240, "height": 920,
    "regen": {"page": 13, "dpi": 300}   // how to rebuild it from the source PDF
  }],

  // ---- chunks: DERIVED, like indexes — no text duplication ----
  "chunks": {
    "policy": "sec-contig-v1:tgt400",   // the regeneration recipe (chars/4 ≈ token target)
    "items": [{"id": "c0001", "block_ids": ["b0001", "b0042"],
               "section": "sec-1-introduction", "page_start": 1, "page_end": 4}]
  },

  // ---- embedding spaces: ARTIFACT metadata; vectors live as a binary member ----
  "embedding_spaces": [{
    "id": "e1", "model": "BAAI/bge-m3", "dimension": 1024, "normalized": true,
    "member": "embeddings/e1.bin",      // little-endian f32, row-major, n_chunks × dim
    "chunk_ids": ["c0001", "c0002"],    // row order — AND the staleness key
    "generated_at": "…", "distillpdf_version": "0.0.33"
  }],

  "links": [...], "named_dests": [...], "toc": [...]
}
```

The container is then `model.json` + `img/…` assets + `embeddings/<id>.bin` vector matrices —
all STORE-only, deterministically ordered, so save→load→save stays byte-identical with
embeddings present.

### Embeddings & chunks (semantic search)

Semantic search is opt-in and self-contained — no corpus, no server, no source PDF:

- **Chunks are DERIVED, exactly like the indexes.** `derive_chunks(blocks)` groups consecutive
  blocks *within one section* toward a ~400-token target (a cheap `chars/4` proxy — the
  tokenizer is **not** loaded to chunk), never splitting a block; an oversize block is its own
  chunk. A chunk stores only addresses (`block_ids`) + spans, **never text** — the chunk text is
  recomposed from the blocks (inline markup stripped) at embed/search time. The `policy` string
  makes the derivation reproducible, so a stored set can be diff'd against a fresh derive to
  detect drift.
- **Vectors are an ARTIFACT, not a derivation.** They are BAAI/bge-m3 embeddings (1024-dim,
  L2-normalized) stored as a raw little-endian f32 matrix in a container member
  (`embeddings/<id>.bin`), `n_chunks × dim`, rows in the order of the space's `chunk_ids`. The
  bytes ride verbatim through save/load (like asset bytes); `embedding_spaces[]` records what
  they are. Multiple spaces may coexist (re-embed with a different model); a typical file has
  one. distillpdf uses kglite's own bge-m3 embedder when it's installed (identical vectors,
  zero duplication) and a faithful vendored twin of it otherwise.
- **Staleness is loud, never silent.** A space's `chunk_ids` are the staleness key: if the
  blocks change (re-distill, an OCR pass switch), a fresh chunk derive no longer matches and the
  space is **stale** — `info` flags it, `search` warns, and re-embedding **drops** the stale
  space rather than silently keeping out-of-date vectors. A space is also written all-or-nothing
  (full matrix or no space), so a mid-batch embed failure leaves the file untouched.

### Multiple OCR passes (append-only, comparable)

The real OCR workflow is iterative: a cheap bundled-Tesseract triage pass →
re-OCR illegible pages with a VLM → escalate decisive pages to the strongest
agent model. The model keeps **every pass** (text is cheap — KB/page):

- **`active_ocr_pass` per page** decides which pass feeds `blocks` and renders;
  default = best outcome, then latest. Switching the active pass and
  re-deriving blocks is an operation, not a re-OCR.
- **Comparison affordances:** diff two passes for a page; legibility delta per
  pass; an **agreement score** between independent engines (char-level
  similarity). Two engines agreeing is itself a trust signal; disagreement
  flags a page for review. No mainstream tool stores this.
- **Audit/chain-of-custody:** for legal/forensic work, "what did the text layer
  say at the time, and which model produced it" is answerable from the file.
- Blocks derived from OCR carry `ocr_pass`, so provenance survives into every
  render and every downstream system.

### Asset policy (externalize / drop — size is a choice, never a surprise)

Each binary asset (figure images, page rasters captured for OCR, vector→SVG
renders) carries a `storage` mode:

- **`embedded`** — in the container; fully self-contained file.
- **`external`** — referenced path next to the file (the existing
  `image_mode="external"` layout).
- **`dropped`** — the bytes are gone, but a **stub remains**: hash, size,
  dimensions, kind, and a `regen` recipe. A dropped asset is a *named* hole —
  structurally lossless, observably incomplete, and **reversible**:
  `doc.rehydrate("case.pdf")` re-extracts dropped assets from the
  hash-verified source PDF.

Save profiles make sharing trivial:

```python
doc.save("case.dpdf")                          # assets="figures"  (default)
doc.save("case-full.dpdf",  assets="full")     # + page rasters (audit archive)
doc.save("case-text.dpdf",  assets="none")     # text+structure only — a few MB
                                               # for a 1,500-page scan; emailable
```

Page rasters (the images OCR actually read) default to *dropped with stubs* —
they're regenerable — but `assets="full"` keeps them for evidentiary use
("this is the image the model read").

## Indexes — derived, never authoritative

The classic failure mode of document indexes is **drift**: separately
maintained page/table/section structures that quietly disagree with the
content after an OCR pass or re-parse. The fix is structural:

- **One source of truth: `blocks`** (reading order, stable ids `b0421` —
  scoped to the file; cross-file stability is the re-distill snapshot
  question, already accepted). Every index is a **derived view** computed
  from blocks by a deterministic pass. Indexes are stored in the file (tiny,
  and they make the raw JSON self-describing for Tier-1 agents) but formally
  regenerable: `reindex` rebuilds them all, and any mutation that touches
  blocks (e.g. switching `active_ocr_pass`) re-derives. Drift is impossible
  by construction.
- **Labels are separated from addresses.** Index entries carry both the
  address (block id, physical page) and the human/PDF-native label. Pages get
  an extensible label map — `"labels": {"pdf": "xii", "stamp": "fls. 249"}` —
  core fills `pdf` from `/PageLabels`; downstream verticals may write others
  (e-filing stamps). Element labels ("Table 3", parsed from the caption) are
  stored beside block ids so label ↔ address resolves both ways. This is what
  makes citations robust.
- **Index coverage is validated, not assumed.** On save: every block must be
  reachable from the page index and from a section (or the explicit
  `unsectioned` bucket — front matter before the first heading). Orphans are
  a validation error; the file records `index_coverage` so a consumer sees
  "97% sectioned, 3% unsectioned (page-reachable)". No silently unreachable
  content.

The index set (all in `"indexes": {…}`): **pages** (n → block range +
labels), **sections** (flat id map → block ranges; the tree is in
`sections`), **kinds** (tables / figures / footnotes / equations: label,
caption, page, block id), **dests** (named destinations / links → page,
block). Deliberately absent in v0: a full-text inverted index — at
single-document scale a linear scan over a few MB of text is milliseconds;
lexical/semantic indexing across documents belongs to the corpus layer.

## The agent CLI — a document shell over `.dpdf`

The piece that makes `.dpdf` *better* than HTML/Markdown for an agent, not
just equivalent — and it distributes itself: any agent with shell access can
use it (no MCP server, no SDK). Markdown out by default; `--json` for
structure. A thin shim over the same Rust accessors as the Python API (one
core, two faces).

```bash
distillpdf case.pdf -o case.dpdf          # distill once (existing CLI verb)

distillpdf case.dpdf info                 # pages, sections, tables, OCR state, assets
distillpdf case.dpdf toc                  # section tree: ids, titles, page ranges
distillpdf case.dpdf read sec-2-methods   # one section as markdown
distillpdf case.dpdf read --pages 610-612 # page range (physical or label: --pages fls.249)
distillpdf case.dpdf find "phrase" --context 2 --section sec-4 [--regex] [--kind table]
distillpdf case.dpdf tables               # table index → read tbl-03
distillpdf case.dpdf figures              # figure index → image fig_03 -o /tmp/fig.png
distillpdf case.dpdf ocr status           # per-page outcomes; passes per page
distillpdf case.dpdf ocr diff p1 p2 --page 610
```

Agent-ergonomics rules (the same shape as mcp-methods' `read_source`/`grep`
for code, applied to documents):

- **Every listing emits ids that thread into the next call** — `toc` → section
  ids, `tables` → `tbl-NN`, `find` hits → `(block id, section, page, labels)`.
  One call's output is the next call's input; no scraping.
- **Navigation lives in the output, not the agent's memory.** `read` ends with
  breadcrumbs: `prev: sec-1-introduction · next: sec-3-results · parent: —`.
  Moving through the document costs zero context.
- **Bounded output, honest truncation.** `read` defaults to a sane
  `--max-chars`; truncation says
  `showing b0210–b0260 of b0210–b0395; continue: read sec-2 --from b0261` —
  measured and resumable, never a silent cut.
- **`find` is the exact-phrase primitive** (the biggest miss in the legal
  field run): lexical, scoped by section/kind/page, context lines, and a
  coverage line (`scanned 3,048 blocks; 14 pages illegible & unsearchable`)
  so "not found" is a measured statement.

## Query surface (and where it stops)

- **Tier 1 — the JSON itself.** Agent-legible with file tools alone; the
  stored indexes make it navigable without computation.
- **Tier 2 — accessors + CLI:** `distillpdf.load("case.dpdf")` → `Doc`, with
  `.pages` / `.sections` / `.toc()`, `.section(id)`, `.blocks(kind=, section=,
  pages=)` / `.block(id)`, `.tables()` / `.figures()`, `.find("phrase", …)`
  (returns a coverage-honest `FindResult`), `.info()`, `.ocr_passes()` /
  `.ocr_status()`, and the fidelity re-renders `.to_html()` / `.to_markdown()` /
  `.text()`; the CLI above is the same surface over the same internal reader, so
  the two cannot drift. (Per-pass `.diff_ocr(…)` waits on phase-2 OCR-pass
  writing.)
- **Stop there.** Cross-document indexing, embeddings, semantic search,
  coverage accounting across a corpus, annotations — that's the corpus layer
  (kglite-docs), which consumes `.dpdf` as its ingestion contract. distillPDF
  owns *single-document truth, immutable once distilled*; the corpus layer
  owns everything written afterwards. (Equally: "find across 200 documents"
  is the corpus CLI's job, not this one's.)

## Renderers become pure functions of the model

`to_html` / `to_markdown` / `to_pdf` (searchable) / `extract_text` take the
model — fresh from a parse or loaded from disk — which guarantees the
"Markdown is a transform of the same HTML" property by construction, for every
format, and makes render options (image modes, page-vs-section) free to vary
after the fact.

## Phasing (the engine track; fidelity ratchet continues in parallel)

1. **The model** (`L`): stabilize internal element structs → serde →
   container + load path → renderers consume the model. Includes block ids,
   the derived indexes + validation, and the per-page legibility outcome
   (`ocr_ok|ocr_partial|ocr_illegible`, `legible_chars`) — all model fields.
   **1b. The agent CLI** (`M`, immediately after — it's the model's proof of
   use): `info / toc / read / find / tables / figures / ocr status|diff`,
   breadcrumb navigation, honest truncation.
2. **OCR tier completion** (`M`): the BYO/**agent backend** (request/submit —
   the caller's LLM is the engine; prepared, *tiled/right-sized* page images
   out, text/DocTags in, batch rows, per-row failure isolation) writes into
   the model as a pass. Tiling/right-sizing to the VLM input cap (~1.15 MP)
   also fixes silent downscale for the granite backends.
3. **Scanned/e-filing benchmark track** (`M`): real scans (stamps, faded
   pages, Form-XObject-wrapped, baked scanner OCR layers) with detect.rs
   decision accuracy + legibility metrics wired into the regression
   `baseline.json`. Also: decide the page-rasterization question explicitly
   (recommend: document "imaging covers scanned pages; BYO rasterizer for
   vector pages"; an optional pdfium feature can come later if demanded).
4. **Downstream cutover = acceptance test**: kglite-docs ingests `.dpdf` on
   the 1,564-page legal corpus; diff vs its pymupdf4llm path. Only after this
   survives does `schema_version` go to `1` (stability commitment — the schema
   is then a more durable API than any function signature).

## Risks

- **Schema commitment** is the big one — hence `schema_version: 0` until the
  cutover proves the shape.
- **Over-modeling**: capture the analysis you already have; resist speculative
  fields. New needs add fields behind a version bump.
- **Size**: text and passes are cheap; assets are the size story, and the
  asset policy makes that the user's explicit choice.
