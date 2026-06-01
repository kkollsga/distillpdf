# pylopdf — progress log

Goal: see `GOAL.md`. Active loop building toward feature-complete PyMuPDF competitor.

## Done
- **Phase 0** (commit c806443): maturin/PyO3 abi3 wheel on lopdf. `Pdf.open/from_bytes`,
  `page_count`, `extract_text`, `extract_page_text`. Smoke test passes. 6/6 PDFs open.
- **Bench integration**: `bench/bench_pylopdf.py` + added to `score_quality.py`.

## Current quality baseline (word recall vs PyMuPDF, bench corpus)
| PDF | pylopdf | best peer | gap |
|---|---|---|---|
| Cold_Email | **R1.00 S1.00** | pdf-inspector .99 | ahead |
| attention | R0.96 S0.98 | pdf-extract .99 | small |
| fw9_form | R0.99 S1.00 | — | competitive |
| romanian | R0.91 | pdf-inspector 1.0 | **diacritics** |
| unicode_prof (CID) | **R0.00** | pdf_oxide 1.0 | **CID fonts** |
| unicode_showcase (CID) | **R0.00** | pdf_oxide .99 | **CID fonts** |
Speed: 0.004–0.10s — already competitive with pdf-inspector/pdf_oxide.

## Iteration 2 (commit pending)
- Added `src/text.rs`: ToUnicode-CMap content-stream extractor (bfchar/bfrange parser,
  Type0 2-byte + simple 1-byte codes, Tj/TJ/Td handling).
- Wired as **rescue-only hybrid**: lopdf default, ours only when lopdf returns empty →
  **no regression** (Cold_Email R1.00, attention .96, fw9 .99, romanian .91 all held).
- Proven: our extractor *does* decode correct Type0 words (saw "Cold/Email/Hacks" on
  Cold_Email), but pollutes with U+200B + over-splits lines → still worse than lopdf there,
  so kept rescue-only.
- **Still open:** synthetic CID PDFs (unicode_professional_demo, unicode_showcase) → R0.00.
  Our extractor emits newlines but no letters on them. Diagnosis needed next:
  - unicode_professional_demo fonts = Type1 Courier/Times/Helvetica (WinAnsi, NO ToUnicode)
    + Type0 Arial (ToUnicode). Visible text likely in the simple fonts; our latin1 fallback
    should fire but produced nothing → font-name matching or current-font tracking bug.
  - NEXT: dump `get_page_fonts` keys vs `Tf` operands + content ops for that file; verify
    CMap parses; fix simple-font /Encoding (WinAnsi/Standard) decoding, not just latin1.

## Next increments (priority order)
1. **Own text extractor** replacing lopdf `extract_text`: walk content stream ops
   (BT/ET, Tf, Td/TD/Tm, Tj/TJ), map bytes→Unicode via font `/ToUnicode` CMap +
   `/Encoding` (fixes diacritics + CID/Type0 → closes the two R0.00 failures).
   Emit text spans with bbox/font/size (needed for tables + markdown).
2. **Font extraction** (`extract_fonts`): per-page font dicts — name, subtype, embedded
   flag, encoding. (Goal pillar; reuses font-dict parsing from #1.)
3. **Image extraction** (`extract_images`): `lopdf::get_page_images` + filter routing
   (DCTDecode passthrough, Flate→PNG). May need lopdf bump for `get_page_images`.
4. **Table extraction**: multi-strategy on spans (ruling-lines/rects/whitespace).
5. **Markdown layer** + re-bench until recall ≥0.99 incl. diacritics & CID.

## Notes
- lopdf 0.36 lacks `get_page_images` (added later upstream) — bump when doing #3.
- lopdf parsed romanian where oxidize-pdf's own parser failed — backend choice validated.
