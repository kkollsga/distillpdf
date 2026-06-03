"""Page-bottom footnotes (footnotes.pdf): a block of smaller-type footnote lines at the
bottom of the page is gathered into one <aside>, each footnote split on its marker, and
not left as loose body paragraphs."""
import re

from _fixtures import GT, html, text

NAME = "footnotes.pdf"
G = GT[NAME]


def _aside(h):
    m = re.search(r"<aside>(.*?)</aside>", h, re.DOTALL)
    return m.group(1) if m else None


def test_footnotes_in_aside():
    h = html(NAME)
    aside = _aside(h)
    assert aside is not None, "footnote block not wrapped in <aside>"
    at = text(aside)
    for snip in G["foot_snippets"]:
        assert snip in at, f"footnote missing from <aside>: {snip!r}"


def test_footnotes_not_loose_body():
    """The footnote text must live in the <aside>, not leak into body paragraphs."""
    h = html(NAME)
    body = text(re.sub(r"<aside>.*?</aside>", "", h, flags=re.DOTALL))
    for snip in G["foot_snippets"]:
        assert snip not in body, f"footnote leaked into body prose: {snip!r}"


def test_footnotes_split_per_marker():
    h = html(NAME)
    aside = _aside(h) or ""
    items = [re.sub(r"<[^>]+>", "", p).strip() for p in re.findall(r"<p>(.*?)</p>", aside, re.DOTALL)]
    assert len(items) >= 3, f"footnotes not split into separate items: {items}"
    # each footnote starts with its split-off marker ("1.", "2.", "3.")
    assert items[0].startswith("1."), f"first footnote marker not split: {items[0]!r}"
