#!/usr/bin/env python3
"""Regenerate distillpdf's own output artifacts into out/ (a wipeable cache).
Currently: out/html/<name>.html for every corpus PDF. Run each dev loop so the
HTML on disk always reflects the current build. Needs only distillpdf.
"""
import glob, os, shutil, time
import distillpdf

HERE = os.path.dirname(os.path.abspath(__file__))
CORPUS = sorted(glob.glob(os.path.join(HERE, "corpus", "*.pdf"))
                + glob.glob(os.path.join(HERE, "corpus_headers", "*.pdf")))
HTML = os.path.join(HERE, "out", "html")

# Fresh each run: wipe and rebuild so stale files never linger.
shutil.rmtree(HTML, ignore_errors=True)
os.makedirs(HTML, exist_ok=True)

t0 = time.perf_counter()
for path in CORPUS:
    name = os.path.splitext(os.path.basename(path))[0]
    try:
        open(os.path.join(HTML, f"{name}.html"), "w").write(distillpdf.Pdf.open(path).to_html())
    except Exception as e:
        print(f"  {name}: ERROR {str(e)[:80]}")
print(f"regenerated {len(CORPUS)} HTML files in out/html/ ({time.perf_counter()-t0:.1f}s)")
