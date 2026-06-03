"""Heading detection on an owned fixture (headings.pdf): the document title becomes
<h1>, numbered/canonical sections land at the right level, body/reference traps are
NOT promoted to headings, and toc()/section() expose the structure. Reproduces the
corpus header-extraction guarantees on data we own."""
import re

import distillpdf

from _fixtures import GT, html, text, FIX
import os

NAME = "headings.pdf"
G = GT[NAME]


def _headings(h):
    return [(int(m.group(1)), re.sub(r"\s+", " ", re.sub(r"<[^>]+>", "", m.group(2))).strip())
            for m in re.finditer(r"<h([1-6])\b[^>]*>(.*?)</h\1>", h, re.DOTALL)]


def test_title_is_h1():
    assert re.search(r"<h1\b[^>]*>\s*" + re.escape(G["title"]) + r"\s*</h1>", html(NAME)), \
        "document title not emitted as <h1>"


def test_headings_present_at_correct_level():
    hs = _headings(html(NAME))
    bytext = {t: lvl for lvl, t in hs}
    for spec in G["headings"]:
        want = int(spec["tag"][1])
        # match by exact text or number-stripped title
        match = next((lvl for t, lvl in bytext.items()
                      if spec["text"] in t or t in spec["text"]), None)
        assert match is not None, f"heading missing: {spec['text']!r}"
        assert match == want, f"{spec['text']!r} at h{match}, expected {spec['tag']}"


def test_heading_recall_and_precision():
    hs = _headings(html(NAME))
    htexts = [t for _, t in hs]
    gt = [s["text"] for s in G["headings"]] + [G["title"]]
    found = sum(1 for g in gt if any(g in t or t in g for t in htexts))
    recall = found / len(gt)
    assert recall == 1.0, f"heading recall {recall:.2f} (<1.0): {htexts}"
    # precision: no spurious headings beyond the known ones
    spurious = [t for t in htexts if not any(g in t or t in g for g in gt)]
    assert not spurious, f"spurious headings: {spurious}"


def test_traps_not_promoted():
    """Body sentences / author lists / reference lines must stay paragraphs."""
    hs = [t for _, t in _headings(html(NAME))]
    for trap in G["not_headings"]:
        assert not any(trap[:30] in t for t in hs), f"body text promoted to heading: {trap!r}"
        assert trap[:40] in text(html(NAME)), f"trap text lost: {trap!r}"


def test_toc_lists_headings():
    """The auto-TOC is a lean 3-level nav (h1–h3); it carries the title and the
    section/subsection heads (deeper h4+ are intentionally omitted)."""
    toc = distillpdf.Pdf.open(os.path.join(FIX, NAME)).toc()
    titles = [t for (_lvl, t, _pg, _id) in toc]
    assert any(G["title"] in t for t in titles), "TOC missing the title"
    for spec in G["headings"]:
        if int(spec["tag"][1]) > 3:
            continue  # TOC is 3-level by design
        assert any(spec["text"] in t for t in titles), f"TOC missing: {spec['text']!r}"


def test_section_extraction():
    sec = distillpdf.Pdf.open(os.path.join(FIX, NAME)).section("introduction")
    assert sec, "section('introduction') returned nothing"
    assert "plain paragraph text under a numbered section" in text(sec), \
        "section body wrong or truncated"
