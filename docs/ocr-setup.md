# distillPDF OCR — setup guide

distillPDF OCRs scanned and image-only pages with one of two engines:

- **Tesseract** — bundled in the wheel, works out of the box. Fast, flat text.
- **granite-docling** — a vision-language model that also recovers **tables, headings and
  reading order**. Slower, and you install its runtime yourself (see below).

## Tesseract — available by default

The base install already contains everything: Tesseract is compiled into the wheel, so there
is no extra package, no model download, and it runs fully offline.

```bash
pip install distillpdf
```

```python
import distillpdf

doc = distillpdf.open("scan.pdf")
doc.run_ocr()                          # bundled Tesseract — no setup needed
doc.to_pdf("scan.searchable.pdf")      # original scan + invisible text layer
doc.to_html("scan.html")               # reuses the same OCR pass (cached)
```

Or from the command line:

```bash
distillpdf scan.pdf --ocr              # → scan.searchable.pdf
```

**English, Portuguese and Norwegian** language data ship in the wheel, and the document
language is auto-detected from a few sample pages. Pin a language with
`doc.run_ocr(languages=["eng"])`, or point `TESSDATA_PREFIX` at a tessdata folder for other
languages.

Tesseract produces flat text only — no table structure. When that matters, use granite.

## granite-docling — the accurate engine

For higher fidelity, **granite-docling is the recommended model**: it reads each page as an
image and emits structure — tagged headings, reading order, and real tables (OTSL) that
distillPDF renders as proper `<table>` HTML.

The trade-off is speed:

| Engine | Speed | Char accuracy | Output |
|---|---|---|---|
| Tesseract (default) | ~0.8 s/page | ~95% | flat text |
| granite-docling | ~6 s/page (GPU) | ~97% | headings, reading order, **tables** |

granite needs a GPU to be practical — on CPU it runs at **minutes per page**. If you have no
GPU, stay on Tesseract.

Because the model runtime is genuinely platform- and hardware-specific, it isn't a pip extra —
you install one of the paths below. Not sure which? distillpdf prints the right commands for
your machine:

```python
print(distillpdf.ocr.install_help("granite"))
```

### Installation paths

#### macOS (Apple Silicon) — MLX

Recommended on Apple Silicon: granite-docling on MLX runs on the Metal GPU.

```bash
pip install mlx-vlm "transformers>=4.57,<5" pillow
```

#### Windows / Linux / Intel Mac — PyTorch

Recommended elsewhere: `torch` ships prebuilt wheels for every platform and Python, so this
installs cleanly with no C++ compiler.

```bash
pip install torch "transformers>=4.57,<5" pillow
```

**NVIDIA GPU:** PyPI's default `torch` is **CPU-only and slow** for a VLM. Install the CUDA
build instead and the engine uses it automatically:

```bash
pip install torch --index-url https://download.pytorch.org/whl/cu124
pip install "transformers>=4.57,<5" pillow
```

Force a device with `OcrConfig(device="cuda"|"cpu")`. (PyTorch-on-Mac MPS is not used —
granite/idefics3 misbehaves there; Mac uses MLX for GPU.)

#### llama.cpp (GGUF) — lightweight alternative

A smaller runtime with no PyTorch, selected explicitly with
`engine="granite-docling-gguf"`:

```bash
pip install llama-cpp-python huggingface-hub pillow
```

On **Windows**, `llama-cpp-python` may build from source when no prebuilt wheel matches your
Python (needs MSVC). Use a prebuilt wheel:

```bash
pip install llama-cpp-python --extra-index-url https://abetlen.github.io/llama-cpp-python/whl/cpu
pip install huggingface-hub pillow
```

or just use the PyTorch path above (no compiler).

> **Why `transformers>=4.57,<5`?** transformers 5.x changed the idefics3 image processor and
> fails to load granite-docling; `>=4.57` is the floor that supports it. mlx-vlm's own
> dependency range can pull in 5.x, so pin it explicitly.

> Optional: `pip install tqdm` to get a progress bar during OCR.

### Using granite

`engine="granite"` picks the right runtime for your platform automatically (MLX on Apple
Silicon, PyTorch elsewhere); name `"granite-docling-gguf"` to use the llama.cpp runtime.

```python
import distillpdf

doc = distillpdf.open("scan.pdf")
doc.run_ocr(engine="granite")               # MLX on Apple Silicon, PyTorch elsewhere
doc.to_html("scan.html")                    # tables come out as real <table> markup
doc.to_pdf("scan.searchable.pdf")           # same pass, cached — the model never re-runs

doc.run_ocr(engine="granite-docling-gguf")  # the llama.cpp runtime, explicitly
doc.run_ocr(engine="granite", only={1, 2})  # try a couple of pages before committing
```

```bash
distillpdf scan.pdf --ocr --ocr-engine accurate     # granite from the CLI
distillpdf --list-ocr-engines                       # what's installed and runnable
```

## Models / cache

The default granite models are **public** and download on first use. The **PyTorch** and
**GGUF** runtimes put weights in a visible **`./ocr_model/`** folder (override with
`OcrConfig(model_dir=...)`); the **MLX** runtime fetches the model automatically (mlx-vlm
manages it). No token is needed for the default models. For a gated/private repo, set
`HF_TOKEN` (env or a `.env`; or `OcrConfig(hf_token=..., store_token=True)`).
