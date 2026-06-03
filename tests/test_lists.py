"""List detection on an owned fixture (lists.pdf): star/dash bullets become <ul><li>,
numbered and colon-introduced items become <ol><li>, a wrapped item stays one <li>,
markers are stripped, and no list is fragmented into single-item lists."""
import re

from _fixtures import GT, html, cells, text

NAME = "lists.pdf"
G = GT[NAME]


def _lists(h, kind):
    return re.findall(rf"<{kind}\b[^>]*>.*?</{kind}>", h, re.DOTALL)


def test_ordered_items_in_ol():
    h = html(NAME)
    li = cells(h, "li")
    for item in G["ordered_items"]:
        assert any(item in c for c in li), f"ordered item not in a <li>: {item!r}"
    # numbered + colon-introduced items live in <ol>
    ol_items = [c for ol in _lists(h, "ol") for c in cells(ol, "li")]
    for item in G["numbered"] + G["colon"]:
        assert any(item in c for c in ol_items), f"item not in <ol>: {item!r}"


def test_unordered_items_in_ul():
    h = html(NAME)
    ul_items = [c for ul in _lists(h, "ul") for c in cells(ul, "li")]
    for item in G["unordered_items"]:
        assert any(item in c for c in ul_items), f"bullet item not in <ul>: {item!r}"


def test_markers_stripped():
    li = cells(html(NAME), "li")
    for c in li:
        assert not re.match(r"^\s*([*–−·•]|\d+[.)])\s", c), \
            f"list marker leaked into <li>: {c!r}"


def test_wrapped_item_merged():
    """A bullet item that wraps to a second line stays a single <li> (continuation
    folded in), not split into a list item plus an orphan paragraph."""
    li = cells(html(NAME), "li")
    assert any(G["wrapped_snippet"] in c for c in li), \
        "wrapped list-item continuation not folded into its <li>"
    # and the wrapped item lives in the (multi-item) unordered list
    ul_items = [c for ul in _lists(html(NAME), "ul") for c in cells(ul, "li")]
    assert any(G["wrapped_snippet"] in c for c in ul_items), "wrapped item not in the <ul>"


def test_two_lists_separated_by_paragraph():
    """Regression: a real paragraph between two numbered lists stays its own <p> and is
    NOT absorbed into the first list's last <li>; both lists remain separate and intact."""
    g = GT["twolists.pdf"]
    h = html("twolists.pdf")
    li = cells(h, "li")
    assert not any(g["middle_snippet"] in c for c in li), \
        "intervening paragraph swallowed into a <li>"
    assert g["middle_snippet"] in text(h), "intervening paragraph lost"
    for item in g["first"] + g["second"]:
        assert any(item in c for c in li), f"list item lost: {item!r}"
    assert len(_lists(h, "ol")) >= 2, "the two lists were merged into one"
