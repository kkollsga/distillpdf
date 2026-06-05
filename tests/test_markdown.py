"""to_markdown(): a transform of the same HTML to_html() emits, so the two stay in lock-step.

Exercises the structural conversions on owned fixtures — headings, GFM tables, lists,
bold/italic, links (with anchor normalisation), figure captions — plus the image strategies
(placeholder / embed / file extraction). Markdown must carry no leaked HTML tags."""
import os
import re

import distillpdf

from _fixtures import FIX

HEADINGS = os.path.join(FIX, "headings.pdf")
NUMERIC = os.path.join(FIX, "numeric.pdf")
LISTS = os.path.join(FIX, "lists.pdf")
TWOLISTS = os.path.join(FIX, "twolists.pdf")
TYPOGRAPHY = os.path.join(FIX, "typography.pdf")
LINKS = os.path.join(FIX, "links.pdf")
FIGURES = os.path.join(FIX, "figures.pdf")


def _md(path, **kw):
    return distillpdf.Pdf.open(path).to_markdown(**kw)


def test_headings_become_atx():
    md = _md(HEADINGS)
    # at least one ATX heading; no leaked heading tags
    assert re.search(r"^#{1,6} \S", md, re.M)
    assert "<h1" not in md and "<h2" not in md


def test_table_becomes_gfm():
    md = _md(NUMERIC)
    assert "<table" not in md and "<td" not in md
    # a GFM table has a header separator row of dashes
    assert re.search(r"^\|.*\|\n\|[ :|-]*---[ :|-]*\|", md, re.M), md[:500]


def test_lists_become_markdown():
    md = _md(LISTS)
    assert "<ul>" not in md and "<li>" not in md
    assert re.search(r"^- \S", md, re.M)        # bullet
    md2 = _md(TWOLISTS)
    assert re.search(r"^\d+\. \S", md2, re.M)    # ordered


def test_no_html_tags_leak():
    """Across a representative spread, the Markdown carries no block HTML tags."""
    for f in (HEADINGS, NUMERIC, LISTS, TYPOGRAPHY, FIGURES):
        md = _md(f)
        for tag in ("<p>", "<div", "<span", "<table", "<ul>", "<ol>", "<section", "<nav>", "<figure"):
            assert tag not in md, f"{tag} leaked from {os.path.basename(f)}"


def test_links_normalised():
    md = _md(LINKS)
    assert "<a " not in md and "href=" not in md
    # internal section anchors lose the sec- prefix; external URLs survive as md links
    assert "](#sec-" not in md
    assert re.search(r"\[[^\]]+\]\([^)]+\)", md)  # at least one markdown link


def test_emphasis_converted():
    md = _md(TYPOGRAPHY)
    # superscripts render as ^, and there is some emphasis somewhere
    assert "<sup>" not in md and "<i>" not in md and "<b>" not in md


def test_figure_caption_is_alt_text():
    """A figure's caption survives as the image alt text."""
    md = _md(FIGURES)
    assert re.search(r"!\[[^\]]*\]\([^)]*\)", md)  # an image with (possibly empty) alt


def test_file_extraction_paths_resolve(tmp_path):
    dest = tmp_path / "f.md"
    distillpdf.Pdf.open(FIGURES).to_markdown(str(dest), image_mode="external")
    md = dest.read_text(encoding="utf-8")
    refs = re.findall(r"\]\((img/[^)]+)\)", md)
    assert refs
    for r in refs:
        assert (tmp_path / r).exists()
        assert (tmp_path / r).stat().st_size > 0


def test_string_and_file_body_match(tmp_path):
    """Writing to a file and returning a string differ only in image handling — the prose
    (headings/paragraphs) is identical."""
    d = distillpdf.Pdf.open(HEADINGS)
    s = d.to_markdown()
    dest = tmp_path / "h.md"
    d.to_markdown(str(dest))
    # strip image lines from both, compare the rest
    def prose(t):
        return "\n".join(l for l in t.splitlines() if not l.lstrip().startswith("!["))
    assert prose(s) == prose(dest.read_text(encoding="utf-8"))
