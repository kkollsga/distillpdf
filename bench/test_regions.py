#!/usr/bin/env python3
"""GOAL gate (Phase 1): region typing + opportunistic caption/figure linking.

This starts RED and turns green as Phase 1 lands. It measures:
  figure coverage  — embedded images wrapped in <figure> (vs bare <img>)
  caption attach   — "Figure N"/"Table N" captions surfaced as <figcaption>
                     (opportunistic: only counted where such captions exist)

Regions stand on their own; captions/IDs are enrichment, never a precondition,
so the caption bar is lenient. Needs only distillpdf (+ frozen text fixtures).

Run:  python3 bench/test_regions.py   (report; non-zero exit until Phase 1 done)
"""
import glob, os, re, sys

HERE = os.path.dirname(os.path.abspath(__file__))
CORPUS = sorted(glob.glob(os.path.join(HERE, "corpus", "*.pdf"))
                + glob.glob(os.path.join(HERE, "corpus_headers", "*.pdf")))
FIX = os.path.join(HERE, "fixtures")

MIN_FIGURE_COVERAGE = 0.90   # embedded <img> that sit inside a <figure>
MIN_CAPTION_ATTACH = 0.50    # of corpus Figure N / Table N captions

CAPTION = re.compile(r"(?:^|\n)\s*(?:Figure|Fig\.|Table)\s+\d+", re.IGNORECASE)


def run():
    import distillpdf
    tot_img = tot_fig_img = 0
    tot_caps = tot_figcaption = 0

    print(f"{'PDF':30s}{'imgs':>6s}{'fig':>6s}{'caps':>6s}{'figcap':>8s}")
    for path in CORPUS:
        name = os.path.basename(path)
        d = distillpdf.Pdf.open(path)
        html = d.to_html()
        # fraction of emitted <img> that sit inside a <figure>
        n_img = len(re.findall(r"<img\b", html))
        fig_img = len(re.findall(r"<figure\b[^>]*>(?:(?!</figure>).)*?<img\b", html, re.DOTALL))
        # An attached caption is a figure's <figcaption> OR a table's caption rendered
        # as the table's sibling <div> (the table-caption-width design emits the caption
        # next to its <table>, not as a <figcaption>). Both are "surfaced + attached".
        # The standalone orphan <div id="tab-N"> (no adjacent table) is NOT counted.
        figcap = len(re.findall(r"<figcaption\b", html))
        figcap += len(re.findall(r"</table>\s*<div>", html))
        figcap += len(re.findall(r"<div>(?:(?!</div>).)*?</div>\s*<table\b", html, re.DOTALL))
        # expected captions from the frozen reference text
        ref_p = os.path.join(FIX, f"{os.path.splitext(name)[0]}.pymupdf_plain.txt")
        caps = len(CAPTION.findall(open(ref_p, encoding="utf-8", errors="replace").read())) if os.path.exists(ref_p) else 0

        tot_img += n_img; tot_fig_img += fig_img
        tot_caps += caps; tot_figcaption += figcap
        if n_img or caps:
            print(f"{name[:30]:30s}{n_img:>6d}{fig_img:>6d}{caps:>6d}{figcap:>8d}")

    fig_cov = tot_fig_img / tot_img if tot_img else 1.0
    cap_attach = min(1.0, tot_figcaption / tot_caps) if tot_caps else 1.0
    print(f"\nfigure coverage: {fig_cov:.3f} (need >={MIN_FIGURE_COVERAGE})  "
          f"[{tot_fig_img}/{tot_img} images in <figure>]")
    print(f"caption attach : {cap_attach:.3f} (need >={MIN_CAPTION_ATTACH})  "
          f"[{tot_figcaption} <figcaption> / {tot_caps} captions]")

    failures = []
    if fig_cov < MIN_FIGURE_COVERAGE:
        failures.append(f"figure coverage {fig_cov:.3f} < {MIN_FIGURE_COVERAGE}")
    if cap_attach < MIN_CAPTION_ATTACH:
        failures.append(f"caption attach {cap_attach:.3f} < {MIN_CAPTION_ATTACH}")
    if failures:
        print("\n❌ GOAL not yet met (Phase 1):")
        for f in failures:
            print("  -", f)
        return False
    print("\n✅ Phase 1 goal met")
    return True


def test_regions():
    assert run(), "Phase 1 region/caption goal not met (see report)"


if __name__ == "__main__":
    sys.exit(0 if run() else 1)
