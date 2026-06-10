# Searching documents

Once a PDF is distilled to a [`.dpdf`](dpdf.md), you search it without re-parsing
the source. There are two independent search paths over the same document: lexical
`find` (exact substring or regex, with honest coverage accounting) and semantic
`search` (cosine similarity over BAAI/bge-m3 chunk vectors). Both run against the
loaded document model and both emit ids that thread straight into a `read`.

```python
import distillpdf

doc = distillpdf.load("case.dpdf")
```

## Lexical search: `Doc.find`

`Doc.find` is the exact-phrase primitive. It is case-insensitive substring matching
by default, or a regex when `regex=True`. The match runs over each block's
*searchable text* — its prose, plus a table's flattened cell contents and a
figure/table caption — so a `find` reaches table data and captions, not only body
paragraphs.

```python
result = doc.find("indemnification")
for hit in result.hits:
    print(hit.block_id, hit.page, hit.block.get("section"))
```

Signature:

```python
doc.find(
    query,
    *,
    regex=False,          # treat query as a regex (re.IGNORECASE)
    section=None,         # scope to a section id (includes its subtree)
    kind=None,            # scope to a block kind: "para", "table", "figure", …
    pages=None,           # scope to a list of physical page numbers
    limit=None,           # truncate the returned hits list (coverage counts stay full)
)
```

`section`, `kind`, and `pages` narrow the search scope. `section` takes a section id
(see [`Doc.toc`](dpdf.md)) and includes that section's whole subtree. `pages` is a
list of physical page numbers — to turn a spec like `"3-7"` or a page label into
physical numbers first, use `doc.resolve_pages("3-7")`.

```python
# Regex, scoped to one section, capped at 5 hits.
result = doc.find(r"\$\d[\d,]*", regex=True, section="sec-methods", limit=5)
```

An invalid regex raises `DpdfError`; an unknown `section` id raises `DpdfError`
carrying the closest section ids it knows about.

### The `FindResult` — coverage is never silent

`find` returns a `FindResult`, not a bare list. The contract is that "not found" is
always a *measured* statement: the result pairs the matches with how much was
searched, so an empty `hits` list is never an ambiguous silent miss.

| Attribute | Meaning |
| --- | --- |
| `hits` | list of `Hit` objects (see below), in document order |
| `searched_blocks` | number of blocks the query ran over (after scoping) |
| `searched_pages` | number of distinct pages those blocks spanned |
| `no_text_pages` | sorted page numbers that were in scope but carried no searchable text |

`no_text_pages` is the honesty signal: a page that is in scope but has no text layer
(for example a scanned image page that has not been OCR'd) shows up here rather than
just contributing zero hits. If you searched a document and got nothing, a non-empty
`no_text_pages` tells you the gap is missing text, not an absent phrase — the fix is
[OCR](ocr.md), not a different query.

Each `Hit` carries:

| Attribute | Meaning |
| --- | --- |
| `block_id` | the id of the matched block (threads into `doc.block(...)` / `read`) |
| `block` | the full block dict (`id`, `kind`, `text`, `page`, `section`, …) |
| `page` | the physical page the block is on (or `None`) |
| `start` / `end` | the matched character span within the block's searchable text |
| `text` | the searchable text the match was found in |

```python
result = doc.find("force majeure")
if not result.hits:
    print(f"no match across {result.searched_blocks} blocks / "
          f"{result.searched_pages} pages")
    if result.no_text_pages:
        print(f"pages with no text layer: {result.no_text_pages}")
else:
    for h in result.hits:
        print(f"{h.block_id}  p{h.page}  …{h.text[h.start:h.end]}…")
```

`limit` truncates only the returned `hits` list; the coverage counts
(`searched_blocks`, `searched_pages`, `no_text_pages`) always reflect the full
search. Pass `limit=None` (the default) for every hit.

## Semantic search

Semantic search matches on meaning rather than exact characters. It is a two-step
flow: embed the document once (`Doc.embed`), then query it as often as you like
(`Doc.search`). Embeddings are stored inside the `.dpdf` itself, so a document only
needs embedding once.

!!! note "Embeddings need an optional runtime"
    `embed` and `search` require the BAAI/bge-m3 model on ONNX Runtime, which is an
    optional dependency. The exact install line is printed when it is missing — see
    [Dependencies](#dependencies) below.

### Chunks: derived, never duplicated

Embeddings are computed over *chunks*, not raw blocks. A chunk is a contiguous run of
blocks **within a single section**, accumulated toward a target of roughly 400 tokens
(measured with a cheap chars/4 proxy, not the real tokenizer). A section boundary
always starts a new chunk, and a single block larger than the target becomes its own
chunk — a block is never split. The intent is that a hit lands on a coherent passage,
a paragraph or two of context, rather than a bare sentence.

Chunks are **derived from blocks, not stored as text.** A chunk record holds only
addresses and spans — `id`, `block_ids`, `section`, `page_start`, `page_end` — and its
text is recomposed from the referenced blocks at embed and search time. Because the
derivation is deterministic (same blocks produce the same chunks), a stored chunk set
can be diffed against a fresh derivation to detect drift, which is the staleness signal
described below.

### `Doc.embed`

```python
info = doc.embed("e1")   # space_id defaults to "e1"
# {'space': 'e1', 'model': 'BAAI/bge-m3', 'chunks': 142,
#  'dimension': 1024, 'backend': 'vendored', 'dropped_stale': []}
```

`embed` derives the chunks, embeds each chunk's text with BAAI/bge-m3, writes the
1024-dimension vectors into the `.dpdf` as an embedding space, and **saves the file.**
Signature:

```python
doc.embed(
    space_id="e1",       # the embedding space id to write
    *,
    cache_dir=None,      # where the bge-m3 weights are cached
    batch_size=32,       # chunks per embedding batch
    progress=True,       # print per-batch progress to stderr when it is a tty
)
```

Behaviour worth knowing:

- **It writes to disk.** The document must have been loaded from a path
  (`distillpdf.load("case.dpdf")`); a `Doc` built in memory has nowhere to write and
  raises `DpdfError`.
- **Atomic.** Vectors are computed in full before anything is written, so a mid-batch
  failure leaves the file untouched — you get a complete space or no space, never a
  half-record.
- **Re-embedding `space_id` overwrites it.** Embedding the same id again replaces the
  prior space.
- **Stale spaces are dropped.** Because `embed` re-derives chunks, any *other* space
  whose stored `chunk_ids` no longer match the current chunking is dropped, and the
  dropped ids are listed in the return value's `dropped_stale` (and printed to stderr).

Vectors are L2-normalized before they are stored (so cosine similarity is a plain dot
product), and the space metadata records `normalized: true`. The on-disk layout of the
space — the metadata record and the raw f32 vector member — is described in the
[.dpdf format reference](../reference/dpdf-format.md).

### `Doc.search`

```python
hits = doc.search("who is liable for delivery delays?", k=5)
for h in hits.hits:
    print(h["chunk_id"], h["score"], h["section"], h["block_ids"])
```

`search` embeds the query with the same model, ranks the document's chunks by cosine
similarity, and returns the top `k` as a `SearchResult`. Signature:

```python
doc.search(
    query,
    *,
    k=8,            # how many chunks to return
    space=None,     # embedding space id; default is the first space
)
```

If the document has no embedding space, `search` raises `DpdfError` pointing you at
`embed` (and, if the runtime is also missing, appending the install help). An unknown
`space` id raises `DpdfError` listing the available ids.

The `SearchResult` carries:

| Attribute | Meaning |
| --- | --- |
| `query` | the query string |
| `hits` | list of hit dicts (see below), highest score first |
| `space` | the embedding space id that was searched |
| `model` | the model the space was embedded with (`BAAI/bge-m3`) |
| `searched` | how many chunks were ranked |
| `stale` | `True` when the chunking has drifted from the current blocks |

Each hit is a dict:

| Key | Meaning |
| --- | --- |
| `chunk_id` | the chunk id |
| `score` | cosine similarity, rounded to 6 places |
| `section` | the chunk's section id |
| `page_start` / `page_end` | the chunk's page span |
| `page_label_start` / `page_label_end` | the `/PageLabels` labels for those pages, when present |
| `block_ids` | the ids of the blocks the chunk covers |
| `snippet` | the chunk's recomposed text |

The `block_ids` (and `section`) thread back into block reading: feed them to
`doc.block(bid)`, `doc.block_markdown(bid)`, or `doc.section(sid)` to pull the exact
source text behind a hit. See [The .dpdf document model](dpdf.md) for those readers.

### Staleness on re-distill

A `.dpdf` is a snapshot. If you re-distill the source PDF (for example after running
OCR), its blocks — and therefore its derived chunks — can change, while an embedding
space written against the old blocks stays put. `Doc.search` detects this: `stale` is
`True` when a fresh chunk derivation no longer matches the chunk ids the space recorded.
The scores then reflect an out-of-date chunking; refresh with `doc.embed(...)`.

`Doc.info()` surfaces the same state without running a query. Its `embeddings` key
reports each space (`id`, `model`, `dimension`, chunk count) and a top-level `stale`
flag:

```python
doc.info()["embeddings"]
# {'spaces': [{'id': 'e1', 'model': 'BAAI/bge-m3', 'dimension': 1024, 'chunks': 142}],
#  'chunks': 142, 'stale': False}
```

### Dependencies

The embedding runtime is optional and imported lazily, so `distillpdf` is always
importable. The moment you `embed` or `search` without it installed, you get an
`EmbedDependencyError` (a subclass of `ImportError`) carrying the exact install line
rather than an opaque `ModuleNotFoundError`. The line it prints:

```bash
pip install onnxruntime tokenizers huggingface_hub
pip install numpy        # optional but recommended (faster cosine)
```

The bge-m3 weights (~2.3 GB) download from HuggingFace on first use and cache under
`~/.cache/fastembed`. Override the cache with the `FASTEMBED_CACHE_PATH` environment
variable, or point `HF_HOME` (or `cache_dir=`) at an existing copy to run offline.

!!! note "Shared vectors with kglite"
    The vectors are byte-identical to what [kglite](https://pypi.org/project/kglite/)'s
    bge-m3 stack produces. If `kglite` is importable, `distillpdf` uses its embedder
    directly; otherwise it uses a vendored twin that produces identical vectors. The
    `backend` field in `embed`'s return value (`"kglite"` or `"vendored"`) tells you
    which ran.

## Command line equivalents

Every method here has a CLI verb that operates on the same model:

```bash
distillpdf case.dpdf find "force majeure" --section sec-terms
distillpdf case.dpdf embed
distillpdf case.dpdf search "who is liable for delivery delays?" --k 5
```

`find --semantic` is an alias for `search`. The verbs print a human form by default
and structured JSON with `--json`. See [Command line](cli.md) for the full flag set.

## See also

- [The .dpdf document model](dpdf.md) — loading a document, sections, blocks, and the readers that hit ids thread into
- [OCR for scanned PDFs](ocr.md) — when `no_text_pages` points at a missing text layer
- [Python API reference](../reference/python.md) — full `Doc`, `FindResult`, and `SearchResult` surfaces
- [.dpdf format reference](../reference/dpdf-format.md) — how chunks and embedding spaces are stored
