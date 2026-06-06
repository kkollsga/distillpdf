# Second-opinion review workflow (Sonnet) — runbook

A reproducible "second opinion" on distillPDF's `to_html()` fidelity. **N independent
Sonnet agents** each take ONE PDF, generate the HTML with distillPDF, **read the source PDF
visually**, and score how faithful the HTML is *for an LLM reader*. A synthesis stage
clusters every finding into a prioritized, deduplicated worklist.

It exists because our owned/corpus test suites only check **exact-match structural gates**
(themes T1–T20). Sonnet judges *full* fidelity — math, reading order, table columns,
figure grounding, reference/footnote noise, unicode — which the gates don't measure. So it
surfaces real defects the green test suite is blind to.

> ⚠️ The output is a **second opinion**: treat findings as leads, spot-check the surprising
> "catastrophic + cheap" ones before acting. The first run (2026-06-02, when the project was
> still named `pylopdf`/`pdf4llm`) scored corpus mean **0.499** and produced
> `benchmarking/SONNET_REVIEW.md` — read that for an example of the output and worklist.

---

## Cost & prerequisites

- **Cost:** ~20–25 Sonnet agents + 1 synthesis ≈ **1.5–2M output tokens**. This is a real,
  large spend — only run it when explicitly asked (it needs the Workflow tool, which
  requires explicit opt-in: the user saying "use a workflow"/"run the second opinion",
  `ultracode` on, or this runbook being invoked).
- **distillPDF must be importable** by the Python the agents call. Pick a Python that has a
  current `distillpdf` wheel installed and set `PY` (below) to it, e.g.
  `/opt/miniconda3/bin/python3` (used during this project's dev). Verify:
  `"$PY" -c "import distillpdf; print(distillpdf.__version__)"`.
- Run it **after** a `maturin build` + install so the wheel reflects the code under review.

---

## Step 1 — Assemble a fresh 20-PDF corpus across broad topics

The point of a *fresh* corpus is to avoid overfitting to the docs we've already fixed. Pick
**~20 PDFs spread across domains AND layouts** — the layout diversity is what stresses the
extractor. Target this spread (≈3 per domain), choosing docs that are *visually varied*
(tables, figures, equations, two-column, forms, non-Latin scripts, scanned text):

| Domain | Where to source | What to favour |
|---|---|---|
| **SEC / finance** | SEC EDGAR (10-K / 10-Q / S-1), annual reports | dense financial **tables**, footnotes, multi-page tables |
| **Geology / earth sci** | USGS pubs (pubs.usgs.gov), NPD/Sodir reports, thesis | maps, cross-sections, **vector + raster figures**, color bars |
| **Coding / CS** | arXiv `cs.*`, language specs, a code-listing PDF | code blocks, monospace, two-column papers |
| **Maths** | arXiv `math.*` | display **equations**, ∫/Σ/Greek, theorem/proof structure |
| **Space / astro / physics** | arXiv `astro-ph`, `physics.*`, `quant-ph` | multi-column, plots, sub/superscripts |
| **Medicine / bio** | PubMed Central, medRxiv, bioRxiv, `q-bio` | clinical tables, figures, references, sometimes scanned |

Plus a few **deliberate stress cases** to round out 20: a tax/government **form** (e.g. IRS
W-9), a **non-English / diacritics** doc (Romanian ș/ț, CJK, Cyrillic), a **slide deck**, and
a **unicode/symbol-heavy** doc.

Drop them in a corpus folder and build the `args` list — an array of `{name, pdf}` with
**absolute** paths (`name` is a slug used for the temp HTML filename and the report):

```bash
mkdir -p bench/secondopinion/corpus
# ... download ~20 PDFs into it, then generate the args list:
PY=/opt/miniconda3/bin/python3   # a python with distillpdf installed
"$PY" - <<'PY'
import os, json
d = os.path.abspath("bench/secondopinion/corpus")
docs = [{"name": os.path.splitext(f)[0], "pdf": os.path.join(d, f)}
        for f in sorted(os.listdir(d)) if f.lower().endswith(".pdf")]
print(len(docs), "docs"); json.dump(docs, open("bench/secondopinion/args.json", "w"), indent=1)
PY
```

`args.json` is what you pass as the Workflow `args`.

---

## Step 2 — Run the workflow

Launch the Workflow tool with the script below and `args` = the doc list from Step 1.
(Set `PY` in the script to your distillPDF-enabled Python first.)

**Critical adaptation vs the original run:** distillPDF's `to_html()` now **writes a file by
default** and returns `1`. To capture the HTML string you MUST pass `return_string=True` —
the eval command below already does.

```javascript
export const meta = {
  name: 'sonnet-html-review',
  description: 'Sonnet second-opinion review of distillpdf to_html() fidelity across a corpus',
  phases: [
    { title: 'Evaluate', detail: 'one Sonnet agent per PDF: render with distillpdf, read source PDF, compare', model: 'sonnet' },
    { title: 'Synthesize', detail: 'aggregate findings into a prioritized worklist' },
  ],
}

// A Python interpreter that can `import distillpdf` (current wheel installed).
const PY = '/opt/miniconda3/bin/python3'

const FINDINGS_SCHEMA = {
  type: 'object',
  properties: {
    name: { type: 'string' },
    doc_type: { type: 'string', description: 'e.g. two-column arXiv paper, tax form, slide deck' },
    overall_quality: { type: 'number', description: '0.0-1.0 fidelity of the HTML vs the source PDF for LLM consumption' },
    strengths: { type: 'array', items: { type: 'string' } },
    issues: {
      type: 'array',
      items: {
        type: 'object',
        properties: {
          area: { type: 'string', enum: ['tables','figures','images','captions','reading_order','math','references','lists','headings','paragraphs','unicode','links','other'] },
          severity: { type: 'string', enum: ['high','medium','low'] },
          problem: { type: 'string', description: 'concrete, specific — cite the actual table/figure/text affected' },
          location: { type: 'string', description: 'where (page, section, table/figure number)' },
          suggestion: { type: 'string', description: 'a concrete idea for how distillpdf could fix it' },
        },
        required: ['area','severity','problem','suggestion'],
      },
    },
  },
  required: ['name','overall_quality','issues'],
}

phase('Evaluate')
// The Workflow `args` (the [{name,pdf},...] list) sometimes arrives as a JSON STRING
// rather than a parsed array — parse defensively or `docs.map` throws "is not a function".
const docs = Array.isArray(args) ? args : JSON.parse(args)
const evals = await parallel(docs.map((d) => () =>
  agent(
    `You are a meticulous document-extraction reviewer giving an independent second opinion on "distillpdf", a pure-Rust library that converts PDFs to AI-ready semantic HTML (thin HTML: <h1-3>, <p>, <table>, <figure>, <img src="data:..."> for rasters, inline <svg> for vector figures, <a> links; NO CSS classes; images embedded as data URIs; vector diagrams transcoded to SVG with <text> labels).

DOCUMENT: ${d.name}
SOURCE PDF: ${d.pdf}

STEP 1 — Generate the HTML fresh from distillpdf:
  Run this exact command (it parses the PDF and writes the HTML):
    ${PY} -c "import distillpdf; open('/tmp/eval_${d.name}.html','w').write(distillpdf.Pdf.open('${d.pdf}').to_html(return_string=True))"
  (return_string=True is required — without it to_html() writes a file and returns 1.)
  If it errors, report that as a HIGH severity 'other' issue and stop.

STEP 2 — Read the generated HTML at /tmp/eval_${d.name}.html (it may be large; data: URIs and <svg> path data are expected to be long — skim those, focus on structure and text).

STEP 3 — Read the SOURCE PDF ${d.pdf} VISUALLY using the Read tool with the pages parameter (read the first 6-8 pages, prioritising any pages with tables, figures, equations, or multi-column layout). This is your ground truth — what the document ACTUALLY contains and how it is laid out.

STEP 4 — Compare the HTML against the source and judge FIDELITY FOR AN LLM READER (not pixel-perfect visual match). Evaluate every applicable dimension:
  - tables: are they <table> with correct rows/columns? cells split/merged wrongly? columns dropped? a full-width table split in half? a real table missing or rendered as prose/figure? header row correct?
  - figures/images: is each figure present as <img> or <svg>? vector diagrams transcoded with their text labels? anything missing, mis-placed, clipped, or wrongly black/blank? raster images embedded?
  - captions: present as <caption>/<figcaption>? correct figure/table number? placed on the right side? any in-text "see Table N" reference wrongly turned into a caption (or vice versa)?
  - reading_order: two-column pages read in correct order (not interleaved across the gutter)? paragraphs flow correctly across column/page breaks?
  - math: equations / inline math / Greek letters / symbols preserved or garbled/dropped?
  - references: bibliography entries intact and separable? citation links resolve?
  - unicode/ligatures: any mojibake, control chars, dropped accents, broken ligatures (ﬁ, ﬂ)?
  - lists, headings, paragraphs, links: correct structure?

Be CONCRETE and specific — name the actual table/figure/section. Distinguish genuine extraction defects from things no text-based extractor can recover (e.g. text baked into a raster). Note real STRENGTHS too.

Return findings via the structured output tool. overall_quality is your honest 0-1 fidelity score. Do NOT edit any code. Keep each issue's suggestion actionable for the distillpdf authors.`,
    { label: `eval:${d.name}`, phase: 'Evaluate', model: 'sonnet', schema: FINDINGS_SCHEMA }
  ).then((r) => ({ ...r, name: r?.name || d.name })).catch(() => null)
))

const good = evals.filter(Boolean)
log(`collected ${good.length}/${docs.length} document reviews`)

phase('Synthesize')
const SYNTH_SCHEMA = {
  type: 'object',
  properties: {
    corpus_quality_mean: { type: 'number' },
    themes: {
      type: 'array',
      description: 'cross-document issue themes, ordered by priority (highest impact first)',
      items: {
        type: 'object',
        properties: {
          title: { type: 'string' },
          area: { type: 'string' },
          severity: { type: 'string', enum: ['high','medium','low'] },
          affected_docs: { type: 'array', items: { type: 'string' } },
          frequency: { type: 'number', description: 'number of documents affected' },
          impact: { type: 'string', description: 'why it matters for an LLM reader' },
          recommendation: { type: 'string', description: 'concrete engineering approach to fix' },
          effort: { type: 'string', enum: ['small','medium','large'] },
        },
        required: ['title','severity','affected_docs','frequency','recommendation','effort'],
      },
    },
    quick_wins: { type: 'array', items: { type: 'string' }, description: 'high-value low-effort fixes' },
    notable_strengths: { type: 'array', items: { type: 'string' } },
    per_doc_quality: {
      type: 'array',
      items: { type: 'object', properties: { name: { type: 'string' }, quality: { type: 'number' } }, required: ['name','quality'] },
    },
  },
  required: ['themes','quick_wins','per_doc_quality'],
}

const synthesis = await agent(
  `You are the orchestrator synthesising ${good.length} independent Sonnet reviews of distillpdf's PDF->HTML output across a corpus. Each review is a JSON object with name, doc_type, overall_quality, strengths, and issues[] (area, severity, problem, location, suggestion).

Produce a PRIORITIZED, DEDUPLICATED worklist for the distillpdf authors:
- Cluster the individual issues into cross-document THEMES (e.g. "tables lose columns", "vector figures clipped", "math symbols dropped"). For each theme: list affected docs, frequency, severity, why it matters for an LLM reader, a concrete engineering recommendation, and a rough effort estimate.
- Order themes by impact = frequency x severity (highest first).
- Surface QUICK WINS (high value, low effort) separately.
- Note genuine strengths worth preserving.
- Include each document's quality score.
Do not invent issues not present in the reviews. Be specific and actionable.

REVIEWS (JSON array):
${JSON.stringify(good)}`,
  { label: 'synthesize', phase: 'Synthesize', schema: SYNTH_SCHEMA }
)

return { reviews: good, synthesis }
```

How it works:
- **`parallel(...)`** fans out one agent per PDF (capped at ~10–16 concurrent; the rest
  queue). Each returns a validated `FINDINGS_SCHEMA` object or `null` (filtered out).
- Each eval agent is forced onto **`model: 'sonnet'`** — that's the whole point (an
  independent model giving a second opinion, not the same model grading itself).
- The **synthesize** agent (orchestrator's default model) clusters all findings into the
  `SYNTH_SCHEMA` worklist.
- The Workflow returns `{ reviews, synthesis }`.

---

## Step 3 — Write the report

After the workflow returns, render `synthesis` into a human-readable, **tiered** worklist and
save it. Convention (matches the first run):

- **Path:** `benchmarking/SONNET_REVIEW.md` (or `bench/secondopinion/REVIEW.md`).
- **Structure:** header with corpus mean + lowest/highest docs and the run id → a one-line
  "second opinion, verify before acting" caveat → **Tier 0 (catastrophic + cheap)** →
  **Tier 1 (high impact, pervasive)** → **Tier 2 (medium)** → **Tier 3 (polish)** →
  **Notable strengths (preserve)** → **Per-document quality (low → high)**.
- For each theme line, keep: `(area, frequency, severity)`, the concrete fix, and an effort
  tag. **Spot-check** the top 1–2 "catastrophic + cheap" claims by reproducing them, and mark
  which you VERIFIED — a second opinion can be wrong; don't chase phantoms.

---

## Output → ROADMAP.md

This file is the **runbook for running** the review. The *findings* are not kept here:

- The raw, full report of a run goes to `benchmarking/SONNET_REVIEW.md` (prior runs archived
  as `SONNET_REVIEW_<date>.md`).
- The living, prioritized **fix list with status + per-item test suggestions** is
  **`ROADMAP.md`** at the repo root. After each run, fold confirmed themes into ROADMAP.md
  (don't duplicate the worklist back into this runbook).
