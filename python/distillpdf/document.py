"""The user-facing `Document` — a loaded PDF.

Wraps the pure-Rust core (`_distillpdf.Pdf`) and adds the OCR workflow as methods on the
document itself:

    doc = distillpdf.open("file.pdf")
    doc.run_ocr()                       # OCR scanned pages (opt-in; needs the [ocr] extra)
    doc.to_html("out.html")             # render — OCR text is folded in automatically

`run_processing()` (page analysis) is called automatically by the render methods; if the
document has scanned pages that haven't been OCR'd yet, they emit a warning pointing at
`run_ocr()`. Every other method (extract_tables, toc, page_count, …) delegates straight to
the Rust core.
"""
from __future__ import annotations

import os
import warnings
from typing import Callable, Optional

_LARGE_HTML = 50 * 1024 * 1024  # warn above this when images are embedded inline

from . import ocr as _ocr
from ._distillpdf import from_bytes as _from_bytes
from ._distillpdf import open as _open


class Document:
    """A loaded PDF document. Returned by :func:`distillpdf.open`."""

    def __init__(self, pdf):
        self._pdf = pdf
        self._scanned = None  # cached page numbers needing OCR (set by run_processing)

    # -- constructors --------------------------------------------------------
    @classmethod
    def open(cls, path: str) -> "Document":
        return cls(_open(path))

    @classmethod
    def from_bytes(cls, data: bytes) -> "Document":
        return cls(_from_bytes(data))

    # -- delegate everything else to the Rust core ---------------------------
    def __getattr__(self, name: str):
        # Only reached when normal lookup fails; guard private names to avoid recursing
        # during __init__ (before _pdf is set).
        if name.startswith("_"):
            raise AttributeError(name)
        return getattr(object.__getattribute__(self, "_pdf"), name)

    def __repr__(self) -> str:
        return f"<distillpdf.Document pages={self._pdf.page_count()} ocr={'yes' if self._pdf.has_ocr() else 'no'}>"

    # -- processing ----------------------------------------------------------
    def run_processing(self) -> "Document":
        """Analyze the document: detect which pages are scanned / image-only and need OCR.
        Cheap and cached — the render methods call this automatically."""
        if self._scanned is None:
            self._scanned = [p["page"] for p in self._pdf.ocr_plan() if p["needs_ocr"]]
        return self

    def run_ocr(self, backend: Optional["_ocr.OcrBackend"] = None, *,
                only: Optional[set] = None,
                progress: Optional[Callable[[int, int, int], None]] = None) -> "Document":
        """OCR every scanned page once, caching the result on this document. After this, the
        render methods fold the recovered text into their output. `backend` defaults to the
        bundled granite-docling model — which requires the ``distillpdf[ocr]`` extra and is
        downloaded on first use. Chainable: ``doc.run_ocr().to_html("out.html")``."""
        self.run_processing()
        _ocr.run(self._pdf, backend, only=only, progress=progress)
        return self

    # -- OCR-pending warning -------------------------------------------------
    def _pending_ocr(self):
        self.run_processing()
        done = self._pdf.get_ocr()
        return [p for p in self._scanned if p not in done]

    def _warn_pending(self) -> None:
        pending = self._pending_ocr()
        if pending:
            warnings.warn(
                f"{len(pending)} page(s) look scanned / image-only and have no extractable "
                f"text. Call doc.run_ocr() (or pass ocr=True) to OCR them with the bundled "
                f"engine — no extra, no download, offline. (For the higher-accuracy granite "
                f"engine instead, `pip install 'distillpdf[ocr]'`.) Rendering now leaves those "
                f"pages with little or no text.",
                stacklevel=3,
            )

    def _warn_large_embed(self, path, result, image_mode: str) -> None:
        """Embedding page rasters inline (image_mode='embed', the HTML default) can balloon a
        scanned document to hundreds of MB. Point users at the leaner modes when it does."""
        if image_mode != "embed":
            return
        # When a path is given the result is that path (the HTML went to disk), so size the
        # file; only when rendering to a string is `result` the HTML itself.
        if path:
            try:
                size = os.path.getsize(path)
            except OSError:
                return
        elif isinstance(result, str):
            size = len(result)
        else:
            return
        if size and size > _LARGE_HTML:
            warnings.warn(
                f"HTML output is {size / 1e6:.0f} MB because page images are embedded inline "
                f"(image_mode='embed'). Pass image_mode='external' (writes a sibling img/ "
                f"folder) or image_mode='drop' for a much smaller file.",
                stacklevel=3,
            )

    # -- outputs (OCR-aware) -------------------------------------------------
    # Signatures mirror the Rust core's so existing calls are unchanged; the only additions
    # are `ocr=True` (auto-run OCR first if it hasn't been) and, otherwise, a warning about
    # scanned pages that haven't been OCR'd. OCR-augmented output is inherently page-scoped.
    def to_html(self, path: Optional[str] = None, return_string: bool = False,
                mode: str = "section", toc: bool = True, image_mode: str = "embed",
                ocr: bool = False, backend: Optional["_ocr.OcrBackend"] = None):
        """Render to HTML. Pass ``ocr=True`` to OCR any scanned pages first (runs ``run_ocr``
        once if it hasn't been), optionally with a specific ``backend``. When the document has
        OCR results, scanned pages are rendered from the recovered text (page mode); otherwise
        this is the core extractor verbatim and un-OCR'd scanned pages trigger a warning."""
        if ocr and not self._pdf.has_ocr():
            self.run_ocr(backend)
        self._warn_pending()
        if self._pdf.has_ocr():
            result = _ocr.to_html(self._pdf, path=path, return_string=return_string, image_mode=image_mode)
        else:
            result = self._pdf.to_html(path, return_string, mode, toc, image_mode)
        self._warn_large_embed(path, result, image_mode)
        return result

    def to_markdown(self, path: Optional[str] = None, return_string: bool = False,
                    mode: str = "section", toc: bool = True, image_mode: str = "external",
                    ocr: bool = False, backend: Optional["_ocr.OcrBackend"] = None):
        """Render to Markdown. ``ocr=True`` OCRs scanned pages first (optionally with a given
        ``backend``); otherwise OCR-augmented only if ``run_ocr`` was already called."""
        if ocr and not self._pdf.has_ocr():
            self.run_ocr(backend)
        self._warn_pending()
        if self._pdf.has_ocr():
            return _ocr.to_markdown(self._pdf, path=path, return_string=return_string, toc=toc, image_mode=image_mode)
        return self._pdf.to_markdown(path, return_string, mode, toc, image_mode)

    def to_pdf(self, path: str, remove_raster: bool = False, ocr: bool = False,
               backend: Optional["_ocr.OcrBackend"] = None) -> str:
        """Write a searchable PDF. ``ocr=True`` OCRs scanned pages first (optionally with a
        given ``backend``). By default the original scan is kept and an invisible selectable
        text layer is added; ``remove_raster=True`` reflows to clean text and drops the raster.
        Warns if no pages have been OCR'd (the result would otherwise just be the original)."""
        if ocr and not self._pdf.has_ocr():
            self.run_ocr(backend)
        if not self._pdf.has_ocr():
            self._warn_pending()
        return self._pdf.to_pdf(path, None, remove_raster=remove_raster)
