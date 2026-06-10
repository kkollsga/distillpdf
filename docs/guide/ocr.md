# OCR for scanned PDFs

Image-only and scanned pages have no text to extract. distillPDF detects them, OCRs them,
and folds the recovered text back into the same HTML, Markdown, and searchable-PDF outputs —
born-digital pages keep distillPDF's normal extraction untouched.

OCR is opt-in. The default **fast** tier (bundled Tesseract) needs nothing beyond
`pip install distillpdf`; the higher-accuracy **accurate** tier (the granite-docling VLM) needs
a platform-specific runtime you install yourself. The per-OS install detail lives in the
[OCR runtime setup](../ocr-setup.md) guide; this page covers how detection and the OCR API work.

## Detecting scanned pages

Detection reads the page structure, not a guess from the filename. A page is flagged as needing
OCR when its real content lives in a raster the text layer doesn't cover. The detector handles
the awkward real-world cases:

- images nested inside **Form XObjects** (e.g. a whole scan wrapped in one XObject by an
  iText-style e-filing producer);
- **CCITT Group-4** fax encodings, **Flate-wrapped JPEG**, and the other common image encodings;
- a **full-page raster whose only text is an e-filing stamp** — a header/footer annotation
  baked in by the filing system, which is treated as boilerplate rather than as the body text.

A born-digital text page (real text, no dominating image) is left as `NotNeeded` and never sent
to OCR.

`Document.run_processing()` runs this analysis (it is cheap and cached, and the render methods
call it for you). The underlying per-page plan is available on the Rust `Pdf` object as
`ocr_plan()`, which returns one dict per page with `page`, `needs_ocr`, `reason`, `width_pts`,
`height_pts`, and the page `image` bytes when OCR is needed.

## The two tiers

| Tier | Engine | Install | Speed | Quality | Output |
|---|---|---|---|---|---|
| **fast** (default) | bundled **Tesseract** | none — in the wheel | ~0.8 s/page | char ~95% | flat text (no tables) |
| **accurate** | **granite-docling** VLM | a runtime you install (see setup) | ~6 s/page (GPU) | char ~97% | structure + OTSL **tables** |

The **fast** tier works on a plain `pip install distillpdf`: no extra, no PyTorch, no model
download, fully offline. **English, Portuguese, and Norwegian** language data ship in the wheel.
The document language is **auto-detected** from a few sample pages at the start of the OCR pass
and OCR then runs in just that language (faster and a touch more accurate); if detection is
low-confidence, all three bundled languages are used. Pin a language with
`OcrConfig(languages=["eng"])`, or point `TESSDATA_PREFIX` at your own tessdata folder for other
languages. The fast tier produces flat text only — no tables.

The **accurate** tier (granite-docling) recovers tables, headings, and reading order, emitting
OTSL table structure. It needs a heavier model runtime that is genuinely platform- and
hardware-specific, so there is **no catch-all `[ocr]` extra** — you pick a path (MLX on Apple
Silicon, PyTorch on Windows/Linux/Intel-Mac, or the lighter GGUF runtime) and install it. See
[OCR runtime setup](../ocr-setup.md) for the per-OS commands, or print them for your platform:

```python
print(distillpdf.ocr.install_help("granite"))
```

!!! warning "granite on CPU is very slow"
    The accurate tier on CPU runs at minutes per page. Use a GPU (MLX on Apple Silicon, the
    CUDA build of PyTorch on NVIDIA) for it, or stay on the fast tier.

## Running OCR

The simplest path is `run_ocr()` on a `Document`. It OCRs every scanned page **once** and caches
the result on the document, so every subsequent render reuses that single pass — the model never
re-runs.

```python
import distillpdf

doc = distillpdf.open("scanned.pdf")
doc.run_ocr()                  # fast tier by default — bundled, offline; cached on the document
doc.to_pdf("out.pdf")          # searchable PDF        (reuses the cache — no second pass)
doc.to_html("out.html")        # OCR text folded into clean HTML
doc.to_markdown("out.md")      # …and Markdown
```

On an interactive terminal a `tqdm` progress bar is shown automatically (it starts labelled
`OCR (loading model)` so the silent first-call model load is visible). Pass `progress=False` to
silence it, or your own `progress(page, done, total)` callable to handle it yourself.

`run_ocr` is chainable, e.g. `doc.run_ocr().to_html("out.html")`.

### `run_ocr` signature

```python
doc.run_ocr(backend=None, *, engine=None, only=None, progress=None, **config)
```

- **`engine`** — which engine to use. `None` or `"fast"` is the bundled Tesseract (the
  default); `"accurate"` or `"granite"` is the granite-docling VLM; or pass a specific
  registered backend name (`"tesseract"`, `"granite-docling"`, `"granite-docling-gguf"`,
  `"granite-docling-pytorch"`, `"server"`).
- **`only`** — a set of 1-based page numbers to restrict OCR to, e.g. `only={1, 2, 3}`. Useful
  for trying the accurate tier on a few pages before committing to the whole document.
- **`progress`** — `None` for the auto progress bar, `False` to silence it, or a
  `progress(page, done, total)` callable.
- **`**config`** — any [`OcrConfig`](#ocrconfig-knobs) field passed inline (`model_dir`,
  `hf_token`, `device`, `languages`, `prompt`, …); no need to build an `OcrConfig` yourself.
- **`backend`** — for power users, an already-constructed `OcrBackend` instance; the
  alternative to `engine` + `**config` (when given, it takes precedence over `engine`).

```python
doc.run_ocr(engine="granite", model_dir=r"D:\models")   # accurate tier, custom model cache
doc.run_ocr(engine="granite", languages=["por"])        # force a language
doc.run_ocr(only={1, 2, 3})                              # OCR just the first three pages
```

### The `ocr=True` shortcut

For a single output you can skip the explicit `run_ocr` call. `to_html`, `to_markdown`, and
`to_pdf` each take `ocr=True` (plus `engine=` / `backend=`), which runs OCR once if it hasn't
been run yet, then renders:

```python
doc.to_pdf("out.pdf", ocr=True)                       # fast tier
doc.to_html("out.html", ocr=True, engine="granite")   # accurate tier
```

Because the pass is cached, `ocr=True` on a second render reuses the first pass rather than
re-running the model.

### Warning when rendering scans without OCR

The render methods also work *without* OCR. When a document has scanned pages that have not been
OCR'd, `to_html` / `to_markdown` / `to_pdf` emit a `warnings.warn` pointing you at `run_ocr()`
(and at `install_help('granite')` for the accurate tier), then render anyway — leaving those
pages with little or no text. So forgetting OCR is loud, never a silent empty page.

## Searchable PDF: two modes

`to_pdf` writes a searchable PDF from the cached OCR results. There are two modes, set by
`remove_raster`:

- **keep the scan (default, `remove_raster=False`)** — the original page image is preserved and
  the OCR text is added as an **invisible, selectable** layer over it. The scan always shows, so
  OCR errors never destroy content. Best for archival and legal use.
- **`remove_raster=True`** — the OCR'd pages are reflowed to clean **visible text + cropped
  figures** and the raster is dropped, for a much smaller file. For the accurate tier the reflow
  also justifies granite's paragraph blocks for a typeset look.

```python
doc.to_pdf("out.pdf")                       # invisible text layer over the original scan
doc.to_pdf("reflowed.pdf", remove_raster=True)  # reflowed clean text + figures, smaller file
```

## Tables (accurate tier)

Tables are recovered natively by the **accurate** tier: granite-docling emits OTSL table
structure, which distillPDF renders as a real `<table>` (with `<th>`, `colspan`, `rowspan`) in
HTML, and as a **gridded table** (cell rules + a shaded header row) in the searchable PDF. The
fast (Tesseract) tier produces flat text only — use the accurate tier when table structure
matters.

## `OcrConfig` knobs

Backend behavior is configured by `distillpdf.ocr.OcrConfig`. You rarely build one directly —
pass its fields as keyword args to `run_ocr` instead — but the fields are:

| Field | Default | Purpose |
|---|---|---|
| `model_id` | `None` | backend-specific model identifier (e.g. a Hugging Face repo id) |
| `model_dir` | `None` | directory model files download to; `None` = engine default. The GGUF engine uses a visible project-local `./ocr_model` folder |
| `hf_token` | `None` | Hugging Face token for a gated/private model. Falls back to the `HF_TOKEN` env var, then `HF_TOKEN` in a `.env` |
| `env_file` | `None` | path to a `.env` to read `HF_TOKEN` from; `None` auto-detects `./.env`. Only `HF_TOKEN` is read |
| `store_token` | `False` | if `True` and an `hf_token` is given, persist it as `HF_TOKEN` in the `.env` (created if absent, perms `0600`) for later runs. The token is stored in plaintext |
| `device` | `"auto"` | `"auto"` / `"cpu"` / `"metal"` / `"cuda"` |
| `prompt` | `"Convert this page to docling."` | instruction given to the VLM |
| `max_tokens` | `4096` | per-page generation backstop (`stop_strings` normally terminate) |
| `stop_strings` | `[]` | strings that end generation |
| `languages` | `[]` | ISO codes an engine should read, e.g. `["eng", "por"]` (Tesseract). Empty = engine default |
| `dpi` | `None` | render-resolution hint; `None` = engine default |

Fields an engine doesn't use are ignored, so the same dataclass covers every backend. The
default granite models are public and need no token.

## Engine selection helpers

List what's registered and runnable in your install (import-light — no model load):

```python
distillpdf.ocr.available_backends()
# ['granite-docling', 'granite-docling-gguf', 'granite-docling-pytorch', 'server', 'tesseract']
```

Print what to install for an engine (the fast tier is bundled and needs nothing; accurate tiers
return the pip commands for your platform):

```python
print(distillpdf.ocr.install_help())          # the fast/bundled tier
print(distillpdf.ocr.install_help("granite"))  # the accurate tier, per-OS commands
```

## Command line

The `distillpdf` CLI exposes OCR with `--ocr` (full reference on the
[Command line](cli.md) page):

```bash
distillpdf scan.pdf --ocr                       # → scan.searchable.pdf  (fast tier, bundled)
distillpdf scan.pdf --ocr --remove-raster       # reflowed clean text + figures, smaller file
distillpdf scan.pdf --ocr -o out.html           # OCR'd HTML  (use a .md path for Markdown)
distillpdf scan.pdf --ocr --ocr-engine accurate # granite-docling (install a runtime — see setup)
distillpdf --list-ocr-engines                   # show engines: name, tier, bundled, offline, avail
```

`--ocr-engine` takes the same selectors as the Python `engine=` (a tier `fast`/`accurate`, the
`granite` alias, or a specific backend name). A progress bar is shown automatically.

## See also

- [OCR runtime setup](../ocr-setup.md) — per-OS install commands and GPU notes for the accurate tier.
- [Rendering HTML & Markdown](rendering.md) and the [Python API reference](../reference/python.md).
- The runnable [OCR example notebook](https://github.com/kkollsga/distillpdf/blob/main/examples/ocr.ipynb) on GitHub.
