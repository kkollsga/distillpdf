"""Locks for math glyph recovery (src/text.rs: cmmi_code/cmsy_code, glyph_to_str math
names, c1_remap). The hand-authored mathfonts.pdf shows the same glyphs four ways:

  - CMSY10 with no encoding/ToUnicode  → ≤ ≥ → ∈  (cmsy_code byte table)
  - CMMI10 with no encoding/ToUnicode  → α μ λ ∂  (cmmi_code byte table)
  - PazoMath /Differences glyph names  → ∑ ∫ α ≤  (glyph_to_str math names)
  - Helvetica raw Windows-1252 C1 bytes → — ’      (c1_remap)

On the pre-change baseline every one of these was dropped (control chars / unmapped).
"""
from _fixtures import GT, html, text

NAME = "mathfonts.pdf"
G = GT[NAME]


def _alltext():
    # extracted text + rendered HTML, so the glyphs are checked end to end
    import distillpdf
    import os
    from _fixtures import FIX
    raw = distillpdf.Pdf.open(os.path.join(FIX, NAME)).extract_text()
    return raw + "\n" + text(html(NAME))


def test_cmsy_operators_recovered():
    blob = _alltext()
    for ch in G["cmsy"]:
        assert ch in blob, f"CMSY operator {ch!r} dropped (cmsy_code)"


def test_cmmi_greek_recovered():
    blob = _alltext()
    for ch in G["cmmi"]:
        assert ch in blob, f"CMMI glyph {ch!r} dropped (cmmi_code)"


def test_differences_math_names_recovered():
    blob = _alltext()
    for ch in G["diff_names"]:
        assert ch in blob, f"/Differences math glyph {ch!r} dropped (glyph_to_str)"


def test_c1_bytes_remapped():
    blob = _alltext()
    for ch in G["c1"]:
        assert ch in blob, f"Windows-1252 C1 byte not remapped to {ch!r} (c1_remap)"
