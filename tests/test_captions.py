"""Locks for the caption improvements (src/html.rs: caption_parts/caption_label,
num_id, caption_is_continued, broadened prefixes).

  - Hierarchical numbers preserved whole in the id (fig-2-3-1 / tab-0-1-1), not truncated
    to the leading integer (the NASA databook bug) — and the displayed caption keeps "2.3.1".
  - A figure-less caption still emits inside <figure>, never as a bare <p>.
  - A multi-page "Figure 4.—Continued" marker is suppressed (no duplicate fig-4-2).
  - "Fig 5" (no dot) and Nature "Figure 7 |" are captions; "As shown in Fig 5 …" body is not.

Each assertion would fail on the pre-change baseline (truncated ids; continued duplicates).
"""
import re

from _fixtures import GT, html, text


def _figures(h):
    return re.findall(r"<figure\b.*?</figure>", h, re.DOTALL)


def _figcaps(h):
    return [re.sub(r"\s+", " ", re.sub(r"<[^>]+>", "", m)).strip()
            for m in re.findall(r"<figcaption>(.*?)</figcaption>", h, re.DOTALL)]


# ----- Step 1: hierarchical numbers -----
def test_hierarchical_figure_id_and_display():
    h = html("figures_hier.pdf")
    g = GT["figures_hier.pdf"]
    assert f'id="{g["fig_id"]}"' in h, f'missing {g["fig_id"]}; ids={re.findall(chr(39)+"id=.(fig-[^\"]*)"+chr(39), h)}'
    assert f'id="{g["tab_id"]}"' in h, f'missing {g["tab_id"]}'
    # not truncated to the leading integer
    assert 'id="fig-2"' not in h and 'id="tab-0"' not in h, "number truncated to leading integer"
    # display keeps the real hierarchical number
    assert any(g["fig_caption"] in c for c in _figcaps(h)), "figure caption lost its full number"


def test_figureless_caption_linked_not_bare_p():
    """A matched caption with no recoverable graphic still lives inside a <figure>
    (an honest content-loss marker keeping the caption as a <figcaption>), never a bare <p>.
    Spurious empties (List-of-Figures dot-leader entries) and recoverable small vectors are
    suppressed/recovered upstream — see test_figure_recovery.py."""
    h = html("figures_hier.pdf")
    probe = GT["figures_hier.pdf"]["linked_no_graphic"]  # "Figure 6"
    assert any(probe in c for c in _figcaps(h)), "figure-less caption not a <figcaption>"
    assert not re.search(rf"<p>\s*{probe}\b", h), "caption leaked as a bare <p>"


# ----- Step 2: continued suppression -----
def test_continued_caption_suppressed():
    h = html("figures_continued.pdf")
    g = GT["figures_continued.pdf"]
    caps = _figcaps(h)
    assert sum(g["caption"] in c for c in caps) == 1, f"expected one '{g['caption']}' caption, got {caps}"
    assert f'id="{g["dup_id"]}"' not in h, "continuation emitted a duplicate figure id"
    assert not any("Continued" in c for c in caps), "‘Continued’ emitted as a caption"


# ----- Step 3: broadened labels + xref guard -----
def test_nodot_and_nature_captions():
    h = html("figures_nodot.pdf")
    g = GT["figures_nodot.pdf"]
    assert f'id="{g["nodot_id"]}"' in h, "‘Fig 5’ (no dot) not attached as fig-5"
    assert f'id="{g["nature_id"]}"' in h, "‘Figure 7 |’ (Nature) not attached as fig-7"


def test_inline_fig_ref_not_a_caption():
    h = html("figures_nodot.pdf")
    probe = GT["figures_nodot.pdf"]["inline_xref"]  # "As shown in Fig 5 the trend"
    assert not any(probe[:20] in c for c in _figcaps(h)), "inline ‘As shown in Fig 5 …’ wrongly captioned"
    assert probe[:20] in text(h), "inline xref text lost from body"
