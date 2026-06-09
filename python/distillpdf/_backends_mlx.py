"""granite-docling OCR backend for Apple Silicon, via MLX (mlx-vlm).

This is the default backend on macOS / Apple Silicon. It runs the **official** IBM
checkpoint ``ibm-granite/granite-docling-258M-mlx`` natively on the Metal GPU through
mlx-vlm — no PyTorch, no GGUF, and no manual tiling: the model's idefics3 image processor
splits each page internally, so it sees the whole page at full resolution. That fixes the
two big limitations of the previous llama.cpp path — lower text accuracy and, critically,
broken tables (tiling fragmented them so OTSL never reached the renderer).

Heavy dependencies are imported lazily inside the backend so importing this module stays
cheap and a base install still gives a clear error the first time OCR is actually run.
"""
from __future__ import annotations

import io
import re
from typing import Any, List, Optional

from .ocr import OcrBackend, OcrConfig, _require, register_backend, resolve_hf_token, setup_help

# The official IBM MLX (Apple-Silicon) build. Emits DocTags incl. native OTSL tables.
_MLX_REPO = "ibm-granite/granite-docling-258M-mlx"
_STOP_STRINGS = ["</doctag>", "<|end_of_text|>"]

_TAG = re.compile(r"<[^>]+>")
# Generation-time loop guard (docling ships a DocTagsRepetitionStopper for the same reason):
# if the last LOOP_WINDOW emitted lines collapse to <= LOOP_DISTINCT distinct strings, the
# model is stuck repeating — stop early to save tokens/time. The Rust de-loop still cleans
# any residue post-hoc.
_LOOP_WINDOW = 12
_LOOP_DISTINCT = 2


class MlxGraniteDoclingBackend(OcrBackend):
    """Run granite-docling-258M on Apple Silicon via mlx-vlm (Metal, no PyTorch)."""

    name = "granite-docling"
    output = "doctags"
    tier = "accurate"
    structure_aware = True          # emits OTSL tables + tagged headings
    bundled = False                 # needs the [ocr] extra + a model download
    offline = False
    detail = "granite-docling VLM via MLX (Apple Silicon). Tables + structure; needs [ocr]."

    @classmethod
    def is_available(cls) -> bool:
        import importlib.util
        return importlib.util.find_spec("mlx_vlm") is not None

    def __init__(self, config: Optional[OcrConfig] = None, **kwargs):
        super().__init__(config, **kwargs)
        if self.config.model_id is None:
            self.config.model_id = _MLX_REPO
        if not self.config.stop_strings:
            self.config.stop_strings = list(_STOP_STRINGS)
        self._model: Any = None
        self._processor: Any = None
        self._mlx_config: Any = None
        self._apply_chat_template: Any = None
        self._stream_generate: Any = None

    # -- model loading -------------------------------------------------------
    def _load(self):
        if self._model is not None:
            return
        _require("mlx_vlm", package="mlx-vlm", hint=setup_help(self.name))
        from mlx_vlm import load, stream_generate
        from mlx_vlm.prompt_utils import apply_chat_template
        from mlx_vlm.utils import load_config

        resolve_hf_token(self.config)  # exports HF_TOKEN (from config/.env) so mlx-vlm sees it
        # `model_id` may be a HF repo id or a local path; mlx-vlm downloads/caches as needed.
        src = self.config.model_id
        self._model, self._processor = load(src)
        self._mlx_config = load_config(src)
        self._apply_chat_template = apply_chat_template
        self._stream_generate = stream_generate

    # -- inference -----------------------------------------------------------
    def ocr_page(self, image: bytes) -> str:
        """One full page image (PNG/JPEG bytes) → DocTags. No tiling — the model handles
        resolution natively."""
        self._load()
        _require("PIL", package="pillow")
        from PIL import Image

        img = Image.open(io.BytesIO(image)).convert("RGB")
        prompt = self._apply_chat_template(self._processor, self._mlx_config, self.config.prompt, num_images=1)

        stops: List[str] = self.config.stop_strings or _STOP_STRINGS
        out: List[str] = []
        text = ""
        pending = ""  # current (incomplete) line
        recent: List[str] = []  # stripped text of recently completed lines
        for token in self._stream_generate(
            self._model,
            self._processor,
            prompt,
            [img],
            max_tokens=self.config.max_tokens,
            temp=0.0,  # deterministic OCR
            verbose=False,
        ):
            out.append(token.text)
            text += token.text
            # Stop as soon as the document-end / EOS marker appears (avoids over-generation).
            if any(s in text for s in stops):
                break
            # Loop guard: track completed lines; bail if recent ones collapse to ~one string.
            pending += token.text
            if "\n" in token.text:
                *done, pending = pending.split("\n")
                for ln in done:
                    key = _TAG.sub("", ln).strip()
                    if key:
                        recent.append(key)
                recent = recent[-_LOOP_WINDOW:]
                if len(recent) >= _LOOP_WINDOW and len(set(recent)) <= _LOOP_DISTINCT:
                    break
        return "".join(out)

    def close(self) -> None:
        self._model = None
        self._processor = None
        self._mlx_config = None


register_backend("granite-docling", MlxGraniteDoclingBackend)
