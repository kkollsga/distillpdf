# pylopdf

A small, fast, pure-Rust PDF extraction library for Python — a permissively-licensed
PyMuPDF alternative built on [`lopdf`](https://github.com/J-F-Liu/lopdf) and exposed via
PyO3 + maturin. **No system dependencies, MIT/Apache, `pip install`-able.**

## Status: v1

| Pillar | Status |
|---|---|
| Text extraction (reading order, CID/Type0, accurate spacing) | ✅ |
| Image extraction (JPEG passthrough, metadata) | ✅ |
| Font extraction (name/subtype/encoding/embedded/ToUnicode) | ✅ |
| Table extraction (gutter/column clustering) | ✅ |

**Quality** (word recall vs PyMuPDF, see `../bench/`): mean **0.982**, median **0.993**.
CID fonts solved (0.99–1.0). **Speed**: ~1.8× faster than pdf_oxide, 20–700× faster than
pymupdf4llm.

Known limitation (deferred): a few-percent recall gap on glyphs encoded only inside
embedded Type1 font programs (e.g. romanian ț/ș) — needs an embedded font-program decoder
(the "font moat"). Tracked in `../PROGRESS.md`.

## Install / build

```bash
cd pylopdf
maturin develop --release      # build + install into the active env
```

## Usage

```python
import pylopdf

doc = pylopdf.Pdf.open("file.pdf")          # or Pdf.from_bytes(data)
print(doc.page_count())
print(doc.extract_text())                    # whole document, reading order
print(doc.extract_page_text(1))              # 1-indexed page

for img in doc.extract_images():             # [{page,index,width,height,format,data}]
    open(f"p{img['page']}_{img['index']}.{img['format']}", "wb").write(img["data"])

for f in doc.extract_fonts():                # [{page,name,subtype,base_font,encoding,embedded,has_tounicode}]
    print(f["base_font"], f["subtype"], f["embedded"])

for t in doc.extract_tables():               # [{page,n_rows,n_cols,cells:[[str]]}]
    print(t["page"], t["n_rows"], "x", t["n_cols"])
```

## License
MIT OR Apache-2.0.
