#!/usr/bin/env python3
"""GOAL gate (Phase 2): borderless table detection on real arXiv papers.

Word-level spans (Phase 3) let the position detector recover space-separated
tables (e.g. bert Table 1, the GLUE results). This gate measures, on the arXiv
corpus:
  presence recall — pages carrying a "Table N" caption that also yield >=1
                    detected table (we don't miss captioned tables)
  spot check      — bert's GLUE table (page 6) parses with a "System" header
                    and the GLUE task columns

Needs only distillpdf. Run: python3 bench/test_tables_real.py
"""
import glob, os, re, sys

HERE = os.path.dirname(os.path.abspath(__file__))
ARXIV = [p for p in (glob.glob(os.path.join(HERE, "corpus", "arxiv_*.pdf"))
                     + glob.glob(os.path.join(HERE, "corpus", "attention_*.pdf")))]
CAP = re.compile(r"(?:^|\n)\s*Table\s+\d+", re.IGNORECASE)

MIN_PRESENCE_RECALL = 0.70


def run():
    import distillpdf
    cap_pages = found = 0
    print(f"{'PDF':24s}{'capPg':>7s}{'hit':>5s}")
    for path in sorted(ARXIV):
        d = distillpdf.Pdf.open(path)
        tabs = d.extract_tables()
        pages_with_tab = set(t["page"] for t in tabs)
        c = h = 0
        for pg in range(1, d.page_count() + 1):
            if CAP.search(d.extract_page_text(pg)):
                c += 1
                if pg in pages_with_tab:
                    h += 1
        cap_pages += c; found += h
        print(f"{os.path.basename(path)[:24]:24s}{c:>7d}{h:>5d}")

    presence = found / cap_pages if cap_pages else 1.0
    print(f"\npresence recall: {presence:.3f} (need >={MIN_PRESENCE_RECALL})  [{found}/{cap_pages} caption pages have a table]")

    # Spot check: bert GLUE table (Table 1, page 6).
    bert = os.path.join(HERE, "corpus", "arxiv_bert.pdf")
    glue_ok = False
    if os.path.exists(bert):
        for t in distillpdf.Pdf.open(bert).extract_tables():
            if t["page"] == 6:
                flat = " ".join(c for row in t["cells"] for c in row)
                if "System" in flat and sum(k in flat for k in ("MNLI", "QQP", "QNLI", "SST")) >= 3:
                    glue_ok = True
    print(f"bert GLUE table (p6) parsed: {glue_ok}")

    failures = []
    if presence < MIN_PRESENCE_RECALL:
        failures.append(f"table presence recall {presence:.3f} < {MIN_PRESENCE_RECALL}")
    if not glue_ok:
        failures.append("bert GLUE table (Table 1) not parsed with expected columns")
    if failures:
        print(f"\n🔴 GOAL not yet met (Phase 2):")
        for f in failures:
            print("  -", f)
        return False
    print("\n✅ Phase 2 goal met")
    return True


def test_tables_real():
    assert run(), "Phase 2 real-table goal not met (see report)"


if __name__ == "__main__":
    sys.exit(0 if run() else 1)
