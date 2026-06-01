#!/usr/bin/env python3
"""Baseline benchmark: pymupdf4llm (markdown) + plain PyMuPDF get_text.
Times each PDF (median of N runs) and saves reference markdown output."""
import glob, json, os, time, statistics, sys
import pymupdf            # fitz
import pymupdf4llm

CORPUS = sorted(glob.glob(os.path.join(os.path.dirname(__file__), "corpus", "*.pdf")))
OUT = os.path.join(os.path.dirname(__file__), "out")
RUNS = 5

def timeit(fn, runs=RUNS):
    fn()  # warm-up (not counted)
    ts = []
    for _ in range(runs):
        t = time.perf_counter()
        fn()
        ts.append(time.perf_counter() - t)
    return statistics.median(ts)

results = {}
for path in CORPUS:
    name = os.path.splitext(os.path.basename(path))[0]
    rec = {"file": os.path.basename(path), "bytes": os.path.getsize(path)}
    try:
        rec["pages"] = pymupdf.open(path).page_count
    except Exception as e:
        rec["pages"] = None

    # --- pymupdf4llm to_markdown ---
    md_holder = {}
    def run_md():
        md_holder["md"] = pymupdf4llm.to_markdown(path, show_progress=False)
    try:
        rec["t_pymupdf4llm_s"] = round(timeit(run_md), 4)
        md = md_holder["md"]
        rec["md_chars"] = len(md)
        rec["md_words"] = len(md.split())
        with open(os.path.join(OUT, f"{name}.pymupdf4llm.md"), "w") as f:
            f.write(md)
    except Exception as e:
        rec["pymupdf4llm_error"] = str(e)[:200]

    # --- plain PyMuPDF get_text (lower bound, raw text) ---
    def run_plain():
        d = pymupdf.open(path)
        t = "".join(p.get_text() for p in d)
        md_holder["plain"] = t
    try:
        rec["t_pymupdf_plain_s"] = round(timeit(run_plain), 4)
        with open(os.path.join(OUT, f"{name}.pymupdf_plain.txt"), "w") as f:
            f.write(md_holder["plain"])
    except Exception as e:
        rec["pymupdf_plain_error"] = str(e)[:200]

    print(f"{name:32s} pages={rec.get('pages')} "
          f"md={rec.get('t_pymupdf4llm_s')}s plain={rec.get('t_pymupdf_plain_s')}s "
          f"chars={rec.get('md_chars')}")
    results[name] = rec

with open(os.path.join(OUT, "results_pymupdf4llm.json"), "w") as f:
    json.dump(results, f, indent=2)
print("\nsaved -> out/results_pymupdf4llm.json")
