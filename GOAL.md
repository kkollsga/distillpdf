# 🎯 Project Goal — `distillpdf` (working name)

> **v1 STATUS (2026-06-02): SHIPPED & accepted.** All 4 pillars (text/images/fonts/tables)
> working; CID fonts solved; mean recall 0.982 / median 0.993 vs PyMuPDF; ~1.8× faster than
> pdf_oxide; small MIT wheel. Per user decision, the residual diacritics gap (embedded
> font-program glyphs like ț/ș) is **accepted for now, deferred** — revisit with an embedded
> Type1/CFF font-program decoder later. See `PROGRESS.md` and `bench/REPORT.md`.


> A feature-complete, pure-Rust **PyMuPDF competitor** for Python, built on **lopdf** as
> the parsing backend, exposed via **PyO3 + maturin**. Clean-room ("greenroom") build that
> uses **pdf-inspector** and **pdf_oxide** as *architectural inspiration*, not copied code.

## North star
Match PyMuPDF on **extraction quality** while being faster, smaller, permissively licensed
(MIT/Apache, no AGPL), and trivially `pip install`-able.

## Scope — must be fully supported
1. **Text extraction** — spans with bbox, font, size, flags, color; reading-order reconstruction.
2. **Table extraction** — multi-strategy detector (ruling-lines, rect/cell-boxes, whitespace
   heuristic, tagged-struct tree). Inspiration: pdf-inspector's 4-strategy `tables/` module.
3. **Image extraction** — via `lopdf::Document::get_page_images()` + filter routing:
   DCTDecode passthrough → JPEG; Flate/LZW → assemble PNG; JPXDecode/CCITT/JBIG2 → codec crates.
4. **Font extraction** — embedded font programs + ToUnicode/CMap + glyph→Unicode mapping
   (Type1/TrueType/CFF/Type0-CID/Type3). **This is the moat** — pdf_oxide's font engine is the bar.
5. **Markdown / RAG output layer** — the `pdf4llm` purpose (chunking, headings, token estimates).

Explicitly **out of v1 scope** (later phases): page rasterization/rendering, PDF generation,
forms/annotations editing, OCR (route to external/vision fallback; flag pages like both peers do).

## Architecture
```
lopdf  ──── parse backend ONLY (xref, objects, Flate/LZW/ASCII85, content-stream tokenizing)
   │        QUARANTINE to a bottom layer so the engine stays swappable (pdf-inspector pattern)
   ▼
own content-stream interpreter  →  fonts/glyph mapping  →  layout/reading-order
   ▼
tables  ·  images  ·  markdown
   ▼
PyO3 + maturin  →  abi3 wheel  →  pip install (zero runtime deps from user's view)
```
- Pure-Rust crates statically linked: `lopdf`, `ttf-parser`/`allsorts` (fonts),
  `image`/`zune-jpeg` (image codecs), `flate2` (miniz, via lopdf).
- Greenroom: study pdf-inspector (MIT) + pdf_oxide (MIT/Apache) for algorithms & layout;
  both permissive so reuse is *legal*, but write fresh for control & to avoid bus-factor lock-in.

## Success criteria (vs the benchmark in `bench/`)
| Dimension | Target |
|---|---|
| Text recall vs PyMuPDF | ≥ 0.99 on real PDFs; **beat pdf_oxide's diacritics gap** (it scored R.81 on Romanian) |
| CID/unicode fonts | match PyMuPDF (≈R 1.0) where oxidize-pdf garbled (R .06–.11) |
| Tables | match/beat pdf-inspector (4 tables on arXiv) and pdf_oxide (fw9=5) |
| Performance | competitive with pdf-inspector & pdf_oxide; ≫ pymupdf4llm (which is 20–700× slower) |
| Footprint | < 10 MB wheel (pdf-inspector 4 MB · pdf_oxide 19 MB · PyMuPDF 79 MB) |
| Robustness | 6/6 on current corpus, then expand corpus |
| License | MIT / Apache-2.0 |

## Benchmark harness (already built, in `bench/`)
Baselines: pymupdf4llm + plain PyMuPDF. Competitors: oxidize-pdf, pdf-extract,
pdf-inspector, pdf_oxide. Scripts: `bench_*.py`, `score_quality.py`, corpus in `bench/corpus/`,
results + `REPORT.md` in `bench/out/`. Add `distillpdf` here once it builds.

## Reference findings (from this repo's investigation)
- MuPDF ≈ 580k LOC C (+ ~1M thirdparty), AGPL → the reason we avoid it.
- pdf_oxide ≈ 386k LOC, own parser (NOT lopdf), real renderer+fonts, the most complete peer.
- pdf-inspector ≈ 63k LOC on lopdf, text/tables/markdown only, cleanly layered, pyo3-ready.
- lopdf gives objects + Flate/LZW/ASCII85 + `get_page_images` (raw bytes, no codec decode).

## Open question
**Name** — `distillpdf` is clear but couples the brand to lopdf (a swappable internal). Consider a
backend-agnostic name before publishing. Tracked, not blocking.
