"""A form/grid page whose non-heading lines all wear a heading's face (form_grid_prose.pdf).

Five traps, all bold and short — a URL sentence cut mid-clause, a table caption, a grid
sub-label in the band just above a table's top rule, a dot-leader form line, and a bulleted
line. Each of them was emitted as a section heading (or, for the first two, as the document
`<h1>`); none of them is one. Two real headings in the SAME bold face are asserted alongside,
so a fix that works by suppressing the face fails here.

The traps' text must also survive: every rule below changes how a line is CLASSIFIED, never
whether it is emitted.
"""
import re

from _fixtures import GT, cells, html, text

NAME = "form_grid_prose.pdf"
G = GT[NAME]


def _headings(h):
    return [(int(m.group(1)), re.sub(r"\s+", " ", re.sub(r"<[^>]+>", "", m.group(2))).strip())
            for m in re.finditer(r"<h([1-6])\b[^>]*>(.*?)</h\1>", h, re.DOTALL)]


def test_title_falls_through_the_traps_to_the_real_heading():
    """The two most prominent lines on the page are a URL sentence and a caption. Neither is
    a title, so the document `<h1>` is the first real heading below them."""
    hs = _headings(html(NAME))
    h1 = [t for lvl, t in hs if lvl == 1]
    assert h1 == [G["title"]], f"document <h1> is {h1}, expected [{G['title']!r}]"


def test_real_headings_in_the_same_face_survive():
    hs = [t for _, t in _headings(html(NAME))]
    for want in G["headings"]:
        assert any(want in t for t in hs), f"real heading lost: {want!r} (got {hs})"


def test_no_trap_is_promoted_to_a_heading():
    hs = [t for _, t in _headings(html(NAME))]
    for trap in G["not_headings"]:
        assert not any(trap[:28] in t for t in hs), f"{trap!r} promoted to a heading: {hs}"


def test_no_trap_text_is_lost():
    body = text(html(NAME))
    for trap in G["not_headings"]:
        assert trap[:28] in body, f"{trap!r} dropped from the output entirely"


def test_the_bulleted_trap_becomes_a_list_item():
    h = html(NAME)
    assert "<ul" in h, "the bulleted line did not become a list"
    assert any("Mineral and energy resources" in c for c in cells(h, "li"))


def test_one_table():
    assert html(NAME).count("<table") == G["tables"]
