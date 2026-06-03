# distillpdf

**Pure-Rust PDF extraction that distills documents into clean, LLM-ready output.**

`distillpdf` reads a PDF and reconstructs its *structure* — reading order, headings,
paragraphs, lists, tables, and figures — then emits compact, semantic **HTML** (or plain
text) suitable for feeding to language models or building RAG pipelines. It is built on
[`lopdf`](https://github.com/J-F-Liu/lopdf) and shipped to Python via
[PyO3](https://pyo3.rs) + [maturin](https://www.maturin.rs) as a small, permissively
licensed wheel — a lightweight, MIT/Apache alternative to AGPL/heavyweight extractors.

> Status: early (`0.0.1`), pre-PyPI. The API surface is small and may still change.

## Why

- **Structure, not just text.** Two-column reading order, grouped multi-level table
  headers mapped onto one grid (`colspan`), vector figures transcoded to inline SVG
  (including rotated axis labels), an auto-generated table of contents, and section
  extraction (`Pdf.section("abstract")`).
- **LLM-ready.** Lean, class-free HTML with a centered max-width layout — semantic
  markup an LLM can read directly, no styling noise.
- **Small & permissive.** Pure Rust on `lopdf`, no system dependencies, MIT OR Apache-2.0.

## Install

Not yet on PyPI. Build from source with [maturin](https://www.maturin.rs):

```bash
git clone https://github.com/kkollsga/distillpdf
cd distillpdf
maturin build --release
pip install target/wheels/distillpdf-*.whl
```

## Quickstart

```python
import distillpdf

doc = distillpdf.Pdf.open("paper.pdf")
html = doc.to_html()            # clean, semantic HTML for an LLM
toc  = doc.toc()                # [(level, title, page, id), ...]
abstract = doc.section("abstract")   # targeted section extraction
```

## Scope

In scope: text, table, image and font extraction, and an HTML/markdown output layer for
RAG. Out of scope (for now): page rendering, PDF generation, OCR.

## Development

The test suite lives in [`tests/`](tests/) (pytest) and runs on CI. It needs only
`distillpdf` installed. CI runs entirely on data we own — a self-contained demo PDF
(`tests/demo/`, end-to-end structure check) and a synthetic table corpus
(`tests/corpus_tables/`). The third-party PDF corpora (`tests/corpus*/`) are gitignored,
so their tests self-skip on a fresh clone and run only when the corpora are present
locally for deeper coverage:

```bash
bash tests/run.sh        # build distillpdf + run pytest
pytest tests/ -q         # or just run the tests against an installed build
```

## License

MIT OR Apache-2.0, at your option.
