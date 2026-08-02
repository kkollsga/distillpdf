# Raw extraction

Alongside the HTML/Markdown renderers, a `Document` exposes lower-level methods that
return structured Python data — tables, images, links, fonts, and plain text — straight
from the PDF object model. These are useful when you want the data itself, not a rendered
document.

All of these methods live on the Rust core (`Pdf`) and are reached through the
`Document` returned by `distillpdf.open()`; the `Document` wrapper delegates any method it
does not define itself straight to the core, so `doc.extract_tables()` calls the Rust
implementation directly.

```python
import distillpdf

doc = distillpdf.open("paper.pdf")
print(doc.page_count())
tables = doc.extract_tables()
```

Unlike the renderers, none of these methods write files or take a `path` — they return
Python objects (lists of dicts, or strings) every time. They also do not require
`run_processing()` or OCR; they read the source PDF directly, so a scanned page with no
text layer yields nothing from `extract_text()` and no entries from the structured
methods.

## page_count

```python
doc.page_count() -> int
```

The number of pages in the document.

## extract_text

```python
doc.extract_text() -> str
```

Plain text from every page, concatenated in page order with a newline after each page.

The extractor is hybrid: a position- and width-aware content-stream reader is primary
(it handles CID fonts, diacritics, accurate word boundaries, and reading order), and
lopdf's own per-page extractor is used as a fallback only when the primary recovers
little or nothing on a page, so simple-encoded PDFs never regress.

To pull text from one page instead of the whole document, use `extract_page_text`:

```python
doc.extract_page_text(page) -> str
```

`page` is **1-indexed**. Passing a page number that does not exist raises `ValueError`
(`"no page N"`). The same hybrid logic applies per page.

!!! note
    `extract_text` reads the PDF's existing text layer. For scanned PDFs with no text
    layer, run OCR first and render through the document model — see
    [OCR for scanned PDFs](ocr.md).

## extract_tables

```python
doc.extract_tables() -> list[dict]
```

Detects tables across all pages from text positions (there are no ruling lines to rely
on in most PDFs). Each detected table is one dict:

| Key      | Type              | Meaning                                              |
| -------- | ----------------- | ---------------------------------------------------- |
| `page`   | `int`             | 1-indexed page the table was found on                |
| `n_rows` | `int`             | number of rows in the cell grid                      |
| `n_cols` | `int`             | number of columns (length of the first row)          |
| `cells`  | `list[list[str]]` | the grid, row-major; each cell is a string           |

```python
for t in doc.extract_tables():
    print(f"page {t['page']}: {t['n_rows']}x{t['n_cols']}")
    for row in t["cells"]:
        print(row)
```

The detector works on runs of consecutive rows that share aligned columns, splits
two-column page layouts down their centre gutter so adjacent prose columns are not merged
into a phantom wide table, and applies admission tests that reject prose, display
equations, symbolic matrices, and commutative diagrams that merely happen to have aligned
tokens.

!!! note "Header structure"
    Internally the detector also recovers grouped / multi-level headers, mapping a header
    cell that spans several data columns to a colspan (this is what feeds the
    `<th colspan=...>` cells in the rendered HTML). That colspan information is **not**
    exposed by `extract_tables()` — the returned `cells` grid is a flat row-major grid of
    strings. For multi-level headers as rendered markup, use
    [`to_html()`](rendering.md) or the [.dpdf document model](dpdf.md) instead.

## extract_images

```python
doc.extract_images() -> list[dict]
```

Every image a page draws, including the image bytes. Each image is one dict:

| Key                  | Type          | Meaning                                                             |
| -------------------- | ------------- | ------------------------------------------------------------------- |
| `page`               | `int`         | 1-indexed page                                                      |
| `index`              | `int`         | image index within that page, in page order                        |
| `width`              | `int`         | pixel width                                                         |
| `height`             | `int`         | pixel height                                                        |
| `color_space`        | `str \| None` | colour-space family: `"DeviceRGB"`, `"DeviceGray"`, `"DeviceCMYK"`, `"ICCBased"`, `"Indexed"`, `"Separation"`, … |
| `bits_per_component` | `int \| None` | 1, 2, 4, 8 or 16                                                    |
| `format`             | `str`         | one of `"png"`, `"jpeg"`, `"jpx"`, `"ccitt"`, `"jbig2"`, `"raw"`    |
| `data`               | `bytes`       | the image bytes                                                     |

```python
for im in doc.extract_images():
    print(im["page"], im["index"], im["width"], "x", im["height"], im["format"])
    with open(f"img_{im['page']}_{im['index']}.{im['format']}", "wb") as f:
        f.write(im["data"])
```

For `"png"`, `"jpeg"` and `"jpx"` the bytes are a complete image file you can open
directly (PIL, a browser, `file`). PDFs store most non-photographic rasters as plain
compressed *samples* with no container; those are assembled into a real PNG and reported
as `"png"`. Photographic images stay in their original codec, unwrapped from any
Flate/LZW/ASCII85 layer the PDF added on top.

`color_space` is resolved through indirect references, through an ICC profile's component
count, and through a name defined in the page's `/ColorSpace` resources — so it tells you
what the samples mean rather than being blank.

!!! warning
    `"raw"`, `"ccitt"` and `"jbig2"` are **not** ready-to-open files. `"ccitt"` and
    `"jbig2"` are the codec bitstreams; `"raw"` is a sample block in a colour space we
    will not reduce for you (`Lab`, `Separation`, `DeviceN`) or a stream that is
    truncated. Those rows still carry `width`, `height`, `color_space` and
    `bits_per_component`, which is everything you need to assemble them yourself. If you
    want render-ready images without any of this, use `to_html(image_mode="external")` or
    `to_markdown()`, which extract figures into an `img/` folder — see
    [Rendering HTML & Markdown](rendering.md).

## extract_links

```python
doc.extract_links() -> list[dict]
```

Link annotations across all pages — external URI links, internal jumps, and links into
another document. Each link is one dict:

| Key           | Type           | Meaning                                                        |
| ------------- | -------------- | -------------------------------------------------------------- |
| `page`        | `int`          | 1-indexed page the clickable rectangle sits on                 |
| `rect`        | `list[float]`  | clickable rectangle `[x0, y0, x1, y1]` in PDF user space (y up) |
| `kind`        | `str`          | `"uri"` external, `"internal"` in-document, `"remote"` another file |
| `uri`         | `str` or `None`| the target URL for `"uri"` links, else `None`                  |
| `dest_page`   | `int` or `None`| 1-indexed destination page for internal links, when resolvable |
| `dest_name`   | `str` or `None`| the raw named destination (e.g. `"cite.devlin2018"`, `"section.3.1"`) when present |
| `remote_file` | `str` or `None`| the target document of a `"remote"` (`/GoToR` or `/Launch`) link |

```python
for lk in doc.extract_links():
    if lk["kind"] == "uri":
        print(lk["page"], "->", lk["uri"])
    elif lk["kind"] == "remote":
        print(lk["page"], "->", lk["remote_file"], lk["dest_name"])
    else:
        print(lk["page"], "-> page", lk["dest_page"], lk["dest_name"])
```

`kind` is `"uri"` exactly when `uri` is not `None`. For internal links the destination
is resolved to a page number where possible; a named destination keeps its name in
`dest_name` even after the page is resolved, which is useful as an anchor id. For
`"remote"` links the destination lives in `remote_file`, so `dest_page` is always `None`
and `dest_name` — when present — names a destination *inside that other file*.

## extract_fonts

```python
doc.extract_fonts() -> list[dict]
```

Per-page font information — one dict per font referenced on each page (a font used on
several pages appears once per page). Each dict:

| Key             | Type   | Meaning                                                            |
| --------------- | ------ | ------------------------------------------------------------------ |
| `page`          | `int`  | 1-indexed page                                                     |
| `name`          | `str`  | the resource name the page refers to the font by (e.g. `"F1"`)     |
| `subtype`       | `str`  | font subtype (e.g. `"Type1"`, `"TrueType"`, `"Type0"`); `""` if absent |
| `base_font`     | `str`  | the `/BaseFont` PostScript name; `""` if absent                    |
| `encoding`      | `str`  | the named encoding, or `"custom"` when the encoding is not a plain name |
| `embedded`      | `bool` | whether the font program is embedded (`FontFile`/`FontFile2`/`FontFile3`, including via a Type0 descendant) |
| `has_tounicode` | `bool` | whether the font carries a `/ToUnicode` CMap                       |

```python
for f in doc.extract_fonts():
    flags = []
    if f["embedded"]:
        flags.append("embedded")
    if f["has_tounicode"]:
        flags.append("tounicode")
    print(f["page"], f["name"], f["base_font"], f["subtype"], " ".join(flags))
```

`has_tounicode` and `embedded` are the two fields that matter most for text-extraction
quality: a font with no `ToUnicode` map and a custom encoding is the usual cause of
garbled extracted text.

## See also

- [Rendering HTML & Markdown](rendering.md) — the higher-level renderers, including
  render-ready figure extraction.
- [The .dpdf document model](dpdf.md) — distill once into a queryable model with
  sections, blocks, and tables as structured markup.
- [Python API reference](../reference/python.md) — the full method surface.
- The source: [github.com/kkollsga/distillpdf](https://github.com/kkollsga/distillpdf).
