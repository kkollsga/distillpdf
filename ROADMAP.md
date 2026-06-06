# distillPDF roadmap

Prioritized fidelity worklist. Source: the **Sonnet second-opinion review** (judges *full*
`to_html()` fidelity — math, tables, reading order, figure grounding, unicode — which the
exact-match gate suites don't measure). To re-run the review see **`second-opinion-claude.md`**;
the full raw report is **`benchmarking/SONNET_REVIEW.md`**.

Latest run: **2026-06-05**, 25 docs, corpus mean **0.448**.
Worst docs: Spanish unicode 0.04 · Moderna 10-K 0.20 · NVIDIA 10-K 0.28 · SpaceX S-1 0.38.

> Workflow: pick the top open item → fix it → add its regression check (below) in the same PR.
>
> **Regression gate (run after EVERY change):** `benchmarking/corpus_tests/test_regression.py`
> is the consolidated, cross-detection-type gate — it runs every corpus suite, merges their
> metrics, and fails loudly if *any* metric drops below `baseline.json` (the compartmentalization
> enforcer: a tables edit that moves a heading/figure metric fails here). `--update` ratchets the
> baseline (intentional gains only). This exists because the band-model change once silently
> regressed `table_cells` 0.677→0.535 while each suite printed its own report and no single check
> asked "did anything regress, anywhere?".

## Now — quick wins (high value, low effort)

- [ ] Suppress running headers/footers and page/line numbers by detecting text that repeats at a fixed top/bottom y-position across pages (only a page number varying).
- [ ] Add a Windows-1252/Mac-Roman C1-byte remap (0x96->en-dash, 0x92->apostrophe, 0x93/0x94->quotes, etc.
- [x] Emit table captions as `<caption>` inside `<table>` (figure captions already use `<figcaption>`). **DONE** — linked caption is the table's first child (`caption-side:bottom` keeps below-table placement); BERT's 8 captions now linked, no sibling `<div>`, suites green.
- [ ] Stop emitting PDF image-resource names ('nvidialogoa1', '562', 'modernalogo.
- [ ] Guard against false <ol>/<li>: do not start a list item mid-sentence after 'Figure'/'Table'/'(' or a bare '.
- [ ] Reuse the already-correct <nav> TOC as ground truth: if a TOC entry appears as a body <p>, promote it to the right heading level; if a body heading is absent from the TOC, demote it.
- [ ] Apply standard dehyphenation (join 'word-\n' + lowercase continuation) to clear soft-hyphen artifacts.

## Themes (impact-ordered)

### 1. Tables lose their column/row structure (split, merged, fragmented, or dropped entirely)
`high` · freq 21 · effort large · area `tables` · **status: IN PROGRESS** (core column model rebuilt 2026-06-05; band+left-x dual-path + diagram rejection 2026-06-06)

- **Shipped (2026-06-06) — band/left-x dual-path + commutative-diagram rejection** (`src/extract.rs`):
  - **Dual-path column model.** The whitespace-lane band model (`column_bands`) is the
    primary; on its degeneration — a wide first-column label bridges a lane and collapses
    the grid to <2 columns (Transformer Table 1, cs_CV) — `flush` now falls back to left-x
    clustering (`columns`/`nearest_col`) behind a ≥0.5 density bar (so a sparse symbol
    *scatter* isn't resurrected as a spurious table). Restored corpus `table_cells`
    0.535→**0.684** and lifted `table_presence` 0.895→**1.000**, no other metric moved.
  - **Commutative-diagram rejection** in `is_coherent_grid`: a grid carrying a
    category-theory morphism glyph (`→ ↦ ⟨ ⟩ …`), not word-dominated, and free of decimal
    data is a diagram, not a table (math_AG, math_GT). This removes the false-positive
    table AND lets the diagram region emit as a `<figure>` — restored `figure_render` to
    0.962. A real data table (decimals present, e.g. physics_optics) or a word-heavy
    transition table survives. **Lock:** corpus regression gate (base-14/Symbol fonts can't
    encode the morphism glyphs, so no synthetic owned fixture is authorable — same
    limitation as the equation-inequality lock).
- **Shipped (2026-06-05) — two-pass "table profile" column model** (`src/extract.rs`):
  - **Admission decoupled from column-keeping.** A single structural test
    `is_coherent_grid` (prose / equation / matrix rejection) now runs on the region's
    content, separate from how many columns survive — so recovering a sparse column can
    no longer flip a prose/equation region into a false-positive table. This was the root
    cause of the earlier whack-a-mole.
  - **Whitespace-lane columns (`column_bands`).** Columns are read off as the covered
    x-bands between clear vertical lanes (an event sweep over cell intervals), keyed on
    *where text sits* not where it starts — so right-aligned numeric columns stay
    distinct. **10-K financial tables now extract 7–9 columns** (were collapsing to a
    few); right-aligned mixed-width values land each in their own cell.
  - **Header-named sparse-column survival.** A band the header names survives even when
    the body rarely fills it (keep iff `support·2 ≥ rows` OR `named`) — wide sparse
    tables keep all their columns instead of folding into a neighbour.
  - **Equation guard** now treats relational operators (≤ ≥ ≠ ≈ …) as equation evidence,
    not just `=`.
- **Tests (locks):** owned scorer `tests/test_tables.py` (col accuracy 0.957→**1.000**,
  detection 1.0, 0 FP) + new `tests/test_table_columns.py` (sparse-survival,
  right-aligned isolation, prose-not-table, equation-not-table, determinism) + 4 new
  `tbl_lock_*` fixtures in `tests/gen_tables.py`. Corpus `math_as_table`/`prose_as_table`
  gates stay green.
- **Still OPEN / deferred:**
  - **Tightly-packed right-aligned numeric tables** (Transformer Table 3): adjacent
    columns whose values occasionally bridge the gutter still merge under lane detection;
    needs alignment-aware (right-edge) sub-splitting + the deferred superscript work (its
    subscripts land on a 2nd line). No regression vs baseline.
  - **Inline multi-tier-header colspan** (group label over sub-columns *within* the data
    run): the data columns are correct, but the group row isn't lifted into `<thead>`
    with colspans yet — needs coordinated `table_html` th/td changes. Stranded group
    headers map via x-overlap as before.
  - **Cross-page table continuation** — needs cross-page state above the per-page loop in
    `src/html.rs`; not containable to extract.rs.
  - **Superscript/exponent recovery** ("O(n²·d)") — root cause in `src/text.rs` (text-rise
    folded into y; `Span` has no `script` field).
- **Affected (21):** ai_attention_transformer_arxiv, bio_protein_design_global_context_arxiv, chem_arxiv, economics_finance_arxiv, geology_usgs_bogoslof_volcano, geology_usgs_volcanic_hazards_california, med_covid_bnt162b2_medrxiv, med_crispr_clinical_trials_pmc, med_crispr_sicklecell_pmc, med_mrna_vaccine_immunology_pmc …

### 2. Heading classifier mislabels non-headings and demotes real section titles
`high` · freq 21 · effort large · area `headings` · **status: IN PROGRESS** (content filters shipped 2026-06-06)

- **Shipped (2026-06-06) — content-based rejection in `detect_header` (`src/html.rs`):**
  Two earlier hypotheses were falsified against the real docs and dropped — *positional
  isolation* (the false headings are sparse/large/distinct-font, so it had zero effect)
  and *outline-trust* (SEC filings carry no `/Outlines` at all). The real discriminators
  are CONTENT:
  - **Math/equation-fragment rejection** (generalized): a line with ≤1 real word that is
    symbol/single-char/digit dominated, plus a symbol-soup density rule
    (`math ≥ 3 && math ≥ real_words + 2`) for mis-extracted equations with a few gibberish
    tokens. `space_moon_lunar_arxiv` 43→14 equation `<h1>`; higgs equation headings gone.
  - **Value/sentence-fragment rejection** (`looks_like_clause`): a clause / value+unit
    phrase ("158 GeV at 95% confidence level have been set", "7 TeV at the LHC, which
    were…") is not a heading — including when its leading value is mis-read as a section
    number. Gone from higgs.
  - **Author/collaboration byline rejection** ("The ATLAS Collaboration").
  - Locked by `tests/test_heading_traps.py` + `gen_heading_traps` fixture; **no arXiv
    recall regression** (corpus `test_headers.py` held-out recall 0.807, precision
    0.755→0.758). Reuses a `HeadingKind{Reliable,Style,RunIn}` tag so the
    numbered/canonical paths bypass the filters (recall guard).
- **Shipped (2026-06-06) — global pre-detection pass + SEC anchors (`src/html.rs`):**
  - **`plan_headings`** (runs once before the parallel render): counts, per style
    signature, how many lines yield a `Style` heading verdict, and DISTRUSTS a signature
    used more than ~once per page (`> max(90, pages)`) — an emphasis/label style, not a
    heading style. `header_at` drops distrusted `Style` verdicts; `Reliable`/`RunIn`
    (numbered/canonical/anchors/run-ins) bypass. Deterministic (integer counts, read-only).
    **NVIDIA 244→73 headings** (314 address-boilerplate lines distrusted); SpaceX 330
    underwriter-address lines distrusted.
  - **`sec_anchor`** — "PART I" / "Item 1A. Risk Factors" recognised as `Reliable` headings
    at section/subsection level (10-K/S-1 ship no outline); guarded so a sentence that
    merely mentions a part ("Part I of Form W-9. If you…") is not matched. NVIDIA 29 /
    Moderna 38 Items now correctly levelled.
  - Locked by `tests/test_sec_headings.py` + `gen_sec_structure` fixture; arXiv header gate
    unchanged (recall 0.844, precision 0.758); `bad_headings` T4 / `title_not_h1` T11 green.
- **Still OPEN / next:**
  - **Mixed-signature SEC flood** (Moderna 270 `<h4>`, SpaceX 422): the cover/section style
    is shared 50/50 by real headings and boilerplate at <1 line/page, so signature-level
    distrust can't separate them — needs content-level cover-boilerplate filtering.
  - Title-split / completeness (higgs title across two `<h2>`); residual equation fragments
    (need the deferred text.rs superscript fix); running-header/footer suppression.
- **Test:** headings/page bounded; no <h*> that is a citation, "Proof.", author name, or sentence fragment (extend bad_headings T4).
- **Affected (21):** ai_attention_transformer_arxiv, bio_protein_design_genai_arxiv, bio_protein_design_global_context_arxiv, economics_finance_arxiv, geology_seismology_arxiv, geology_usgs_bogoslof_volcano, geology_usgs_volcanic_hazards_california, math_ricci_flow_arxiv, med_covid_bnt162b2_medrxiv, med_crispr_clinical_trials_pmc …

### 3. Figure captions not semantically linked, missing, or contaminated
`high` · freq 19 · effort medium · area `captions` · **status: IN PROGRESS** (caption-number + Continued + labels shipped 2026-06-06)

- **Shipped (2026-06-06) — `caption_label`/`caption_parts`, `num_id`, `caption_is_continued`
  (`src/html.rs`):**
  - **Full hierarchical caption numbers** (NASA databook bug fixed): `caption_parts` captures
    the whole number token `DIGITS(('.'|'-')DIGITS)*LETTER?` ("6.2.1", "0.1-1", "1A"); `num_id`
    slugs it for the id only (`fig-6-2-1`), the displayed caption keeps "6.2.1". NASA databook:
    `fig-0`/`tab-0` collisions → 30 distinct hierarchical ids.
  - **Multi-page "Continued" suppression** (`caption_is_continued`): "Figure 5.—Continued"
    markers no longer emit duplicate `fig-5-2…` captions. Geology USGS: 6× duplicates gone.
  - **Broader labels:** "Fig N" (no dot), "Cuadro" (Spanish), "Box N", Nature "Figure N |";
    new keywords also added to `is_inline_xref` so body references stay un-captioned.
  - Locked by `tests/test_captions.py` + `gen_figure_hier`/`_continued`/`_nodot` fixtures.
    Gates green: `ref_as_caption` T7, `figure_caption_decoupled` T1, `duplicate_ids` T1;
    all 26 existing figure/demo/markdown tests pass.
- **Still OPEN / out of scope:** raster-BAKED captions (med_covid, bio_protein emit 0
  figcaption — only inline "(Fig. N)" xrefs are extractable → OCR territory); an
  `is_inline_xref` predicate false-positive that drops real captions beginning "Figure N.
  Plot showing…" ("plot"/"show" read as predicates); caption containment / anti-contamination
  (x-range-constrain `gather_caption`).
- **Test:** every Figure/Table caption is a <caption>/<figcaption> linked to its object; no caption-only <figure>/<div>.
- **Affected (19):** ai_attention_transformer_arxiv, bio_protein_design_genai_arxiv, bio_protein_design_global_context_arxiv, chem_arxiv, economics_finance_arxiv, geology_usgs_bogoslof_volcano, geology_usgs_volcanic_hazards_california, med_covid_bnt162b2_medrxiv, med_crispr_clinical_trials_pmc, med_crispr_sicklecell_pmc …

### 4. Math/equations fragmented, with Greek letters and operators dropped or mis-mapped
`high` · freq 15 · effort large · area `math` · **status: IN PROGRESS** (glyph recovery shipped 2026-06-06; gated script reattach 2026-06-06)

- **Shipped (2026-06-06) — gated sub/superscript reattach in `src/html.rs` `lines_of`:**
  a dense math cluster's offset glyphs (subscript indices i/j/K, primes, ∂Σ) were left at
  their raised/lowered y, so `xy_cut` ordered them out of line and orphaned them into
  spurious `<aside>`s. `order_y` now re-bases a *clustered* script onto its host glyph's
  baseline — **but only when the host is a FORMULA row** (symbol-dominated): a larger-font
  host, a **bold** host, or a **word-dominated** host row (a body-sized, unbolded section
  title the classifier finds structurally — e.g. "H.2. … Suboptimal O(T^{3/4}) Regret …")
  is left untouched, so the earlier spill (an exponent pulled into a heading; list/table
  churn) cannot recur. Plus a post-pass folds a punctuation-only line (`,`/`.` left behind
  by a reordered equation) onto the preceding line, so no stray `<p>,</p>`. **Orphaned
  fragments: arxiv_math 24→5, math_GT 37→18, math_PR 10→9 asides**, every other metric ≥
  baseline. **Lock:** the consolidated regression gate (a faithful synthetic that
  reproduces page-level xy-cut orphaning + the body-sized-heading edge case would be
  brittle; the gate baselines `bad_headings`/`single_item_list`/`stray_comma_p` at their
  good values so the regressions can't return).
- **Shipped (2026-06-06) — glyph recovery in `src/text.rs`:** the dominant defect was
  GLYPH DROPPING (a math font with no ToUnicode → bytes hit the raw fallback → control
  chars → dropped by `push_norm`). Sub/superscript was already handled (`Run.script` is live).
  - **CM-math byte tables** `cmmi_code` (Greek α–ω, Γ–Ω, ∂, math-italic Latin) + `cmsy_code`
    (relations/operators − · × ± ≤ ≥ ≈ ⊂ ⊃ ∈ ∀ ∃ ∇ ← → ↔ ⇒ ∝ ∞ √ …), enabled via a
    `cm_math` flag for CMMI/CMMIB/EUFM and CMSY/CMBSY/MSAM/MSBM fonts without ToUnicode
    (mirrors `ot1_text_code`). **physics_higgs_cms: Greek 0 → 378, operators 0 → 224,
    control chars 32 → 0** — "H → γγ, ZZ", "Z → μμ", "mH ≤ 13" now read correctly.
  - **Math glyph NAMES in `glyph_to_str`** (`alpha`…`omega`, `Gamma`…`Omega`, `summation`,
    `integral`, `radical`, `lessequal`, `greaterequal`, `element`, `partialdiff`, …) for
    `/Differences` math fonts (PazoMath, Euler). higgs_atlas operators 427 → 498.
  - **Windows-1252 C1 remap** `c1_remap` (0x91–0x97/0x85/0x95 → ‘ ’ “ ” – — … •) in the raw
    fallback — fixes the em-dash/smart-quote mojibake (gravitational_waves control chars
    32 → 0). med_oncology `≥` recovered.
  - Locked by `tests/test_mathfonts.py` + hand-authored `gen_mathfonts` fixture (exercises
    CMSY/CMMI bytes, `/Differences` names, C1 bytes). Gates green: `math_as_table`,
    `math_no_subsup`; `test_math.py`/`test_typography.py`/`test_text.py` unchanged.
- **Still OPEN / deferred:**
  - **Display-equation fragmentation** (the other class): docs WITH ToUnicode (ricci, atlas)
    map glyphs fine but shatter display equations across `<p>`/`<aside>`. The orphaned
    *index-fragment* subset is now largely fixed by the gated script reattach above
    (asides roughly halved on the math docs), but multi-line display blocks still split —
    the remaining work is positive equation-region grouping (atomic block + eq-number
    anchoring) in html.rs, which the reattach intentionally does NOT attempt.
  - **CMEX big-operator delimiters** (∑/∫ displaystyle) — its extensible-recipe slots are
    error-prone; deferred (a wrong map is worse than a drop). Full
    WinAnsi/MacRoman/Standard base tables; MathML output; markdown `<sub>` drop.
- **Test:** no math inside <table>/<li>; <sub>/<sup> present where expected; ∫/Σ/Greek survive.
- **Affected (15):** ai_attention_transformer_arxiv, bio_protein_design_global_context_arxiv, chem_arxiv, economics_finance_arxiv, geology_seismology_arxiv, geology_usgs_bogoslof_volcano, math_ricci_flow_arxiv, med_mrna_vaccine_immunology_pmc, med_oncology_immunotherapy_pmc, physics_gravitational_waves_arxiv …

### 5. Raster/vector figure content silently missing (empty <figure>, dropped images, or SVG with no text labels)
`high` · freq 14 · effort large · area `figures` · **status: IN PROGRESS** (LoF suppression + caption-aware vector recovery 2026-06-06)

- **Shipped (2026-06-06) — pure-Rust figure recovery.** Measurement reframed the problem: the
  dominant "empty figure" cause was NOT decode failures (all corpus images decode) but two
  things, both now fixed:
  - **List-of-Figures / dot-leader suppression** (`src/html.rs is_dotleader_toc`): a ToC line
    that merely looks like a caption ("Figure 3.1: title … . . . . 95") no longer emits an
    empty `<figure>`. Scans the caption line + tight continuation lines for a ≥4 leader-dot run
    (wrapped LoF entries). **Kaspersen thesis: 92 → 8 empty figures (84 spurious removed).**
  - **Caption-aware two-tier vector recovery** (`src/vector.rs` strong/weak partition +
    `src/html.rs` promotion): small diagrams below the figure bar (72×54pt/6-path) are kept as
    WEAK candidates and promoted to real `<figure><svg>` ONLY when a figure caption sits
    adjacent (edge ≤ body·6, x-overlap, no strong figure already serving it) — so stray marks
    are never resurrected. arxiv_math ellipse diagrams recovered; corpus empty figures
    ~100 → 0 spurious (genuine graphic-less captions stay as honest `<figure><figcaption>`).
  - **Decision:** pure-Rust, no native PDF-renderer dependency (would break "no system deps" +
    the abi3 single-wheel build). Page-raster fallback, shadings (`sh`), inline images
    (`BI/ID/EI`) DEFERRED (low/zero corpus incidence); the deferred items are reimplementable
    clean-room from the PDF spec (ISO 32000).
  - **Locks:** `tests/test_figure_recovery.py` (LoF-not-figure incl. wrapped; small-vector
    recovered; precision: no promotion without a caption + positive control) on 3 new
    `gen_fixtures.py` fixtures; `empty_figures` count added to the corpus regression gate.
- **Still OPEN / deferred:** real graphics still missing where the figure is cross-page from its
  caption, or its content lives in a Form/shading we don't rasterize (cond-mat Fig 3, cs_LG
  Fig 5, attention Fig 5) — these need cross-page anchoring and/or `sh`/inline-image support.
- **Test:** figure-heavy doc emits non-empty <svg>/<img> per source XObject; no empty <figure>.
- **Affected (14):** ai_attention_transformer_arxiv, chem_arxiv, geology_seismology_arxiv, geology_usgs_bogoslof_volcano, geology_usgs_volcanic_hazards_california, med_covid_bnt162b2_medrxiv, med_mrna_vaccine_immunology_pmc, nonenglish_spanish_astrofisica_arxiv, physics_gravitational_waves_arxiv, physics_higgs_atlas_discovery …

### 6. Two-column and bottom-to-top reading-order failures interleave or reverse content
`high` · freq 11 · effort large · area `reading_order` · **status: IN PROGRESS** (Y-flip reversal + two-column interleaving both fixed 2026-06-06)

- **Shipped (2026-06-06) — Y-flip (bottom-to-top) reversal fix** (`src/text.rs`
  `extract_spans::emit`). Root cause of the catastrophic SEC reversal (Moderna/NVIDIA/SpaceX,
  the worst-scoring docs): pages laid out under a **Y-flip CTM** (`[1 0 0 -1 0 H]`, top-left
  origin) had that CTM *ignored*, leaving y in the flipped frame so a top glyph sorted below a
  bottom one → the whole document emitted bottom-to-top. Fix: for a consistent axis-aligned
  Y-flip CTM (`dm.d<0, b≈c≈0`) emit **device-space** coordinates (`dm.f = H − text_y`),
  restoring y-up. Surgical — identity/pure-translate/scale paths untouched; the common corpus
  is byte-identical and the gate shows no regression. Confirmed against a controlled Y-flip PDF
  (read FIVE..ONE → now ONE..FIVE) and real fetched docs (Higgs, two 10-Ks) unaffected.
  **Lock:** `tests/gen_fixtures.py gen_yflip_page` + `test_text.py::test_yflip_reading_order_not_reversed`.
- **Shipped (2026-06-06) — two-column interleaving fix** (two combined fixes). Root cause was
  **inaccurate span widths**: Standard-14 fonts without a
  `/Widths` array (Higgs body is `Times-Roman`) fell back to a flat 0.5-em guess, so justified
  left-column words overshot ~7pt and closed the narrow (~9pt) PRL gutter — defeating every
  interval-based detector. (1) **Standard-14 AFM widths** (`src/afm.rs`, used in
  `text.rs::font_info`) give real advances → the gutter opens to ~23pt. (2) A **crossing-
  tolerant gutter** in `xy_cut` (`tolerant_vgap`) ignores a stray centered page number /
  running header that still split the lane, gated to SPAN-level ordering only, as a last resort
  after the full-width horizontal peel, and only for **tall** columns (`tall_columns`) so a
  short multi-line caption is never split. Net on the corpus gate: `references` +0.02,
  `figure_caps`/`figure_render` unchanged, all 37 metrics ≥ baseline. Higgs body now reads
  column-by-column. **Lock:** `gen_twocol_tight` + `test_text.py::test_twocol_tight_gutter_reads_in_order`.
- **Test:** known doc: emitted marker words monotonic in (-y, x); no dropped section.
- **Affected (11):** bio_protein_design_global_context_arxiv, geology_usgs_bogoslof_volcano, geology_usgs_volcanic_hazards_california, med_crispr_clinical_trials_pmc, med_crispr_sicklecell_pmc, physics_gravitational_waves_arxiv, physics_higgs_atlas_discovery, sec_moderna_10k, sec_nvidia_10k, sec_spacex_s1_ipo …

### 7. Unicode encoding errors: dropped/mis-mapped glyphs, mojibake, decomposed diacritics
`medium` · freq 13 · effort medium · area `unicode` · **status: OPEN**

- **Fix:** Add a text-normalization post-pass: map the Windows-1252 C1 range (0x80-0x9F) to correct Unicode (0x96->U+2013, 0x93/0x94->U+201C/U+201D, 0x92->U+2019), apply equivalent Mac-Roman mapping, and run NFC normalization to recompose base+combining-diacritic sequences (also stripping stray spaces between a base letter and a combining mark). Map ligature code points U+FB01/FB02 to their letters. Add font-specific fixes for the custom '+' glyph (->U+002B) and audit Greek capital accents (É) in the glyph->unicode table.
- **Test:** no C1 mojibake / decimal→":"; NFC output; ligatures resolved (extend stray_comma_p T19).
- **Affected (13):** bio_protein_design_genai_arxiv, chem_arxiv, geology_seismology_arxiv, math_ricci_flow_arxiv, med_crispr_sicklecell_pmc, med_mrna_vaccine_immunology_pmc, med_oncology_immunotherapy_pmc, nonenglish_spanish_astrofisica_arxiv, physics_gravitational_waves_arxiv, physics_higgs_atlas_discovery …

### 8. References not grouped as single units / inconsistent list markup / spurious tables
`medium` · freq 11 · effort medium · area `references` · **status: OPEN**

- **Fix:** In the references region, merge consecutive blocks that do not start with a new '[N]'/'N.' reference marker into one entry (single <p> or <li>), detecting hanging-indent continuation. Normalize all entries to one consistent markup with a correct start index. Suppress table detection inside the references region (flatten any detected table whose cells contain author-name/DOI fragments to <p>). Couple with the heading-false-positive fix so reference titles are not promoted to headings.
- **Test:** bibliography entries grouped/separable; not rendered as a spurious <table>.
- **Affected (11):** ai_attention_transformer_arxiv, bio_protein_design_genai_arxiv, chem_arxiv, geology_seismology_arxiv, med_covid_bnt162b2_medrxiv, med_crispr_clinical_trials_pmc, med_crispr_sicklecell_pmc, med_mrna_vaccine_immunology_pmc, med_oncology_immunotherapy_pmc, physics_gravitational_waves_arxiv …

### 9. Author/title blocks fused, split, or mis-promoted to headings
`medium` · freq 11 · effort medium · area `headings` · **status: OPEN**

- **Fix:** Treat the page-1 title block as the largest centered/bold run before the author list and reconstruct multi-line titles into a single <h1> (merge adjacent same-font centered fragments with no intervening paragraph); when nav/TOC text for the title is longer than the h1 content, adopt the nav text as canonical. Detect the author+affiliation block as a unit (names with superscript affiliation markers) and emit as structured <p>/definition-list, never as headings/sections. Suppress heading promotion for affiliation lines containing institution/university keywords.
- **Test:** headings/page bounded; no <h*> that is a citation, "Proof.", author name, or sentence fragment (extend bad_headings T4).
- **Affected (11):** ai_attention_transformer_arxiv, bio_protein_design_global_context_arxiv, chem_arxiv, economics_finance_arxiv, geology_seismology_arxiv, med_covid_bnt162b2_medrxiv, med_crispr_clinical_trials_pmc, med_crispr_sicklecell_pmc, med_mrna_vaccine_immunology_pmc, physics_higgs_atlas_discovery …

### 10. Running headers/footers, page numbers, and line numbers leak into body content
`medium` · freq 10 · effort medium · area `reading_order` · **status: OPEN**

- **Fix:** Detect recurring header/footer content by matching text that repeats at a consistent y-position near the page top/bottom across consecutive pages (allowing only a trailing page number to vary) and suppress it or move it to a <header>/<aside> outside the content flow. Strip isolated incrementing integer tokens in the left margin (manuscript line numbers) and at page top/bottom (page numbers). After suppression, merge the flanking paragraph fragments that were split by the removed header.
- **Test:** known doc: emitted marker words monotonic in (-y, x); no dropped section.
- **Affected (10):** geology_seismology_arxiv, geology_usgs_bogoslof_volcano, med_covid_bnt162b2_medrxiv, med_crispr_clinical_trials_pmc, med_crispr_sicklecell_pmc, med_mrna_vaccine_immunology_pmc, physics_gravitational_waves_arxiv, sec_nvidia_10k, space_moon_lunar_arxiv, space_moon_lunar_surface_databook_nasa

### 11. Word-spacing and glyph-spacing failures (run-together words and letter-by-letter spacing)
`medium` · freq 8 · effort medium · area `unicode` · **status: OPEN**

- **Fix:** Implement a single glyph-run merge step driven by inter-glyph advance width: group consecutive glyphs into a word when the gap is below ~0.3x the font space-width, and insert a space when the gap between two runs exceeds ~0.25x the font size (also when a lowercase->uppercase boundary has zero gap). Apply this uniformly to body text, table cells, and SVG <text> before emitting nodes. The California 'tracked'/letter-spaced figure text ('N e a r - s o u r c e') needs the same collapse with a tolerance up to ~1.5x advance width.
- **Test:** no C1 mojibake / decimal→":"; NFC output; ligatures resolved (extend stray_comma_p T19).
- **Affected (8):** bio_protein_design_genai_arxiv, med_crispr_clinical_trials_pmc, sec_moderna_10k, sec_nvidia_10k, sec_spacex_s1_ipo, space_moon_lunar_arxiv, space_moon_lunar_surface_databook_nasa, geology_usgs_volcanic_hazards_california

### 12. Spurious or misclassified list markup (false <ol>/<li>, lost real lists, paragraph-continuation breaks)
`medium` · freq 7 · effort medium · area `lists` · **status: OPEN**

- **Fix:** Require a list item to begin at the start of a visually offset/indented line, not mid-sentence after 'Figure'/'Table'/'(' or a bare period-space; require >=2 sequential list-like entries before emitting an <ol>. When an <ol> immediately follows a </p> whose text ended truncated (e.g. '(Ref') and the first <li> starts with '. ' or lowercase, merge it back into the paragraph and recover the stripped number. Conversely, detect genuine bullet/numbered lists by consistent left-indent + bullet glyph (including encoded variants like '¥'/'•') and emit <ul>/<ol>.
- **Test:** consecutive bullets group into one <ul>; no list started mid-sentence (extend single_item_list T8).
- **Affected (7):** bio_protein_design_genai_arxiv, geology_seismology_arxiv, math_ricci_flow_arxiv, med_crispr_clinical_trials_pmc, med_oncology_immunotherapy_pmc, space_moon_lunar_arxiv, geology_usgs_bogoslof_volcano

### 13. Hyphenation at line breaks not rejoined
`low` · freq 2 · effort small · area `unicode` · **status: OPEN**

- **Fix:** Apply standard dehyphenation: when a token ends with a hyphen at a line break and the next token starts lowercase, join them and drop the hyphen unless the joined form fails a dictionary check; handle the dropped-hyphen variant ('protonproton') via the same word-merge heuristic.
- **Test:** no C1 mojibake / decimal→":"; NFC output; ligatures resolved (extend stray_comma_p T19).
- **Affected (2):** med_crispr_clinical_trials_pmc, physics_higgs_cms_discovery

### 14. Bare URLs and DOIs left unlinked
`low` · freq 1 · effort small · area `links` · **status: OPEN**

- **Fix:** Detect bare URL patterns (http://, https://, www.) in text runs and wrap them in <a href>; additionally harvest PDF link annotation objects and apply them to the overlapping text spans so the link layer is preserved even when not typed inline.
- **Test:** bare URLs/DOIs linked; #ref-N/#eq-N anchors exist (citation_page_anchor T20).
- **Affected (1):** geology_usgs_bogoslof_volcano

## Recently shipped (for context)

- Figure clipPath-id collision → curves no longer overshoot the plot box (0.0.17)
- Stray caption-number label pulled into figures fixed (0.0.17)
- Composite raster+vector figures with coupled captions (0.0.11)
- Document style-profile heading detection (0.0.10)
- CMYK/Adobe-YCCK JPEG black-image fix (0.0.10)
- Front-matter: title/authors+orgs/abstract/keywords (0.0.10)
- to_markdown() + unified image_mode + write-by-default API (0.0.13–0.0.16)

## Notable strengths to preserve

- Citation/cross-reference hyperlinking is excellent across arXiv/journal papers: in-text citations resolve to anchor targets (#cite.X), and figure/equation cross-refs work (ai_attention_transformer, chem_arxiv has all 48 links resolving, med_crispr_clinical_trials has all 208 links resolving). Preserve this.
- References sections are almost always textually complete and individually readable, with DOIs and arXiv URLs preserved as live links (chem_arxiv, bio_protein_design_genai, med_crispr_clinical_trials with all 204 refs).
- Navigation <nav> TOC with section anchors and data-level/data-page attributes is reliably generated and frequently captures the correct outline even when body headings are wrong (med_crispr_clinical_trials, med_mrna_vaccine, med_oncology) - this is a usable ground-truth source for fixing body headings.
- Vector figure -> inline SVG transcoding works well in many arXiv papers, preserving text labels, colored paths, and diagram semantics (ai_attention Figs 3/4, bio_protein_design_global_context all 4 figs, geology_seismology Fig 1, space_mars SVG figures).
- Unicode/accent handling is strong in the better documents: ligatures resolved to ASCII, Greek letters and accented author names preserved without mojibake (ai_attention, chem_arxiv, med_mrna mostly).
- Inline italic/bold emphasis on scientific terms (de novo, organism names, journal titles) and sub/superscript notation for chemical formulae is preserved well in body prose across the medical/bio corpus.
- Single-column reading order is consistently correct with no interleaving artifacts (chem_arxiv, economics_finance, geology_seismology).
