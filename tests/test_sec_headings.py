"""Locks for the global heading pre-detection pass + SEC anchors (src/html.rs:
plan_headings, sec_anchor). On sec_structure.pdf:
  - "PART I" / "Item 1. Business" / "Item 1A. Risk Factors" are recognised as headings
    (Reliable SEC anchors) even though their styling matches non-heading lines;
  - a genuine larger-style section ("Financial Overview") stays a heading;
  - a >90-line flood of one shared bold line-item style is DISTRUSTED → none becomes a
    heading (the SEC <h4>-flood failure mode).
Each assertion would fail on the pre-Stage-2 baseline (the flood was emitted as headings;
Item/PART were mis-levelled style matches).
"""
import re

from _fixtures import GT, html, text

NAME = "sec_structure.pdf"
G = GT[NAME]


def _headings(h):
    return [(int(m.group(1)), re.sub(r"\s+", " ", re.sub(r"<[^>]+>", "", m.group(2))).strip())
            for m in re.finditer(r"<h([1-6])\b[^>]*>(.*?)</h[1-6]>", h, re.DOTALL)]


def test_sec_anchors_are_headings():
    h = html(NAME)
    hs = [t for _, t in _headings(h)]
    for a in G["anchors"]:
        assert any(a in t for t in hs), f"SEC anchor not a heading: {a!r} (headings={hs[:8]}…)"


def test_part_outranks_item_level():
    """PART is a section (h2); Item is a subsection (h3) — distinct, non-collapsed levels."""
    lv = {t: l for l, t in _headings(html(NAME))}
    part = next((l for t, l in lv.items() if t.startswith("PART I")), None)
    item = next((l for t, l in lv.items() if t.startswith("Item 1.")), None)
    assert part is not None and item is not None, f"missing PART/Item levels: {lv}"
    assert part < item, f"PART (h{part}) should outrank Item (h{item})"


def test_genuine_section_kept():
    hs = [t for _, t in _headings(html(NAME))]
    assert any("Financial Overview" in t for t in hs), f"genuine section dropped: {hs[:8]}…"


def test_lineitem_flood_distrusted():
    """No line-item label from the >90-line over-used style is emitted as a heading, yet
    the text survives in the body."""
    h = html(NAME)
    hs = [t for _, t in _headings(h)]
    body = text(h)
    for lab in G["flood"]:
        assert not any(lab == t for t in hs), f"line-item flood promoted to heading: {lab!r}"
    assert "Net product sales" in body, "flood text lost from body"
