#!/usr/bin/env python3
"""Benchmark distillpdf (our wheel) on the same corpus, mirroring the others."""
import glob, json, os, time, statistics
import distillpdf

CORPUS = sorted(glob.glob(os.path.join(os.path.dirname(__file__), "corpus", "*.pdf")))
OUT = os.path.join(os.path.dirname(__file__), "out")
RUNS = 5

def timeit(fn, runs=RUNS):
    fn()
    ts = []
    for _ in range(runs):
        t = time.perf_counter(); fn(); ts.append(time.perf_counter()-t)
    return statistics.median(ts)

results = {}
for path in CORPUS:
    name = os.path.splitext(os.path.basename(path))[0]
    rec = {"file": os.path.basename(path), "bytes": os.path.getsize(path)}
    box = {}
    def run():
        d = distillpdf.Pdf.open(path); box["txt"] = d.extract_text(); box["pc"] = d.page_count()
    try:
        rec["t_text_s"] = round(timeit(run), 4)
        rec["pages"] = box["pc"]
        txt = box["txt"]; rec["chars"] = len(txt); rec["ok"] = True
        open(os.path.join(OUT, f"{name}.distillpdf.txt"), "w").write(txt)
    except Exception as e:
        rec["ok"] = False; rec["error"] = str(e)[:160]
    print(f"{name:30s} ok={rec.get('ok')} pages={rec.get('pages')} "
          f"t={rec.get('t_text_s')}s chars={rec.get('chars')}")
    results[name] = rec

json.dump(results, open(os.path.join(OUT, "results_distillpdf.json"), "w"), indent=2)
print("\nsaved -> out/results_distillpdf.json")
