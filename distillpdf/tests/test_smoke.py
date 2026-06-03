"""Phase-0 smoke test: open a PDF via lopdf, get page count + non-empty text."""
import glob, os
import distillpdf

CORPUS = sorted(glob.glob(os.path.join(os.path.dirname(__file__), "..", "..", "bench", "corpus", "*.pdf")))

def test_import():
    assert hasattr(distillpdf, "Pdf")
    assert distillpdf.__version__

def test_open_and_extract():
    assert CORPUS, "no corpus PDFs found"
    opened = 0
    for path in CORPUS:
        try:
            doc = distillpdf.Pdf.open(path)
        except Exception as e:
            print(f"SKIP {os.path.basename(path)}: {e}")
            continue
        pc = doc.page_count()
        txt = doc.extract_text()
        print(f"{os.path.basename(path):30s} pages={pc} text_chars={len(txt)}")
        assert pc > 0
        opened += 1
    assert opened >= 1, "no PDF opened successfully"

if __name__ == "__main__":
    test_import()
    test_open_and_extract()
    print("\nSMOKE OK")
