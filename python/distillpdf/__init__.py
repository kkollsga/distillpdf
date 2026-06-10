"""distillpdf — pure-Rust PDF extraction on lopdf."""
from ._distillpdf import (
    Pdf,
    __version__,
    from_bytes as _from_bytes_raw,
    load_model,
    open as _open_raw,
    read_dpdf_member,
    render_html,
    render_markdown,
    render_text,
    save_dpdf,
)
from . import ocr
from .doc import Doc
from .document import Document
from .dpdf import DpdfError, FindResult
from .ocr import OcrBackend, OcrConfig, OcrDependencyError, available_backends, get_backend


def open(path: str) -> Document:
    """Open a PDF from a path. Returns a :class:`Document` (the Rust core plus the OCR
    workflow); all core extraction methods work as before."""
    return Document.open(path)


def from_bytes(data: bytes) -> Document:
    """Open a PDF from raw bytes. Returns a :class:`Document`."""
    return Document.from_bytes(data)


def load(path: str) -> Doc:
    """Load a distilled ``.dpdf`` document model from a path. Returns a :class:`Doc` — the
    single-document reading surface (sections, blocks, ``find``, fidelity re-renders). The
    counterpart to :func:`open`: ``open`` reads a source PDF, ``load`` reads a model distilled
    from one (``distillpdf.open('x.pdf').distill('x.dpdf')``). Raises
    :class:`~distillpdf.dpdf.DpdfError` when ``path`` is not a readable ``.dpdf``."""
    return Doc.load(path)


__all__ = [
    "Pdf",
    "Document",
    "Doc",
    "open",
    "from_bytes",
    "load",
    "load_model",
    "save_dpdf",
    "read_dpdf_member",
    "render_html",
    "render_markdown",
    "render_text",
    "DpdfError",
    "FindResult",
    "__version__",
    "ocr",
    "OcrBackend",
    "OcrConfig",
    "OcrDependencyError",
    "get_backend",
    "available_backends",
]
