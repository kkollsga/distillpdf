"""distillpdf — pure-Rust PDF extraction on lopdf."""
from ._distillpdf import Pdf, __version__, from_bytes, open
from . import ocr
from .ocr import OcrBackend, OcrConfig, OcrDependencyError, available_backends, get_backend

__all__ = [
    "Pdf",
    "open",
    "from_bytes",
    "__version__",
    "ocr",
    "OcrBackend",
    "OcrConfig",
    "OcrDependencyError",
    "get_backend",
    "available_backends",
]
