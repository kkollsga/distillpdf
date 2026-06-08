"""Placeholder for the Windows/Linux OCR backend (granite-docling via PyTorch/vLLM).

Not yet implemented — paused. On Apple Silicon the default MLX backend is used instead.
"""
from __future__ import annotations

from typing import Optional

from .ocr import OcrBackend, OcrConfig, OcrDependencyError, register_backend

_NOT_READY = (
    "Windows/Linux OCR (granite-docling via PyTorch/vLLM) is not yet implemented. "
    "Apple Silicon users get the default MLX backend automatically. The PyTorch/vLLM "
    "path is planned — see distillpdf/_backends_pytorch.py."
)


class PyTorchGraniteDoclingBackend(OcrBackend):
    """Placeholder — raises a clear error until the PyTorch/vLLM path is built."""

    name = "granite-docling-pytorch"
    output = "doctags"

    def __init__(self, config: Optional[OcrConfig] = None, **kwargs):
        super().__init__(config, **kwargs)

    def ocr_page(self, image: bytes) -> str:
        raise OcrDependencyError(_NOT_READY)


register_backend("granite-docling-pytorch", PyTorchGraniteDoclingBackend)
