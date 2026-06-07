"""granite-docling OCR backend (default), via llama-cpp-python + huggingface_hub.

Heavy dependencies are imported lazily inside the backend, so importing this module
(which only registers the backend) stays cheap and a base install still gives a clear
error the first time OCR is actually run.
"""
from __future__ import annotations

import base64
from typing import Optional

from .ocr import OcrBackend, OcrConfig, _require, register_backend

# Defaults: the GGUF the llama.cpp team publishes (Q8_0 weights + F16 vision projector).
_REPO = "ggml-org/granite-docling-258M-GGUF"
_MODEL_FILE = "granite-docling-258M-Q8_0.gguf"
_MMPROJ_FILE = "mmproj-granite-docling-258M-f16.gguf"


class GraniteDoclingBackend(OcrBackend):
    """Run granite-docling-258M (GGUF) in-process via llama-cpp-python (Metal/CPU)."""

    name = "granite-docling"
    output = "doctags"

    def __init__(self, config: Optional[OcrConfig] = None, **kwargs):
        super().__init__(config, **kwargs)
        if self.config.model_id is None:
            self.config.model_id = _REPO
        self._llm = None  # lazily loaded on first page

    # -- model loading -------------------------------------------------------

    def _load(self):
        if self._llm is not None:
            return self._llm
        hub = _require("huggingface_hub", package="huggingface-hub")
        llama_cpp = _require("llama_cpp", package="llama-cpp-python")

        def fetch(fname: str) -> str:
            return hub.hf_hub_download(
                repo_id=self.config.model_id,
                filename=fname,
                cache_dir=self.config.model_dir,  # None → HF cache (HF_HOME/HF_HUB_CACHE)
                token=self.config.hf_token,
            )

        model_path = fetch(_MODEL_FILE)
        mmproj_path = fetch(_MMPROJ_FILE)

        # Multimodal chat handler bound to the vision projector (mmproj).
        from llama_cpp.llama_chat_format import Llava15ChatHandler

        n_gpu_layers = 0 if self.config.device == "cpu" else -1  # -1 = offload all (Metal/CUDA)
        self._handler = Llava15ChatHandler(clip_model_path=mmproj_path, verbose=False)
        self._llm = llama_cpp.Llama(
            model_path=model_path,
            chat_handler=self._handler,
            n_ctx=8192,
            n_gpu_layers=n_gpu_layers,
            verbose=False,
        )
        return self._llm

    # -- inference -----------------------------------------------------------

    def ocr_page(self, image: bytes) -> str:
        llm = self._load()
        data_url = "data:image/png;base64," + base64.b64encode(image).decode()
        resp = llm.create_chat_completion(
            messages=[
                {
                    "role": "user",
                    "content": [
                        {"type": "image_url", "image_url": {"url": data_url}},
                        {"type": "text", "text": self.config.prompt},
                    ],
                }
            ],
            temperature=0.0,
            max_tokens=self.config.max_tokens,
            repeat_penalty=1.1,  # prevents the greedy repetition runaway on sparse pages
        )
        return resp["choices"][0]["message"]["content"] or ""

    def close(self) -> None:
        self._llm = None
        self._handler = None


register_backend("granite-docling", GraniteDoclingBackend)
