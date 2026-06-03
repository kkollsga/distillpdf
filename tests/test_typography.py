"""Typography on an owned fixture (typography.pdf): accented Latin characters survive
intact, hyphenation across a line break is rejoined, no control characters leak, and
sub/superscripts (positioned via the PDF text-rise operator) are surfaced as <sup>/<sub>."""
from _fixtures import GT, html, text

NAME = "typography.pdf"
G = GT[NAME]


def test_accents_intact():
    t = text(html(NAME))
    for word in G["accents"]:
        assert word in t, f"accented word corrupted or lost: {word!r}"


def test_dehyphenation():
    t = text(html(NAME))
    assert G["dehyphenated"] in t, f"hyphenated word not rejoined: {G['dehyphenated']!r}"
    assert G["not_present"] not in t, f"stray hyphen+space survived: {G['not_present']!r}"


def test_no_control_characters():
    h = html(NAME)
    ctrl = [c for c in h if ord(c) < 0x20 and c not in "\t\n\r"]
    assert not ctrl, f"control characters leaked: {[hex(ord(c)) for c in ctrl[:5]]}"


def test_superscript_and_subscript():
    h = html(NAME)
    assert "<sup>" in h, "superscript not surfaced"
    assert "<sub>" in h, "subscript not surfaced"
