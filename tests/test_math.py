"""Math discrimination on an owned fixture (math.pdf): a display equation stays in the
text flow (never a spurious <table>, never a heading), a 'Theorem 1.' lead is not
promoted to a standalone heading, and the surrounding section structure is intact."""
import re

from _fixtures import GT, html, text

NAME = "math.pdf"
G = GT[NAME]


def test_title_and_section():
    h = html(NAME)
    assert re.search(r"<h1\b[^>]*>\s*" + re.escape(G["title"]), h), "title not <h1>"
    assert re.search(r"<h2\b[^>]*>(?:\s*<b>)?\s*" + re.escape(G["section"]), h), \
        "numbered section not <h2>"


def test_equation_not_a_table():
    assert "<table" not in html(NAME), "display equation mis-detected as a <table>"


def test_equation_not_a_heading():
    h = html(NAME)
    heads = [re.sub(r"<[^>]+>", "", m) for m in re.findall(r"<h[1-6]\b[^>]*>.*?</h[1-6]>", h, re.DOTALL)]
    assert not any(G["equation_fragment"] in t for t in heads), "equation promoted to a heading"
    assert G["equation_fragment"] in text(h), "equation text lost"


def test_theorem_lead_not_a_heading():
    h = html(NAME)
    heads = [re.sub(r"<[^>]+>", "", m) for m in re.findall(r"<h[1-6]\b[^>]*>.*?</h[1-6]>", h, re.DOTALL)]
    assert not any(t.strip().startswith("Theorem 1") for t in heads), \
        "'Theorem 1.' promoted to a standalone heading"
    assert G["theorem_text"] in text(h), "theorem statement text lost"
