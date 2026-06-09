# distillPDF OCR — setup guide

distillPDF OCRs scanned / image-only pages. There are two tiers; pick by what you need.

## Fast tier (default) — nothing to install
The **fast** engine (Tesseract) is **bundled in the wheel** — no extra, no model download,
fully offline. English, Portuguese and Norwegian ship in it (the document language is
auto-detected). Just:
```bash
pip install distillpdf
```
```python
doc = distillpdf.open("scan.pdf"); doc.run_ocr(); doc.to_pdf("out.pdf")
# or:  distillpdf scan.pdf --ocr
```
Other languages: point `TESSDATA_PREFIX` at a tessdata folder.

## Accurate tier — the granite-docling VLM (tables + structure)
Heavier and slower, but recovers **tables, headings and reading order**. Installed via the
`[ocr]` extra, which pulls the right runtime for your platform — **no C++ compiler needed**:

| OS | `pip install 'distillpdf[ocr]'` pulls | Engine | GPU |
|---|---|---|---|
| **macOS (Apple Silicon)** | `mlx-vlm` | granite-docling (MLX) | Metal — automatic |
| **Windows / Linux / Intel-Mac** | `torch` + `transformers` | granite-docling (PyTorch) | CUDA — see below |

```bash
pip install "distillpdf[ocr]"
```
```python
doc.run_ocr(engine="granite"); doc.to_pdf("out.pdf")
# or:  distillpdf scan.pdf --ocr --ocr-engine granite
```

### GPU acceleration
- **macOS:** MLX runs on the Metal GPU automatically — nothing to do.
- **Windows / Linux (NVIDIA):** PyPI's default `torch` is **CPU-only and slow** for a VLM.
  Install the CUDA build first, then the engine uses it automatically:
  ```bash
  pip install torch --index-url https://download.pytorch.org/whl/cu124
  pip install "distillpdf[ocr]"
  ```
  Force a device with `OcrConfig(device="cuda"|"cpu")`. On **CPU** granite is very slow
  (minutes/page) — prefer a GPU, or use the fast tier.
- **PyTorch on Mac (MPS)** is *not* used: granite/idefics3 misbehaves on MPS — use MLX (the
  default) for Mac GPU.

### Lightweight alternative: GGUF (llama.cpp)
A smaller, no-PyTorch runtime. Opt in and select it explicitly:
```bash
pip install "distillpdf[ocr-gguf]"
```
```python
doc.run_ocr(engine="granite-docling-gguf")
```
⚠️ On **Windows**, `llama-cpp-python` may build from source when no prebuilt wheel matches your
Python (needs a C++ compiler / MSVC). To avoid that, install a prebuilt wheel:
```bash
pip install llama-cpp-python --extra-index-url https://abetlen.github.io/llama-cpp-python/whl/cpu
```
or just use the default PyTorch tier (`pip install "distillpdf[ocr]"`), which needs no compiler.

## Models / cache
Granite weights download on first use to a visible **`./ocr_model/`** folder (override with
`OcrConfig(model_dir=...)`). Gated/private repos: set `HF_TOKEN` (env, or a `.env` file; or
`OcrConfig(hf_token=..., store_token=True)`). The default public model needs no token.
