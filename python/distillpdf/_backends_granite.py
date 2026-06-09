"""granite-docling OCR backend (Windows/Linux default), via llama-cpp-python + huggingface_hub.

Registered as ``"granite-docling-gguf"``. This is the default OCR backend on every platform
EXCEPT Apple Silicon, where the native MLX backend (`_backends_mlx.py`, registered
``"granite-docling"``) is used instead — so Mac users never pull llama-cpp-python and
Windows/Linux users never pull PyTorch.

Runs the GGUF the llama.cpp team publishes, in-process via llama-cpp-python (CUDA/Metal/CPU).
Full pages are downscaled hard by the vision encoder, so dense scans are vertically TILED
(`_TILE_MAX_PX`) and the per-tile DocTags are merged back into page coordinates.

Heavy dependencies are imported lazily inside the backend, so importing this module
(which only registers the backend) stays cheap and a base install still gives a clear
error the first time OCR is actually run.
"""
from __future__ import annotations

import base64
import io
import re
from typing import List, Optional

from .ocr import OcrBackend, OcrConfig, _require, register_backend

# Defaults: the GGUF the llama.cpp team publishes (Q8_0 weights + F16 vision projector).
_REPO = "ggml-org/granite-docling-258M-GGUF"
_MODEL_FILE = "granite-docling-258M-Q8_0.gguf"
_MMPROJ_FILE = "mmproj-granite-docling-258M-f16.gguf"

# Above this rendered height (px), a full page is downscaled so far by the vision encoder
# that dense body text becomes unreadable (the model then loops or emits `<other>`). We
# split such pages into vertical tiles, OCR each at a legible resolution, and merge — this
# is the single biggest quality lever on real full-page scans.
_TILE_MAX_PX = 1200

_LOC = re.compile(r"<loc_(\d+)>")


def _retile_locs(doctags: str, tile_idx: int, n_tiles: int) -> str:
    """Map a tile's DocTags `<loc_*>` y-coordinates into the full page's coordinate space.

    DocTags bounding boxes are `<loc_x1><loc_y1><loc_x2><loc_y2>` normalized to 0–500. A tile
    covers vertical band ``[tile_idx/n, (tile_idx+1)/n]`` of the page, so a local y maps to
    ``(tile_idx*500 + y) / n_tiles`` globally. Counting loc tags in quads, every 2nd/4th is a
    y-coordinate. Keeps the merged DocTags' reading order correct for the renderer."""
    if n_tiles <= 1:
        return doctags
    counter = {"i": 0}

    def repl(m: "re.Match") -> str:
        v = int(m.group(1))
        is_y = counter["i"] % 4 in (1, 3)
        counter["i"] += 1
        if is_y:
            v = round((tile_idx * 500 + v) / n_tiles)
        return f"<loc_{v}>"

    return _LOC.sub(repl, doctags)


class GraniteDoclingBackend(OcrBackend):
    """Run granite-docling-258M (GGUF) in-process via llama-cpp-python (Metal/CPU)."""

    name = "granite-docling-gguf"
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

        # Multimodal chat handler bound to the vision projector (mmproj). Prefer the modern
        # libmtmd handler (MTMDChatHandler): it drives the projector through llama.cpp's mtmd
        # pipeline and applies the MODEL's own chat template from the GGUF — granite-docling
        # is idefics3-based (`<|start_of_role|>…`), so the legacy Llava15 handler's
        # `USER:/ASSISTANT:` template mismatches and yields role-echo / repetition garbage.
        import llama_cpp.llama_chat_format as _cf

        n_gpu_layers = 0 if self.config.device == "cpu" else -1  # -1 = offload all (Metal/CUDA)
        Handler = getattr(_cf, "MTMDChatHandler", None) or _cf.Llava15ChatHandler
        self._handler = Handler(clip_model_path=mmproj_path, verbose=False)
        self._llm = llama_cpp.Llama(
            model_path=model_path,
            chat_handler=self._handler,
            n_ctx=8192,
            n_gpu_layers=n_gpu_layers,
            verbose=False,
        )
        return self._llm

    # -- inference -----------------------------------------------------------

    def _infer(self, image: bytes) -> str:
        """One model pass over one image → its DocTags string."""
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

    def _tiles(self, image: bytes) -> List[bytes]:
        """Split a tall page into vertical tiles legible to the vision encoder; return the
        tile PNGs top-to-bottom (a single-element list when the page is short enough)."""
        _require("PIL", package="pillow")
        from PIL import Image
        im = Image.open(io.BytesIO(image)).convert("RGB")
        w, h = im.size
        n = max(1, round(h / _TILE_MAX_PX))
        if n <= 1:
            return [image]
        out: List[bytes] = []
        for t in range(n):
            y0, y1 = h * t // n, h * (t + 1) // n
            buf = io.BytesIO()
            im.crop((0, y0, w, y1)).save(buf, "PNG")
            out.append(buf.getvalue())
        return out

    def ocr_page(self, image: bytes) -> str:
        # Tile tall pages so dense body text is read at a legible resolution, then merge the
        # per-tile DocTags into one page-coordinate-consistent result.
        tiles = self._tiles(image)
        if len(tiles) == 1:
            return self._infer(tiles[0])
        parts = [_retile_locs(self._infer(t), i, len(tiles)) for i, t in enumerate(tiles)]
        return "\n".join(p for p in parts if p.strip())

    def close(self) -> None:
        self._llm = None
        self._handler = None


register_backend("granite-docling-gguf", GraniteDoclingBackend)
