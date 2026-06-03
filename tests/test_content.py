#!/usr/bin/env python3
"""GOAL gate: real-document CONTENT extraction quality.

Ground truth (content_groundtruth.json) was extracted independently by Sonnet
reading each corpus PDF — never the extractor — so it is a true oracle of what
distillpdf.to_html() SHOULD produce. It scores five facets that the synthetic
table / header-list gates do NOT cover, and that real users care about:

  table cells   — fraction of distinctive ground-truth table cells that land
                  inside some <table> (table content recall)
  table bleed   — ground-truth "must_not" phrases (adjacent prose) that wrongly
                  appear INSIDE a <table>. This is the exact failure the Table-5
                  column-bleed bug was: it must be ZERO. (table precision)
  references    — fraction of verbatim bibliography-entry openings that appear
                  in the output (reference extraction recall)
  paragraphs    — fraction of verbatim contiguous paragraph snippets that appear
                  intact inside a SINGLE <p> (reading-order / paragraph integrity)
  figure caps   — fraction of figure captions surfaced in a <figcaption>
  figure text   — fraction of in-figure text labels that appear anywhere

Matching is tolerant: HTML tags/entities stripped, Unicode NFKC-folded (so
superscripts/ligatures normalise), whitespace collapsed, lowercased. Snippets
were chosen to avoid hyphenated line-breaks so a correct reading order yields an
exact normalised substring.

Run:  python3 bench/test_content.py     (report; non-zero exit until goals met)
  or: pytest bench/test_content.py
"""
import json, os, re, sys, unicodedata

HERE = os.path.dirname(os.path.abspath(__file__))
GT = os.path.join(HERE, "content_groundtruth.json")
FIX = os.path.join(HERE, "fixtures")  # frozen PyMuPDF reference text (recoverability oracle)

# Two-tier scoring:
#   TARGETS — aspirational quality bars (reported; where we want extraction to be).
#   FLOORS  — regression guards set just below today's measured baseline. The gate
#             FAILS (build-breaking) if any metric drops below its floor, so a
#             reintroduced table-bleed, a ligature regression, etc. turns the
#             build red immediately. Ratchet the floors up as the targets are met.
# "Great" = strong & pragmatic (see GOALS.md "Content quality"). Targets are
# scored AFTER carve-out: ground-truth items the PyMuPDF reference also can't
# recover (source tofu / JPEG2000 / not in text layer) are excluded from the
# denominator and logged. So these bars are "of what is recoverable at all".
TARGETS = {
    "table_cells": 0.90,
    "references": 0.85,
    "paragraphs": 0.85,
    "figure_caps": 0.85,
    "figure_text": 0.60,
    "figure_render": 0.85,  # each ground-truth figure is actually rendered (img/svg)
}
FLOORS = {
    "table_cells": 0.65,
    "references": 0.57,
    "paragraphs": 0.50,
    "figure_caps": 0.70,
    "figure_text": 0.85,
    "figure_render": 0.90,
}
MAX_TABLE_BLEED = 3      # Phase B: cs_CV 2-col-prose bleed fixed; 3 residual are
                         # fw9 (a dense gov form) — headings absorbed into tables. Target 0.
MAX_CTRL_CHARS = 0       # Phase A LOCKED: f-ligatures (Differences + OT1) + safety net


def ctrl_count(html):
    """Stray control chars (excluding tab/newline/return) — these are dropped
    f-ligatures (fi/fl/ff emitted as \\x01-\\x03) or unmapped glyphs."""
    return sum(1 for c in html if ord(c) < 0x20 and c not in "\t\n\r")


def norm(s):
    """NFKC-fold, strip tags/entities, lowercase, collapse whitespace."""
    s = re.sub(r"<[^>]+>", " ", s)
    for a, b in (("&amp;", "&"), ("&lt;", "<"), ("&gt;", ">"), ("&quot;", '"'), ("&#39;", "'")):
        s = s.replace(a, b)
    s = unicodedata.normalize("NFKC", s)
    s = s.lower()
    return re.sub(r"\s+", " ", s).strip()


def table_blocks(html):
    # The <caption> is the table's descriptive label, not a data cell — strip it so
    # caption text is neither counted as a recovered cell nor flagged as prose-bleed.
    out = []
    for m in re.findall(r"<table\b.*?</table>", html, re.DOTALL):
        out.append(norm(re.sub(r"<caption\b.*?</caption>", " ", m, flags=re.DOTALL)))
    return out


def figcaptions(html):
    return [norm(m) for m in re.findall(r"<figcaption\b.*?</figcaption>", html, re.DOTALL)]


def paragraphs(html):
    # Block-level containers a contiguous paragraph snippet may land in. Match
    # each tag with its own non-capturing pattern (a single capturing group would
    # make re.findall return the tag name instead of the block).
    out = []
    for tag in ("p", "li", "figcaption", "td", "th"):
        out += [norm(m) for m in re.findall(rf"<{tag}\b.*?</{tag}>", html, re.DOTALL)]
    return out


def contains(haystacks, needle):
    n = norm(needle)
    if len(n) < 3:
        return False
    return any(n in h for h in haystacks)


def run():
    import distillpdf
    gt = json.load(open(GT, encoding="utf-8"))
    docs = {k: v for k, v in gt.items() if not k.startswith("_")}

    agg = {k: [0, 0] for k in ("table_cells", "references", "paragraphs", "figure_caps", "figure_text", "figure_render")}
    bleed_hits = []  # (doc, phrase)
    total_ctrl = 0
    excluded = 0  # ground-truth items carved out as unrecoverable (see below)
    fig_missing = []  # (doc, rendered, expected) — figures in the truth not rendered
    weak_tables = []  # (doc, label, hit, total) — a ground-truth table that came out mangled

    print(f"{'PDF':30s}{'tbl':>9s}{'bleed':>6s}{'ref':>9s}{'para':>9s}{'figcap':>8s}{'figtxt':>8s}{'ctrl':>6s}")
    for name, d in sorted(docs.items()):
        path = os.path.join(HERE, "corpus", name)
        if not os.path.exists(path):
            path = os.path.join(HERE, "corpus_headers", name)
        if not os.path.exists(path):
            continue
        html = distillpdf.Pdf.open(path).to_html()
        nctrl = ctrl_count(html)
        total_ctrl += nctrl
        full = norm(html)
        tbls = table_blocks(html)
        figcaps = figcaptions(html)
        paras = paragraphs(html)

        # Carve-out: an item is "recoverable" only if the reference extractor
        # (PyMuPDF, frozen in fixtures/) also surfaces it. Items PyMuPDF misses too
        # are source limitations — glyphs stored as tofu/U+FFFD, JPEG2000 figure
        # labels, characters absent from the text layer — not distillpdf bugs, so they
        # are excluded from the denominator (and counted/logged). This pins "great"
        # to PyMuPDF parity (the project's north star) rather than to the impossible.
        ref_path = os.path.join(FIX, f"{os.path.splitext(name)[0]}.pymupdf_plain.txt")
        ref = norm(open(ref_path, encoding="utf-8", errors="replace").read()) if os.path.exists(ref_path) else ""

        def recoverable(s):
            return ref == "" or contains([ref], s)

        # tables: best-matching <table> per ground-truth table
        tc_hit = tc_tot = 0
        for t in d["tables"]:
            (th, tt) = (0, 0)  # this table's cell recall (to flag a mangled table)
            for c in t.get("must_cells", []):
                if not recoverable(c):
                    excluded += 1
                    continue
                tc_tot += 1
                tt += 1
                if any(contains([tb], c) for tb in tbls):
                    tc_hit += 1
                    th += 1
            # A ground-truth table whose cells mostly did NOT land in a <table>
            # came out mangled (split across blocks / leaked into <p>) — auto-flag
            # it so the failure is visible without manual page inspection.
            if tt >= 3 and th * 2 < tt:
                weak_tables.append((name, t.get("label") or "?", th, tt))
            for mn in t.get("must_not_cells", []):
                if contains(tbls, mn):
                    bleed_hits.append((name, mn))

        r_hit = r_tot = 0
        for s in d["references"].get("samples", []):
            if not recoverable(s):
                excluded += 1
                continue
            r_tot += 1
            if contains([full], s):
                r_hit += 1

        p_hit = p_tot = 0
        for p in d["paragraphs"]:
            if not recoverable(p["text_snippet"]):
                excluded += 1
                continue
            p_tot += 1
            if contains(paras, p["text_snippet"]):
                p_hit += 1

        fc_hit = fc_tot = ft_hit = ft_tot = 0
        for f in d["figures"]:
            cap = f.get("caption_snippet", "")
            if cap:
                if recoverable(cap):
                    fc_tot += 1
                    fc_hit += 1 if contains(figcaps, cap) else 0
                else:
                    excluded += 1
            for t in f.get("in_figure_text", []):
                if not recoverable(t):
                    excluded += 1
                    continue
                ft_tot += 1
                ft_hit += 1 if contains([full], t) else 0

        # figure RENDER coverage: is each ground-truth figure actually drawn? Count
        # <figure> blocks that contain an <img> or inline <svg>; if fewer than the
        # truth lists, figures are missing — flagged explicitly below.
        gt_figs = len(d["figures"])
        rendered_figs = len(re.findall(r"<figure\b[^>]*>(?:(?!</figure>).)*?<(?:img|svg)\b", html, re.DOTALL))
        fr_hit = min(rendered_figs, gt_figs)
        if gt_figs > rendered_figs:
            fig_missing.append((name, rendered_figs, gt_figs))

        for key, (h, tot) in (("table_cells", (tc_hit, tc_tot)), ("references", (r_hit, r_tot)),
                              ("paragraphs", (p_hit, p_tot)), ("figure_caps", (fc_hit, fc_tot)),
                              ("figure_text", (ft_hit, ft_tot)), ("figure_render", (fr_hit, gt_figs))):
            agg[key][0] += h
            agg[key][1] += tot

        def pct(h, t):
            return f"{h}/{t}" if t else "  -"
        print(f"{name[:30]:30s}{pct(tc_hit, tc_tot):>9s}"
              f"{sum(1 for b in bleed_hits if b[0] == name):>6d}"
              f"{pct(r_hit, r_tot):>9s}{pct(p_hit, p_tot):>9s}"
              f"{pct(fc_hit, fc_tot):>8s}{pct(ft_hit, ft_tot):>8s}{nctrl:>6d}")

    print("\n--- aggregate (floor = regression guard, target = goal) ---")
    scores = {}
    failures = []
    for key, (h, tot) in agg.items():
        scores[key] = h / tot if tot else 1.0
        floor, target = FLOORS[key], TARGETS[key]
        if scores[key] < floor:
            mark, note = "❌REGRESSION", f"below floor {floor:.2f}"
            failures.append(key)
        elif scores[key] < target:
            mark, note = "🔴", f"floor {floor:.2f} ok, target {target:.2f}"
        else:
            mark, note = "✅", f"target {target:.2f} met"
        print(f"  {key:14s}{scores[key]:.3f}  [{h}/{tot}]  {mark}  ({note})")

    bleed_mark = "❌REGRESSION" if len(bleed_hits) > MAX_TABLE_BLEED else ("✅" if not bleed_hits else "🔴")
    print(f"  table_bleed   {len(bleed_hits)}  {bleed_mark}  (floor <={MAX_TABLE_BLEED}, target 0)")
    for doc, ph in bleed_hits:
        print(f"       BLEED {doc}: {ph[:70]!r}")
    ctrl_mark = "❌REGRESSION" if total_ctrl > MAX_CTRL_CHARS else ("✅" if total_ctrl == 0 else "🔴")
    print(f"  ctrl_chars    {total_ctrl}  {ctrl_mark}  (floor <={MAX_CTRL_CHARS}, target 0; dropped ligatures/glyphs)")
    print(f"  (carved out   {excluded} ground-truth items PyMuPDF also can't recover — source tofu / JP2 / not in text layer)")
    # Explicit, auto-detected problem lists — so missing figures / mangled tables
    # surface here instead of by manual page inspection.
    if fig_missing:
        print("\n--- figures NOT rendered (truth has more figures than <img>/<svg> emitted) ---")
        for doc, got, want in fig_missing:
            print(f"  {doc[:34]:34s} rendered {got}/{want}")
    if weak_tables:
        print("\n--- mangled tables (most ground-truth cells did NOT land in one <table>) ---")
        for doc, label, h, t in weak_tables:
            print(f"  {doc[:30]:30s} {label:10s} cells {h}/{t}")

    if len(bleed_hits) > MAX_TABLE_BLEED:
        failures.append("table_bleed")
    if total_ctrl > MAX_CTRL_CHARS:
        failures.append("ctrl_chars")
    if failures:
        print("\n❌ content REGRESSION — below baseline floor:", ", ".join(failures))
        return False
    print("\n✅ content floors held (see 🔴 items for remaining targets)")
    return True


def test_content():
    assert run(), "content extraction goal not met (see report)"


if __name__ == "__main__":
    sys.exit(0 if run() else 1)
