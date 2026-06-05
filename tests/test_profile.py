#!/usr/bin/env python3
"""Document style-profile heading detection (fixtures_pdf/profile_heads.pdf).

Guards the profile-driven path: section headers set in a distinct, non-bold font only
slightly larger than body (≈1.14×, below the old fixed 1.18× ratio gate and not bold) are
recognised from the document's size·font cluster and promoted to <h2>. Regenerate with
`python tests/gen_fixtures.py`.
"""
import json
import os
import re

import distillpdf
import _htmlcheck as hc

HERE = os.path.dirname(os.path.abspath(__file__))
FIX = os.path.join(HERE, "fixtures_pdf")
PDF = os.path.join(FIX, "profile_heads.pdf")
with open(os.path.join(FIX, "groundtruth.json")) as _f:
    GT = json.load(_f)["profile_heads.pdf"]


def _heads(html):
    return [(t, re.sub(r"<[^>]+>", "", x).strip()) for t, x in re.findall(r"<(h[1-6])[^>]*>(.*?)</\1>", html, re.DOTALL)]


def test_distinct_font_heads_become_h2():
    heads = _heads(distillpdf.Pdf.open(PDF).to_html(mode="section", return_string=True))
    texts = {t for _, t in heads}
    for want in GT["section_heads"]:
        assert want in texts, f"{want!r} not detected as a heading; got {heads}"
    # each section header is a section-level <h2> (the title is the only <h1>)
    by_text = {t: tag for tag, t in heads}
    for want in GT["section_heads"]:
        assert by_text[want] == "h2", f"{want!r} is {by_text[want]}, expected h2"


def test_title_is_single_h1():
    html = distillpdf.Pdf.open(PDF).to_html(mode="section", return_string=True)
    h1 = [t for tag, t in _heads(html) if tag == "h1"]
    assert h1 == [GT["title"]], h1


def test_section_bodies_stay_paragraphs():
    html = distillpdf.Pdf.open(PDF).to_html(mode="section", return_string=True)
    assert GT["body_snippet"] in hc.plain(html)
    # the heading text must not be swallowed into a paragraph
    assert GT["section_heads"][0] not in [
        re.sub(r"<[^>]+>", "", p).strip() for p in re.findall(r"<p>(.*?)</p>", html, re.DOTALL)
    ]


def test_profile_heads_well_formed():
    for mode in ("section", "page"):
        ok, errs = hc.well_formed(distillpdf.Pdf.open(PDF).to_html(mode=mode, return_string=True))
        assert ok, errs
