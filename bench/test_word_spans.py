#!/usr/bin/env python3
"""GOAL gate (Phase 3): word-level span extraction.

Table cells are useless to detect while a whole numeric row arrives as ONE span
(e.g. bert Table 1: "84.6/83.4 71.2 90.5 …" is a single Tj). After word-level
splitting, each value is its own positioned span and column alignment becomes
detectable. This gate fails while any span still packs many space-separated
numeric tokens (the row-as-one-span symptom); it turns green once spans are
word-granular.

Run:  python3 bench/test_word_spans.py   (non-zero exit until Phase 3 done)
"""
import glob, os, re, sys

HERE = os.path.dirname(os.path.abspath(__file__))
CORPUS = sorted(glob.glob(os.path.join(HERE, "corpus", "*.pdf"))
                + glob.glob(os.path.join(HERE, "corpus_headers", "*.pdf")))
MAX_NUMERIC_TOKENS_PER_SPAN = 3  # a span with more is an un-split table row

NUMERIC = re.compile(r"^[-+]?\d[\d.,/%]*$")


def numeric_tokens(text):
    return sum(1 for tok in text.split() if NUMERIC.match(tok))


def run():
    import distillpdf
    worst = 0
    worst_where = ""
    for path in CORPUS:
        d = distillpdf.Pdf.open(path)
        for pg in range(1, d.page_count() + 1):
            for t, x, w, s in d._dbg_spans(pg):
                n = numeric_tokens(t)
                if n > worst:
                    worst, worst_where = n, f"{os.path.basename(path)} p{pg}: {t.strip()[:50]!r}"
    print(f"max numeric tokens packed in one span: {worst} (need <= {MAX_NUMERIC_TOKENS_PER_SPAN})")
    print(f"  worst: {worst_where}")
    if worst > MAX_NUMERIC_TOKENS_PER_SPAN:
        print("\n🔴 GOAL not yet met (Phase 3): table rows still arrive as single spans.")
        return False
    print("\n✅ Phase 3 goal met: spans are word-granular.")
    return True


def test_word_spans():
    assert run(), "Phase 3 word-span goal not met (see report)"


if __name__ == "__main__":
    sys.exit(0 if run() else 1)
