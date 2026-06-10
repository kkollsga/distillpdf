"""distillpdf — pure-Rust PDF extraction on lopdf."""
from ._distillpdf import Pdf, __version__, from_bytes as _from_bytes_raw, load_model, open as _open_raw
from . import ocr
from .document import Document
from .ocr import OcrBackend, OcrConfig, OcrDependencyError, available_backends, get_backend


def open(path: str) -> Document:
    """Open a PDF from a path. Returns a :class:`Document` (the Rust core plus the OCR
    workflow); all core extraction methods work as before."""
    return Document.open(path)


def from_bytes(data: bytes) -> Document:
    """Open a PDF from raw bytes. Returns a :class:`Document`."""
    return Document.from_bytes(data)


__all__ = [
    "Pdf",
    "Document",
    "open",
    "from_bytes",
    "load_model",
    "__version__",
    "ocr",
    "OcrBackend",
    "OcrConfig",
    "OcrDependencyError",
    "get_backend",
    "available_backends",
]
