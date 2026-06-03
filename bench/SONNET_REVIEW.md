# Sonnet second-opinion review — distillpdf `to_html()` fidelity

25 independent Sonnet agents each parsed one corpus PDF with distillpdf, read the
**source PDF visually**, and scored fidelity for an LLM reader. A synthesis stage
clustered findings into prioritized themes. Run `wf_49f14683-684`, 2026-06-02.

**Corpus mean quality: 0.499** (Sonnet is far stricter than our exact-match gates —
it judges *full* fidelity: math, reading order, footnote/ref noise, figure grounding —
none of which the carved-out content gate measures). Lowest: code_sample 0.10,
unicode_professional_demo 0.18, unicode_showcase 0.28. Highest: cs_CV 0.72, stat_ML 0.72.

> ⚠️ These are a second opinion — treat as leads, verify before acting. I spot-checked
> the two most surprising "catastrophic + cheap" claims (token duplication; code_sample
> reading order) and **both reproduce**.

---

## Tier 0 — Catastrophic + cheap (do first)

1. **Token double-emission** (theme 16, freq 2, VERIFIED) — `unicode_showcase` /
   `unicode_professional_demo` emit every token twice ("The The Rust Rust",
   "SupportSupport"). 100% duplication → both docs unreadable (0.18/0.28). Likely a
   double-traversal of text-show operators or reading both raw codes *and* a
   ToUnicode/ActualText overlay in one pass. **Effort: small.**

2. **Reading order globally wrong** (theme 6, freq 3, VERIFIED for code_sample) —
   `code_sample` (0.10), `Cold_Email_Hacks` (0.37), `physics_optics` (0.45) emit
   bottom-to-top (Y-sign flip) or raw stream order with word-level interleaving across
   blocks. Fix: group spans into blocks by y-gap FIRST, then sort words L→R within each
   block; audit the PDF→user-space Y transform for a sign flip. Recovers the 3 worst docs.
   **Effort: small–medium.**

3. **Romanian ș/ț split into stray `<p>,</p>`** (theme 19, freq 1) — 21 stray comma
   paragraphs + word-fusion ("exploatăriis,i"). Comma-below diacritics are separate
   glyph+comma objects; merge a lone comma sitting a descender below the preceding glyph.
   **Effort: small.**

## Tier 1 — High impact, pervasive

4. **Figure graphic ↔ caption decoupled** (theme 1, freq 13) — *the single most pervasive
   structural defect.* SVG/img goes in one `<figure>`, the `Figure N` caption in a
   separate `<figure id=fig-N><figcaption>` (often duplicate ids). An LLM can't ground a
   caption to its graphic. Fix: figure-assembly pass — anchor the nearest caption block to
   the emitted graphic by bbox proximity, merge into one `<figure>`, dedupe ids.
   *(This is the "caption-associate SVG figures" task offered earlier.)* **Effort: medium.**

5. **Spurious heading promotion** (theme 4, freq 21) — body fragments, equation
   subscripts, footnotes (9 in astro-ph), 28+ reference entries (cs_DS), "Proof." labels,
   author names tagged `<h1>`–`<h4>`. Pollutes the outline almost everywhere. Fix: gate on
   (≥2pt larger OR bold) AND sole/short content on its line AND NOT a sentence-fragment /
   citation / math pattern; suppress inside table/figure/footnote/ref regions. **Effort: medium.**

6. **Table structure collapse** (theme 5, freq 13) — side-by-side tables concatenated
   column-by-column (BERT Tables 2/3/4 on p7), multi-line headers packed into one `<th>`,
   8–12 columns collapsed to 4, multi-value cells, page-break splits. Fix: (a) split tables
   sharing a y-band by horizontal gap; (b) rebuild columns from x-centroids and reuse them
   to split headers token-by-token; (c) detect multi-line/spanning headers; (d) carry table
   schema across page/column breaks. **Effort: large.**

7. **Two-column scrambling + block loss** (theme 3, freq 11) — left/right columns
   interleaved line/word-wise ("extracts visual features guage model"); whole blocks
   silently dropped (BERT Conclusion, NeRF/quant-ph column-break text). Fix: column
   partition before block assembly (find gutter by x-clustering, emit full left col then
   right); audit the column-end collector that drops column tails. **Effort: large.**

8. **Display equations fragmented** (theme 2, freq 19) — fractions split across `<p>`,
   ∫/Σ become bare "Z"/"X", exponents become `<ul><li>`, display math forced into spurious
   `<table>`. No `<sub>/<sup>` anywhere. Fix: detect display-math regions by bbox clustering
   + eq-number, emit as one block (never `<table>`/`<li>`); baseline/size heuristic →
   `<sub>/<sup>`. **Effort: large.**

9. **Glyph→Unicode failures in math/symbol fonts** (theme 15, freq 8) — ∈→"2", →→"!",
   ∫→"Z", Σ→"X", Greek dropped, decimal point→" : " (473× in NeRF). Fix: CMSY/CMMI slot
   maps, AMS PUA ranges; resolve via Encoding/Differences first; emit U+FFFD instead of
   silently dropping. **Effort: large.**

10. **Two-column prose / titles mis-detected as `<table>`** (theme 10, freq 6) — fw9
    pages 2–6, USGS pages 1–4, doc titles trapped in `<td>`. Fix: only emit `<table>` for
    genuine grid geometry (ruling lines OR x-aligned columns across ≥2 rows of parallel
    content); else column-ordered `<p>`. **Effort: medium.**

11. **Figures missing graphic content** (theme 13, freq 7) — all 8 USGS pie charts, 6 econ
    DAGs (empty `<figure>` + garbled node tables), attention heatmaps absent; some SVGs have
    0 `<text>`. Fix: iterate ALL `/XObject` entries per page; raster fallback to
    `<img data:png>` when vector transcode empties; emit all Tj/TJ within the figure bbox as
    `<text>`. **Effort: large.**

## Tier 2 — Medium

12. **In-text "Figure N/Table N shows…" → caption** (theme 7, freq 11) — *partially fixed*
    (table refs + connective-continuation). Still fires on "Figure N shows/summarizes" verb
    pattern at line start. Fix: require adjacency to a detected graphic AND a formal marker;
    reject cross-reference verb patterns. **Effort: small.**
13. **List items split / one-`<ul>`-per-item / unrecognized markers** (theme 8, freq 9) —
    broaden bullet set (✓✗●–), group consecutive markers, merge continuation into the `<li>`.
14. **Page numbers / running headers / arXiv watermarks leak into body** (theme 9, freq 18) —
    margin/repetition filters; strip before table construction.
15. **Title/abstract not promoted to single `<h1>`** (theme 11, freq 15).
16. **Sub/sup as orphan `<p>`; decomposed accents** (theme 14, freq 13) — NFC-normalize;
    attach 1–2 char offset fragments as `<sub>/<sup>`.
17. **Multi-panel/grid figures exploded into many anonymous `<figure>`** (theme 12, freq 4) —
    up to 69 on one optics page; cluster sub-images under one named figure.
18. **Run-in bold labels ("Proof.") as `<h4>`; algorithm/ref entries inconsistent** (theme 18, freq 6).

## Tier 3 — Low / polish

19. **Hyphenation & split-link artifacts** (theme 17, freq 10) — de-hyphenate line breaks;
    merge adjacent `<a>` with same href.
20. **Citation links target `#page-N` not `#ref-N`/`#eq-N`** (theme 20, freq 4).

---

## Notable strengths (preserve)
- BERT GLUE Table 1 now whole (10 cols, correct headers) — recent fix landed.
- Vector figures transcoded to SVG with rich `<text>` labels (116/86/79/163 nodes on
  BERT figs 1–4).
- Table captions correctly placed below via `caption-side:bottom`.
- Ligatures (ﬁ/ﬂ) resolved; reference lists extracted as separable `<p>`; citation +
  external hyperlinks present.

## Per-document quality (low → high)
0.10 code_sample · 0.18 unicode_professional_demo · 0.28 unicode_showcase · 0.32
geology_usgs_fs · 0.37 Cold_Email_Hacks · 0.42 fw9_form · 0.45 arxiv_math · 0.45
physics_optics · 0.48 eess_SP · 0.48 quant-ph · 0.52 cond-mat / cs_LG / issue-93-romanian /
math_AG / math_GT / math_PR · 0.55 astro-ph · 0.56 cs_CL · 0.58 attention · 0.62 arxiv_bert /
arxiv_nerf / econ_EM / q-bio · 0.64 cs_DS · 0.72 cs_CV / stat_ML
