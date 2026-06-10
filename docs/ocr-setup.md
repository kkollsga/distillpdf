# distillPDF OCR — setup guide

distillPDF OCRs scanned / image-only pages. There are two tiers. The **fast** tier is bundled
and needs nothing. The **accurate** tier needs a heavy, platform-specific model runtime — there's
**no catch-all `[ocr]` extra**; you pick a path and install it yourself (distillpdf prints the
exact commands via `distillpdf.ocr.install_help("granite")`).

## Fast tier (default) — nothing to install
The **fast** engine (Tesseract) is **bundled in the wheel** — no extra, no model download, fully
offline. English, Portuguese and Norwegian ship in it (language auto-detected).
```bash
pip install distillpdf
```
```python
doc = distillpdf.open("scan.pdf"); doc.run_ocr(); doc.to_pdf("out.pdf")
# or:  distillpdf scan.pdf --ocr
```
Other languages: point `TESSDATA_PREFIX` at a tessdata folder.

## Accurate tier — granite-docling VLM (tables + structure)
Heavier and slower, but recovers **tables, headings and reading order**. Install the runtime for
your platform, then `doc.run_ocr(engine="granite")`. Not sure what you need? Run:
```python
print(distillpdf.ocr.install_help("granite"))   # prints the commands for your OS
```

### macOS (Apple Silicon) — MLX (Metal GPU, automatic)
For Apple Silicon, granite-docling on MLX is recommended (runs on the Metal GPU):
```bash
pip install mlx-vlm "transformers>=4.57,<5" pillow
```

### Windows / Linux / Intel-Mac — PyTorch (no C++ compiler)
For Windows/Linux, granite-docling on PyTorch is recommended — `torch` ships prebuilt wheels for
every platform + Python, so this installs cleanly with no C++ compiler:
```bash
pip install torch "transformers>=4.57,<5" pillow
```
**GPU (NVIDIA):** PyPI's default `torch` is **CPU-only and slow** for a VLM. Install the CUDA
build instead and the engine uses it automatically:
```bash
pip install torch --index-url https://download.pytorch.org/whl/cu124
pip install "transformers>=4.57,<5" pillow
```
Force a device with `OcrConfig(device="cuda"|"cpu")`. On **CPU** granite is very slow
(minutes/page) — use a GPU or the fast tier. (PyTorch-on-Mac MPS is not used — granite/idefics3
misbehaves there; Mac uses MLX for GPU.)

### Lightweight alternative — GGUF (llama.cpp), `engine="granite-docling-gguf"`
Smaller, no PyTorch:
```bash
pip install llama-cpp-python huggingface-hub pillow
```
**Note:** on **Windows**, `llama-cpp-python` may build from source when no prebuilt wheel matches
your Python (needs MSVC). Use a prebuilt wheel:
```bash
pip install llama-cpp-python --extra-index-url https://abetlen.github.io/llama-cpp-python/whl/cpu
pip install huggingface-hub pillow
```
or just use the PyTorch path above (no compiler).

> **Why `transformers>=4.57,<5`?** transformers 5.x changed the idefics3 image processor and
> fails to load granite-docling; `>=4.57` is the floor that supports it. mlx-vlm's own
> dependency range can pull in 5.x, so pin it explicitly.

> Optional: `pip install tqdm` to get a progress bar during OCR.

## Models / cache
The default granite models are **public** and download on first use. The **PyTorch** and **GGUF**
runtimes put weights in a visible **`./ocr_model/`** folder (override with
`OcrConfig(model_dir=...)`); the **MLX** runtime fetches the model automatically (mlx-vlm manages
it). No token is needed for the default models. For a gated/private repo, set `HF_TOKEN` (env or a
`.env`; or `OcrConfig(hf_token=..., store_token=True)`).
