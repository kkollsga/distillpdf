"""granite-docling OCR backend via PyTorch + transformers.

This is the **accurate** tier's default on Windows / Linux / Intel-Mac: unlike the GGUF
runtime (`_backends_granite.py`, which needs a compiler when no llama-cpp-python wheel
exists), PyTorch ships prebuilt wheels for every platform and Python version, so
``pip install 'distillpdf[ocr]'`` works with **no C++ compiler**. Apple Silicon keeps the
lighter MLX backend. The same accurate granite-docling VLM, same DocTags output.

Runs the official transformers checkpoint ``ibm-granite/granite-docling-258M`` (idefics3):
the processor splits the page internally, so no manual tiling. CPU by default (slower) or
CUDA when available. Heavy deps are imported lazily, so a base install gives a clear error.
"""
from __future__ import annotations

import io
from typing import Any, List, Optional

from .ocr import OcrBackend, OcrConfig, _require, register_backend, resolve_hf_token, setup_help

_REPO = "ibm-granite/granite-docling-258M"
_STOP_STRINGS = ["</doctag>", "<|end_of_text|>"]
# Visible, project-local default download dir (same convention as the GGUF backend).
_MODEL_DIR = "ocr_model"


def _pick_device(torch, requested: str) -> str:
    """Resolve OcrConfig.device into a torch device. ``"auto"`` uses **CUDA** if available,
    else **CPU**. An explicit ``"cuda"``/``"mps"``/``"cpu"`` is honored. Apple MPS is NOT
    auto-selected: for granite-docling (idefics3) it produces garbage in fp16 and isn't faster
    — Apple Silicon should use the MLX accurate tier instead (the default there). NOTE: PyPI's
    default torch wheel is CPU-only — for NVIDIA acceleration install the CUDA build, e.g.
    ``pip install torch --index-url https://download.pytorch.org/whl/cu124``."""
    if requested in ("cuda", "mps", "cpu"):
        return requested
    return "cuda" if torch.cuda.is_available() else "cpu"


class PyTorchGraniteDoclingBackend(OcrBackend):
    """Run granite-docling-258M via PyTorch/transformers (CPU or CUDA). No compiler needed."""

    name = "granite-docling-pytorch"
    output = "doctags"
    tier = "accurate"
    structure_aware = True
    bundled = False
    offline = False
    detail = "granite-docling VLM via PyTorch/transformers (Win/Linux/Intel-Mac). Tables; needs [ocr]."

    @classmethod
    def is_available(cls) -> bool:
        import importlib.util
        return (importlib.util.find_spec("torch") is not None
                and importlib.util.find_spec("transformers") is not None)

    def __init__(self, config: Optional[OcrConfig] = None, **kwargs):
        super().__init__(config, **kwargs)
        if self.config.model_id is None:
            self.config.model_id = _REPO
        if not self.config.stop_strings:
            self.config.stop_strings = list(_STOP_STRINGS)
        self._model: Any = None
        self._proc: Any = None
        self._torch: Any = None
        self._device: str = "cpu"

    # -- model loading -------------------------------------------------------
    def _load(self):
        if self._model is not None:
            return
        resolve_hf_token(self.config)  # config / HF_TOKEN env / .env
        hint = setup_help(self.name)
        torch = _require("torch", package="torch", hint=hint)
        _require("transformers", package="transformers", hint=hint)
        from transformers import AutoModelForImageTextToText, AutoProcessor

        self._device = _pick_device(torch, self.config.device)
        # bf16 on CUDA (fast, stable); fp32 elsewhere (fp16 on CPU/MPS is slow or numerically broken).
        dtype = torch.bfloat16 if self._device == "cuda" else torch.float32
        cache_dir = self.config.model_dir or _MODEL_DIR

        self._proc = AutoProcessor.from_pretrained(self.config.model_id, cache_dir=cache_dir)
        self._model = AutoModelForImageTextToText.from_pretrained(
            self.config.model_id, dtype=dtype, cache_dir=cache_dir
        ).to(self._device)
        self._model.eval()
        self._torch = torch

    # -- inference -----------------------------------------------------------
    def ocr_page(self, image: bytes) -> str:
        """One full page image (PNG/JPEG bytes) → DocTags. No tiling (the processor splits)."""
        self._load()
        _require("PIL", package="pillow")
        from PIL import Image
        from transformers import StoppingCriteriaList, StopStringCriteria

        img = Image.open(io.BytesIO(image)).convert("RGB")
        msgs = [{"role": "user", "content": [{"type": "image"}, {"type": "text", "text": self.config.prompt}]}]
        text = self._proc.apply_chat_template(msgs, add_generation_prompt=True)
        inputs = self._proc(text=[text], images=[img], return_tensors="pt").to(self._device)

        stops: List[str] = self.config.stop_strings or _STOP_STRINGS
        criteria = StoppingCriteriaList([StopStringCriteria(self._proc.tokenizer, stops)])
        with self._torch.no_grad():
            ids = self._model.generate(
                **inputs, max_new_tokens=self.config.max_tokens, do_sample=False,
                stopping_criteria=criteria,
            )
        # Decode only the newly generated tokens (drop the prompt), keeping DocTags markers.
        new_ids = ids[:, inputs["input_ids"].shape[1]:]
        return self._proc.batch_decode(new_ids, skip_special_tokens=False)[0]

    def close(self) -> None:
        self._model = None
        self._proc = None
        self._torch = None


register_backend("granite-docling-pytorch", PyTorchGraniteDoclingBackend)
