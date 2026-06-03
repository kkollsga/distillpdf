# distillpdf — progress log

Goal: see `GOAL.md`. Active loop building toward feature-complete PyMuPDF competitor.
Dev loop & gates: `bash bench/gates.sh` (regression gates must stay green; goal
gates turn green as phases land). Goal/gate definitions: `bench/GOALS.md`.

## Layout-analysis roadmap — DONE (2026-06-02), all gates green
Phase 1 ✅ region typing + opportunistic `<figure>`/`<figcaption>` linking (figure
coverage 1.00, caption attach 0.93). Phase 3 ✅ word-level span extraction
(`decode_words` splits Tj/TJ at spaces+kern gaps; empty-but-advancing glyph = a
space; join threshold dropped to 0.2em for word spans). Phase 2 ✅ borderless
tables now detected from word alignment (bert Table 1 / GLUE results parsed;
presence recall 0.93) — `detect_tables_pos` grid keeps majority columns but MERGES
stray cells into the nearest kept column (dense grid, no content loss, no markup
bloat; synthetic detection 23/23, cell recall 0.94). Run `bash bench/gates.sh`.

## Layout-analysis roadmap (region-first, ABBYY-style) — tasks #1–#3
Region-first DLA: localize regions (figure/table/header-footer/text), then split
cells only inside table regions → no artifacts in prose. Captions/IDs are an
opportunistic overlay (a caption-less figure is still a figure), not a gate.
Prior art (open-source MCP): pdfplumber text strategy — columns = word x-clusters
with ≥3 aligned words (the alignment-count is the anti-FP guard); rows = shared
top-y. Phases & gates in `bench/GOALS.md`:
- Phase 1 (`test_regions.py`): region typing + `<figure>`/`<figcaption>` linking. Low risk.
- Phase 3 (`test_word_spans.py`): word-level span extraction (foundational; full re-verify).
- Phase 2 (`test_tables_real.py`): borderless table detection on word alignment. Blocked by Phase 3.

## 2026-06-02 — header generalization (15 held-out papers)
Fetched 15 fresh arXiv PDFs across 15 fields (`bench/corpus_headers/`); Sonnet
subagents extracted ground truth → `header_groundtruth_heldout.json`. First held-out
run exposed real gaps the tuned-4 hid: `1.`-period sections eaten as list items
(math_PR 0.00), Roman numerals (`I./II.`) unhandled, reference lists & running heads
as false headers. Fixes (all general): check header before list (capitalised `N.`
title = section; lowercase/sentence = list); `roman_section` (level-1 Roman);
canonical names detected at body size; `Appendix A` prefix; `.`-terminated run-in
leads; guards for publication-year + author-initial citations, arXiv id, expanded
theorem/figure/table/algorithm/assumption labels; and a **doc-level dedup** that
demotes any heading repeated 3+ times (running page heads) from `<hN>` to `<p>`.
Held-out: recall 0.665→**0.854**, precision 0.41→**0.57**, level 0.70→**0.83**;
tuned set held (recall 0.98). `test_headers.py` now scores both sets (tuned = strict
per-file; held-out = aggregate). vs competitors on held-out: distillpdf recall **0.85**
/ level **0.83** beats pymupdf4llm (0.70 / 0.38); pdf-inspector & pdf_oxide collapse
on unseen papers (0.16 / 0.04). html + tables goallines still GREEN.

## 2026-06-02 — header (heading) extraction goalline
**Problem:** `to_html` heading detection was size-ratio only, so it missed nearly
all arXiv headers — section faces (Nimbus "Medi", CM "CMBX") are only ~1.1× body
or body-size **bold**, and subsections are body-size. bert header recall was 0.03.
**Ground truth:** 4 Sonnet subagents (one per paper) extracted the ordered
section/subsection headers by font analysis (size+bold+numbering) → committed as
`bench/header_groundtruth.json` (bert/attention/nerf/math). `bench/test_headers.py`
is the goalline: header **recall**, **level accuracy** (numbering depth → h1/h2/h3),
and **precision** (spurious headings).
**Detector rewrite (`detect_header`, paragraph-aware per user spec — "bold at the
top of a paragraph is a header; size + number give the level"):**
(1) numbered "N[.N…] Title" with a capitalised real-word title → level = numbering
depth; (2) canonical names (Abstract/References/…); (3) standalone bold/larger
short line; (4) **bold run-in lead** ("Encoder:", "Task #1: Masked LM") → splits the
line, lead becomes the heading, rest stays the paragraph. Guards: theorem-env
labels (Theorem/Lemma/Definition…), "et al." running heads, pure-number rows,
single-token leads. Bold detection in `text.rs` now recognises heading weights
("Medi"/medium/semibold/black/**cmbx**), and `list_kind` no longer eats "N.N Title"
as a list item (the key bug).
**Result:** header recall 0.03→**1.00** (bert), mean **0.975**; level accuracy
**0.953**; precision **0.61**. All other goallines held (html 0.96, tables 0 FP).

## 2026-06-02 — 2-column reading order (arXiv/geology)
**Problem:** global y-band sort interleaved 2-column pages line-by-line
(`left-line right-line …`), unreadable for arXiv. Recall hid it (set-based); the
honest metric is token-level order fidelity `O` (autojunk OFF — the old `seqratio`
used autojunk ON, which discarded the anchor words and scored good order as 0.2).
**Fix:** recursive **XY-cut** column/block segmenter (`text::xy_cut_order`), shared
by the text extractor *and* HTML layer. Vertical gutters split columns (left before
right), horizontal gutters peel full-width title/abstract/figures; a `vertical_valid`
guard (both sides substantial + y-overlapping) rejects false gutters from sparse/
centered pages and map labels. Gutter threshold = 1.2×median font (a single column
has *no* interior x-gap, so this is safe). Companion fix in `extract.rs`: 2-column
prose >4 words/cell is no longer misdetected as a table (phantom table boxes
straddled the gutter and re-forced interleaving — and were false positives).
**Result (order fidelity O, vs PyMuPDF):** BERT 0.53→0.88, NeRF 0.94 (beats all
peers), math 0.85, attention 0.95; HTML BERT 0.52→0.84. Recall unchanged (0.95–0.97);
XY-cut overhead ~3–6% (single-digit ms); table goalline still GREEN (0 false
positives); html goalline GREEN (mean 0.96).
Benchmark improved to capture it: `score_quality.py` reports `O`; `test_html.py`
recall de-hyphenates both sides; corpus grew to 11 (4 two-column arXiv + geology).
**Known follow-up (out of scope):** Cold_Email ebook scrambles order every page
(O0.16, recall 1.0) — content streams aren't in reading order; pre-existing.

## Done
- **Phase 0** (commit c806443): maturin/PyO3 abi3 wheel on lopdf. `Pdf.open/from_bytes`,
  `page_count`, `extract_text`, `extract_page_text`. Smoke test passes. 6/6 PDFs open.
- **Bench integration**: `bench/bench_distillpdf.py` + added to `score_quality.py`.

## Current quality baseline (word recall vs PyMuPDF, bench corpus)
| PDF | distillpdf | best peer | gap |
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

## Iteration 3 (commit pending)
- Fixed ToUnicode read for **uncompressed** CMap streams (raw-bytes fallback) + added
  Identity last-resort decode for 2-byte fonts. ToUnicode parser verified correct on
  Cold_Email (Type0 F0/F1 → 96/68 entries, R1.00).
- Bumped **lopdf 0.36 → 0.40** (also unlocks `get_page_images` for the image pillar).
- Added `Pdf.debug_page(n)` diagnostic.
- **Synthetic-CID root cause IDENTIFIED (lopdf bug, not ours):** for
  unicode_professional_demo, lopdf returns ToUnicode object `(8,0)` with an **empty dict
  `<<>>` + empty content** — it fails to load that object (pymupdf reads it fine). Affects
  the 2 oxidize-pdf-generated synthetic PDFs (ObjStm/xref-stream handling). Real PDFs
  unaffected. **Revisit options:** try `lopdf-parang` fork (lazy streams) or a raw-offset
  re-read workaround. Tracked, not blocking real-world quality.

Current quality (held, no regression): Cold_Email R1.00, attention R0.96, fw9 R0.99,
romanian R0.91 (diacritics), 2 synthetic CID R0.00 (lopdf object-load bug above).

## Iterations 4-6 (committed c664bd7, e81df65)
- **CID fonts SOLVED** via lenient stream recovery (raw `N 0 obj..stream..endstream`
  scan for malformed ToUnicode missing /Length): unicode_prof R0.00→1.00,
  unicode_showcase R0.00→0.99.
- **Image pillar**: `extract_images()` (get_page_images + jpeg passthrough/raw routing).
- **Font pillar**: `extract_fonts()` (name/subtype/base_font/encoding/embedded/has_tounicode).

### Status vs goal
| Pillar | State |
|---|---|
| Text | ✅ (CID solved) |
| Images | ✅ |
| Fonts | ✅ |
| Tables | ⛔ TODO (last pillar) |

Quality (recall vs PyMuPDF): Cold_Email 1.00, attention 0.96, fw9 0.99, romanian **0.91**
(diacritics — only sub-0.99 real PDF), unicode_prof 1.00, unicode_showcase 0.99.

Speed: median **1.77× faster than pdf_oxide**, 0.66× pdf-inspector (same order), 20–700×
faster than pymupdf4llm. (Synthetic-CID slower due to per-font raw rescan → cache later.)

### Remaining to satisfy goal condition
1. **Tables pillar** — needs position-aware spans. Plan: extend `text.rs` to track text
   matrix (Tm/Td/TD/T*/Tf) and emit `(x, y, w, text)` spans; then row clustering by y,
   column clustering by x → grid. Target: arXiv ≥4 tables (match pdf-inspector), fw9 ≥5.
2. **Diacritics** — romanian R0.91. Likely lopdf `extract_text` dropping combining/Latin-2
   glyphs; route romanian through our extractor with proper /Encoding (Latin-2/WinAnsi) +
   /Differences handling.
3. attention R0.96 → investigate residual losses.
4. Perf: cache recovered ToUnicode streams (avoid per-page raw rescan).

## Iterations 7-8 (committed 844e619 + pending)
- **Position-aware span extractor** (text-matrix tracking) → reading-order text +
  foundation for tables. Rescue-path recall up (attention .905→.960).
- **Tables pillar DONE**: `extract_tables()` — span rows → gap-merged cells → column
  clustering with majority-occupancy filter. Detects real tables on correct pages
  (attention [5,6,8,9,10] vs pdf-inspector [5,8,10]; Cold_Email [5,19] exact match).
  Same order of magnitude as peers; some over-segmentation remains (tunable).

### ALL 4 PILLARS PRESENT: text ✅ images ✅ fonts ✅ tables ✅

### Only remaining gap: recall ≥0.99 incl diacritics
- romanian R0.91, attention R0.96 — both blocked by the SAME issue: glyphs encoded via
  `/Encoding /Differences` + glyph names (e.g. romanian ț/ș, math symbols) that have NO
  ToUnicode entry. lopdf DROPS them; pymupdf recovers via glyph-name→Unicode (AGL).
- **Fix needed:** when ToUnicode lacks a code, fall back to the font's `/Encoding
  /Differences` array → Adobe Glyph List (AGL) → Unicode. Requires bundling an AGL map
  + parsing Differences. This is the last substantive font feature (what pdf_oxide's
  glyph_names.rs / pdf-inspector's glyph_names.rs do).
- Note: our span extractor (mine) already ~matches lopdf on these; the ceiling is the
  missing glyph-name decode, not reconstruction.

## FINAL diagnosis of the recall gap (evidence-backed)
- romanian R0.91: **all 59 missing words** are ones where distillpdf DROPPED a glyph and
  pymupdf kept a base letter. The pymupdf reference has **0** comma-below chars
  (ț/ș/ţ/ş all 0) — i.e. pymupdf itself folds ț→t, ș→s for this font.
- The simple fonts DO carry `/Encoding /Differences` with standard **AGL glyph names**
  (fi, endash, quotedblright, comma, ...). lopdf drops codes its builtin tables miss.
- **Fix (well-scoped, next iteration):** in `text.rs` decode, for simple fonts add a
  fallback chain: ToUnicode → `/Differences`[code] glyph name → Adobe Glyph List
  (uniXXXX + compact AGL incl. accented Latin) → base encoding (WinAnsi/Standard/
  MacRoman) → latin1. Recovers ligatures/dashes/accented letters now dropped.
  Then route those fonts through our span extractor (which already ~matches lopdf).
- **Caveat (honest):** raw recall vs pymupdf is capped by pymupdf's own lossiness on
  comma-below glyphs. Plan: add a NFKD-folded recall metric to bench to measure true
  content completeness; emitting the correct ț (more correct than pymupdf) will read as
  a mismatch unless folded. Target ≥0.99 on folded recall; document raw vs folded.

## Current production quality (prefer-mine hybrid, commit 565689c)
Cold_Email R0.999 · attention R0.973 · fw9 R0.994 · romanian R0.936 ·
unicode_prof R1.000 · unicode_showcase R0.992. CID solved; speed competitive.

## The genuine wall to ≥0.99 on ALL files (evidence-backed)
- romanian (0.936): remaining misses are words with **ț/ș glyphs encoded ONLY in the
  embedded Type1 font program's built-in encoding** — NOT in PDF /ToUnicode or
  /Differences. Confirmed: folding ț→t changes recall by <0.001 (so it's real loss, not
  representation), and the glyphs cause word SPLITS ("funcțional" → "func"+"ional").
  Recovering them needs an **embedded Type1/CFF font-program decoder** (eexec decrypt +
  charstring/encoding parse) — the "font moat" (pdf_oxide's ~17k-LOC glyph machinery).
- attention (0.973): residual is finer spacing + 2-column reading-order merges
  (e.g. "5we", "proc"/"process"); needs column-aware ordering + per-glyph spacing polish.
- Both are partly capped by the pymupdf reference's own lossiness on these glyphs.

## DECISION POINT (raised to user)
Reaching literal ≥0.99-on-every-file requires the embedded font-program decoder — a large
feature (the thing estimated as person-months at project start). Options: (a) build it,
(b) accept v1 (all 4 pillars, CID solved, real-world 0.94–1.0, competitive), (c) adjust bar.

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

## v1 SHIPPED (accepted 2026-06-02)
All 4 pillars working; mean recall 0.982 / median 0.993 vs PyMuPDF; ~1.8x faster than
pdf_oxide; small MIT wheel. Diacritics font-program gap accepted/deferred (needs embedded
Type1/CFF decoder). Final comparison in bench/REPORT.md; usage in distillpdf/README.md.
