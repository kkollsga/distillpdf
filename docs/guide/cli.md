# Command line

The `distillpdf` command has two faces. Point it at a **source PDF** and it converts —
HTML, Markdown, plain text, a searchable PDF, or a durable `.dpdf` model. Point it at a
**`.dpdf` model** followed by a verb and it becomes an agent shell over that document:
`info`, `toc`, `read`, `find`, `search`, `embed`, `tables`, `figures`, `ocr-status`.

The two faces share one entry point. `python -m distillpdf …` is equivalent to the
`distillpdf …` console script in every example below.

```console
$ distillpdf paper.pdf -o paper.html      # convert (PDF input)
$ distillpdf case.dpdf toc                 # query (.dpdf input + verb)
$ python -m distillpdf paper.pdf -o paper.html   # same as the first line
```

Routing is by the first argument: a `.dpdf` first positional **followed by a known verb**
goes to the document shell. Anything else (including a `.dpdf` typo with no verb) falls
through to the converter, which reports a normal per-file error.

## Converting a PDF

```console
$ distillpdf paper.pdf                  # HTML to stdout (images inline)
$ distillpdf paper.pdf -o paper.html    # HTML to a file
$ distillpdf paper.pdf -o paper.md      # Markdown (inferred from the .md extension)
$ distillpdf paper.pdf --markdown       # Markdown to stdout
$ distillpdf *.pdf -o out/              # batch: out/<name>.html per input
```

### Output format and where it goes

The format is resolved in this order: `--text` wins, then `--markdown`, then an output
path ending in `.md` / `.markdown` infers Markdown, otherwise HTML.

The destination follows `-o` / `--output`:

- **No `-o`, single input** — streams to stdout.
- **No `-o`, multiple inputs** — writes `<stem><ext>` beside each source PDF.
- **`-o <file>`, single input** — writes exactly that file.
- **`-o <dir>/` or an existing directory, or multiple inputs** — treats `-o` as a
  directory and writes `<stem><ext>` into it (creating it if needed).

When writing a file, distillpdf prints `distillpdf: wrote <path>` to stderr. Per-input
errors (malformed PDF, unreadable path) are reported to stderr and the command continues
with the next input, exiting non-zero if any input failed.

### All converter options

| Option | Effect |
| --- | --- |
| `pdf` (positional, one or more) | Input PDF file(s). Globs are expanded by your shell. |
| `-o`, `--output` | Output file (single input) or directory (batch). Default: stdout for one PDF, `<name>.<ext>` beside each for many. |
| `--markdown`, `--md` | Emit Markdown (also inferred from a `.md` output path). |
| `--text` | Emit plain text instead of HTML. |
| `--toc` | Print the table of contents (indented text) instead of the document. |
| `--section NAME` | Emit only the named section. Errors if no section matches. |
| `--mode {section,page}` | Structure: section-first (default) or page-first. |
| `--image-mode {embed,external,drop}` | Figures: inline `data:` URIs, an external `img/` folder, or placeholder text. Default: `embed` for HTML, `external` for Markdown. |
| `--no-toc` | Omit the auto table-of-contents nav from the output. |
| `--ocr` | OCR scanned / image-only pages first (see [OCR](#ocr-for-scanned-pdfs) below). |
| `--ocr-engine ENGINE` | OCR engine or tier. Default: `fast`. |
| `--remove-raster` | With `--ocr` and a PDF output: reflow to clean text + cropped figures and drop the page raster. |
| `--list-ocr-engines` | List available OCR engines and exit. |
| `--version` | Print `distillpdf <version>` and exit. |

!!! note "`--image-mode` defaults differ by format"
    Leave `--image-mode` unset and each format picks its own default: HTML embeds images
    inline, Markdown writes an external `img/` folder. Pass the flag only to override.

A few worked examples:

```console
$ distillpdf paper.pdf --mode page                       # page-first HTML
$ distillpdf paper.pdf -o p.html --image-mode external   # lean HTML + an img/ folder
$ distillpdf paper.pdf --image-mode drop                 # replace images with placeholders
$ distillpdf paper.pdf --no-toc                          # omit the table-of-contents nav
$ distillpdf paper.pdf --text                            # plain text
$ distillpdf paper.pdf --toc                             # print the TOC
$ distillpdf paper.pdf --section abstract                # just one section
```

See [Rendering HTML & Markdown](rendering.md) for what each mode and image mode produces,
and [Raw extraction](extraction.md) for the text-only path.

## Distilling to a .dpdf

A `.dpdf` **output** path distills the durable document model instead of converting. The
asset profile is fixed to the `.dpdf` default (`figures`); `--image-mode` does not apply
here.

```console
$ distillpdf case.pdf -o case.dpdf      # distill one PDF
$ distillpdf *.pdf -o models/           # batch: models/<stem>.dpdf per input
```

For a single input the `.dpdf` path is the output file; for a batch, `-o` names a
directory and each input writes `<stem>.dpdf` into it. See
[The .dpdf document model](dpdf.md) for what a model contains and
[the .dpdf format reference](../reference/dpdf-format.md) for the on-disk layout.

!!! warning "Experimental format"
    The `.dpdf` schema is `schema_version` 0 and not yet stabilized across releases.
    Treat distilled models as a working cache you can regenerate from the source PDF, not
    a long-term archive format.

## OCR for scanned PDFs

`--ocr` OCRs the scanned / image-only pages of each input before producing output. The
output format follows the same `-o` rules, but the default is a **searchable PDF** rather
than HTML:

- `-o` ends in `.pdf` → a searchable PDF.
- `-o` ends in `.md` / `.markdown`, or `--markdown` is given → OCR'd Markdown.
- `-o` ends in `.html` / `.htm` → OCR'd HTML.
- otherwise → a searchable PDF written as `<stem>.searchable.pdf` beside the source
  (`.searchable.html` / `.searchable.md` for those formats).

```console
$ distillpdf scan.pdf --ocr                       # → scan.searchable.pdf (fast tier, bundled)
$ distillpdf scan.pdf --ocr --remove-raster       # reflow to clean text + figures, smaller file
$ distillpdf scan.pdf --ocr -o out.html           # OCR'd HTML (use a .md path for Markdown)
$ distillpdf scan.pdf --ocr --ocr-engine accurate # granite-docling (needs a runtime you install)
$ distillpdf --list-ocr-engines                   # show engines: name, tier, bundled, offline
```

Before running, distillpdf counts the scanned pages. If none are detected it prints
`no scanned/image-only pages detected; nothing to OCR` and moves on. Otherwise it shows a
progress bar and a line like
`OCR'ing 12 scanned page(s) [tesseract] -> scan.searchable.pdf`.

`--ocr-engine` takes a **tier** (`fast` = the bundled Tesseract build, no extra or
download; `accurate` = granite-docling, which needs a heavier model runtime you install
yourself — there is no single `[ocr]` extra) or a specific backend name. `--list-ocr-engines` prints each engine's name, tier, and whether it is
bundled, offline, and currently available:

```console
$ distillpdf --list-ocr-engines
name                   tier      bundled  offline  avail  description
tesseract              fast      True     True     True   …
granite-docling        accurate  False    False    False  …
```

`--remove-raster` only applies to `--ocr` with a PDF output: instead of keeping the
original scan with a hidden text layer, it reflows pages to clean text plus cropped
figures and drops the page raster, producing a much smaller file.

For installing the `accurate` tier and choosing a runtime, see the dedicated
[OCR for scanned PDFs](ocr.md) guide and the [OCR runtime setup](../ocr-setup.md) page.

## Querying a .dpdf (the agent shell)

When the first argument is a `.dpdf` followed by a verb, distillpdf becomes a document
shell. The shell is built for agents: every listing emits **ids that thread into the next
call**, `read` carries navigation breadcrumbs and bounds its output with resumable
truncation, and `find` reports a coverage line so "not found" is a measured statement.

`--json` is available on **every** verb and mirrors the human output for structured
callers.

```console
$ distillpdf case.dpdf info                   # pages, sections, tables, OCR state, assets
$ distillpdf case.dpdf toc                     # section tree: ids → read targets
$ distillpdf case.dpdf read sec-methods        # one section as markdown + breadcrumbs
$ distillpdf case.dpdf find "fls. 249" --pages xii-xv   # scoped lexical search
$ distillpdf case.dpdf embed                   # build the semantic index (BAAI/bge-m3)
$ distillpdf case.dpdf search "limits on liability"     # semantic search; ids thread into read
```

If the file is not a readable `.dpdf`, the shell says what it looks like and reminds you
how to make one: `make one with distillpdf <file.pdf> -o <file>.dpdf`.

### info

`distillpdf case.dpdf info` prints a roll-up: schema version and the distillpdf release
that wrote it, the source file's sha256 prefix and generation time, counts of pages /
sections / blocks / tables / figures / footnotes, sectioned coverage, OCR state, the asset
profile, and the embedding spaces (or `none (run embed to enable semantic search)`).

```console
$ distillpdf case.dpdf info
case.pdf  (schema v0, distillpdf 0.0.33)
  source sha256: a1b2c3d4…   generated 2026-06-05T…
  pages: 318   sections: 24   blocks: 1840
  tables: 6   figures: 11   footnotes: 92
  sectioned coverage: 96%   (12 unsectioned)
  OCR: none (born-digital text layer)
  embeddings: none (run `embed` to enable semantic search)
```

### toc

The section tree: each row is `<id>  <title>  (page span)`, indented by level. The id is a
`read` target.

```console
$ distillpdf case.dpdf toc
sec-intro       Introduction        (p1-3)
  sec-methods   Methods             (p4-9)
sec-results     Results             (p10-15)
```

### tables / figures

Kind indexes. Each row is `<id>  <label>  p<page>  — <caption>`, with a page-label suffix
(e.g. `p13 (xiii)`) when the page carries a `/PageLabels` label. The id threads into
`read` and `find --kind`.

```console
$ distillpdf case.dpdf tables
b0421  Table 1  p13  — Summary of measured parameters
$ distillpdf case.dpdf figures --json
{ "figures": [ … ] }
```

### read

`read` renders a target as Markdown. The target is a **section id**, a **block id**, or a
`--pages` range — exactly one of them.

```console
$ distillpdf case.dpdf read sec-methods            # a section
$ distillpdf case.dpdf read b0421                  # a single block
$ distillpdf case.dpdf read --pages xii-xv         # a page range (labels or physical)
```

| Option | Effect |
| --- | --- |
| `target` (positional, optional) | A section id or a block id. |
| `--pages N-M` | Read a page range instead. Physical numbers or page labels on either side (e.g. `xii-xv`). |
| `--max-chars N` | Bound the output (default 6000); truncation is resumable. |
| `--from BLOCK-ID` | Resume reading at this block id. |
| `--json` | Structured output. |

Output is bounded to `--max-chars` (default 6000), always showing at least one block so a
single oversized block still makes progress. When it stops short, `read` prints an honest,
resumable truncation line naming exactly where it stopped and how to continue:

```console
$ distillpdf case.dpdf read sec-methods
## Methods
…
showing blocks b0410–b0431 of b0410–b0498; continue: read sec-methods --from b0432
prev: sec-intro · next: sec-results · parent: —
```

The last line is the navigation breadcrumb (`prev · next · parent`); a missing neighbour
shows as an em dash. Reading by `--pages` has no breadcrumb. A `--pages` token is tried
whole first, so a label that itself contains `-` (e.g. `A-1`) resolves as a single page
before being read as a range. An unresolvable page or unknown id is an error, never a
silent empty result; an unknown id suggests the closest section ids.

### find

Lexical search over block text — the exact-phrase primitive. Case-insensitive substring by
default, or a regex with `--regex`.

```console
$ distillpdf case.dpdf find "limitation of liability"
$ distillpdf case.dpdf find "fls\.\s*\d+" --regex
$ distillpdf case.dpdf find "indemnity" --section sec-methods --context 2
```

| Option | Effect |
| --- | --- |
| `query` (positional) | Substring (default) or regex (with `--regex`). |
| `--regex` | Treat the query as a regex (case-insensitive). |
| `--section ID` | Scope to a section id. |
| `--kind KIND` | Scope to a block kind (`table`, `figure`, …). |
| `--pages N-M` | Scope to a page range (physical or labels). |
| `--context N` | Also list N neighbouring block ids per hit. |
| `--limit N` | Max matches to print (default 20); never silently truncated. |
| `--semantic` | Run semantic search instead — an alias for `search` (see below). |
| `--space ID` | With `--semantic`, the embedding space id (default: the first). |
| `--json` | Structured output. |

Each hit is `<block-id>  [<section>]  p<page>  <snippet>`, the matched span wrapped in
`«…»`. The run always ends with a **coverage line** — how many blocks across how many
pages were searched, and which pages have no text — so a zero-match result is a measured
statement, not a shrug:

```console
$ distillpdf case.dpdf find "fls. 249" --pages xii-xv
b0007  [sec-front]  p13 (xiii)  …per «fls. 249» of the…
searched 41 blocks across 4 pages
```

When there are more matches than `--limit`, `find` says how many it held back and how to
see them (`narrow with --section/--pages or raise --limit`) rather than cutting silently.

`find --semantic` is a convenience alias that routes straight to `search` with the same
query (the lexical scope flags do not apply to a vector query).

### embed

`embed` derives chunks from the blocks, embeds them with `BAAI/bge-m3`, and writes the
vectors back into the `.dpdf` as an embedding space (saving the file). This needs the
optional ONNX runtime; a missing dependency prints the exact install line and exits
non-zero — never a silent no-op.

```console
$ distillpdf case.dpdf embed
distillpdf: embedded 412 chunks into space 'e1' (BAAI/bge-m3, dim 1024, backend …)
```

| Option | Effect |
| --- | --- |
| `--space ID` | Embedding space id to write (default: `e1`). |
| `--cache-dir DIR` | Where the bge-m3 weights are cached (default: `~/.cache/fastembed` or `FASTEMBED_CACHE_PATH`; `HF_HOME` is also honoured). |
| `--batch-size N` | Chunks per embedding batch (default 32). |
| `--json` | Structured output. |

### search

Semantic search over the chunk embeddings (run `embed` first). Ranks chunks by cosine
similarity to the query and prints the top-k, whose block ids thread into `read`.

```console
$ distillpdf case.dpdf search "limits on liability"
semantic search over 412 chunks (model BAAI/bge-m3, space e1)
c0033  score=0.7421  [sec-terms]  p22-23
    …the supplier's aggregate liability under this agreement shall not exceed…
    blocks: b0512 b0513
```

| Option | Effect |
| --- | --- |
| `query` (positional) | Natural-language query. |
| `--k N` | How many chunks to return (default 8). |
| `--space ID` | Embedding space id (default: the first). |
| `--json` | Structured output. |

A header line states how many chunks (and which model / space) were searched. If the space
is stale — the document's blocks changed since embedding — it prints a `WARNING` and
suggests refreshing with `embed`. A missing space or missing runtime is an actionable
error, not a crash.

See [Searching documents](search.md) for how lexical and semantic search compare and when
to reach for each.

### ocr-status

Per-page OCR decisions and per-pass outcomes recorded in the model. A born-digital
document prints `no OCR passes recorded`.

```console
$ distillpdf case.dpdf ocr-status
per-page decisions:
  p4: ocr  active: pass-1
per-pass outcomes:
  pass   engine                    page outcome        legible conf
  pass-1 tesseract                    4 ok                 1832 0.91
```

## Notes

- `--json` on any shell verb produces the structured form of the same data — use it from
  scripts and agents instead of parsing the human layout.
- Both faces share the `distillpdf` / `python -m distillpdf` entry point; see the
  [Python API](../reference/python.md) for the library calls these commands wrap, and the
  [design notes](../datamodel-design.md) for why the agent shell is shaped the way it is.
- Source: [github.com/kkollsga/distillpdf](https://github.com/kkollsga/distillpdf).
