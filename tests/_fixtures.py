"""Shared fixture access for the owned CI tests."""
import json
import os
import re

import distillpdf

HERE = os.path.dirname(os.path.abspath(__file__))
FIX = os.path.join(HERE, "fixtures_pdf")
with open(os.path.join(FIX, "groundtruth.json")) as _f:
    GT = json.load(_f)


def html(name):
    # Page mode: these fixture checks predate the section-mode default and assert the
    # per-page structure. Section mode has its own coverage in test_section_mode.py.
    return distillpdf.Pdf.open(os.path.join(FIX, name)).to_html(mode="page")


def doc(name):
    return distillpdf.Pdf.open(os.path.join(FIX, name))


def text(html):
    """Visible text (SVG dropped), entities decoded, whitespace collapsed."""
    s = re.sub(r"<svg\b.*?</svg>", " ", html, flags=re.DOTALL)
    s = re.sub(r"<[^>]+>", " ", s)
    for a, b in (("&amp;", "&"), ("&lt;", "<"), ("&gt;", ">"), ("&quot;", '"'), ("&#39;", "'")):
        s = s.replace(a, b)
    return re.sub(r"\s+", " ", s).strip()


def cells(html, tag):
    return [re.sub(r"\s+", " ", re.sub("<[^>]+>", "", c)).strip()
            for c in re.findall(rf"<{tag}\b[^>]*>(.*?)</{tag}>", html, re.DOTALL)]
