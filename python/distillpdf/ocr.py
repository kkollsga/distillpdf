"""OCR backends for distillpdf.

distillpdf's core (detection, DocTags parsing, HTML/searchable-PDF rendering) is pure
Rust and always available. *Running the OCR model* is optional and pulls in larger
dependencies, installed via:

    pip install 'distillpdf[ocr]'

A backend is a thin, standardized wrapper around a model: it handles model download /
caching (``model_dir``) and authentication (``hf_token``), and turns a page image into
the model's DocTags string. The Rust core then renders those DocTags. New backends
(other libraries, or models with stronger table support) register under a name and the
rest of the pipeline is unchanged.

This module is always importable; the heavy dependencies are imported lazily inside a
backend, so a base install gives a precise, actionable error the moment OCR is used.
"""
from __future__ import annotations

from dataclasses import dataclass
from typing import Callable, Dict, Optional


class OcrDependencyError(ImportError):
    """Raised when an OCR backend's optional dependencies are not installed."""


def _require(module: str, *, package: Optional[str] = None):
    """Import an optional dependency or raise a clear, actionable error."""
    try:
        return __import__(module)
    except ImportError as e:  # pragma: no cover - exercised via backends
        raise OcrDependencyError(
            f"distillpdf OCR requires the optional '{package or module}' dependency, "
            f"which is not installed.\n\n"
            f"    pip install 'distillpdf[ocr]'\n\n"
            f"(missing module: {module!r})"
        ) from e


@dataclass
class OcrConfig:
    """Configuration common to every backend.

    model_id:   backend-specific model identifier (e.g. a Hugging Face repo id).
    model_dir:  where model files are downloaded / loaded from. None → the backend's
                default cache (honors HF_HOME / HF_HUB_CACHE for HF-backed models).
    hf_token:   Hugging Face access token for gated/private models (else uses the
                ambient HF_TOKEN / cached login).
    device:     "auto" | "cpu" | "metal" | "cuda" — backend maps as appropriate.
    prompt:     instruction given to the model (DocTags conversion by default).
    max_tokens: generation cap per page.
    """

    model_id: Optional[str] = None
    model_dir: Optional[str] = None
    hf_token: Optional[str] = None
    device: str = "auto"
    prompt: str = "Convert this page to docling."
    max_tokens: int = 4096


class OcrBackend:
    """Abstract OCR backend: a page image → DocTags string."""

    #: human-facing name used in the registry and error messages
    name: str = "abstract"
    #: the model output dialect the Rust core should expect ("doctags")
    output: str = "doctags"

    def __init__(self, config: Optional[OcrConfig] = None, **kwargs):
        self.config = config or OcrConfig(**kwargs)

    def ocr_page(self, image: bytes) -> str:
        """Return the model's DocTags for one page image (PNG/JPEG bytes)."""
        raise NotImplementedError

    def close(self) -> None:
        """Release any held resources (model handle, server process)."""

    def __enter__(self):
        return self

    def __exit__(self, *exc):
        self.close()


# ---- registry --------------------------------------------------------------

#: name → factory. Backends self-register here; today only granite-docling. Future
#: entries (e.g. a table-capable "docling" backend) plug in without touching callers.
_BACKENDS: Dict[str, Callable[..., OcrBackend]] = {}


def register_backend(name: str, factory: Callable[..., OcrBackend]) -> None:
    _BACKENDS[name] = factory


def available_backends() -> list[str]:
    return sorted(_BACKENDS)


def get_backend(name: str = "granite-docling", **kwargs) -> OcrBackend:
    """Construct a backend by name. Raises OcrDependencyError if its deps are missing,
    with the exact install command."""
    try:
        factory = _BACKENDS[name]
    except KeyError:
        raise ValueError(
            f"unknown OCR backend {name!r}; available: {available_backends() or ['(none registered)']}"
        ) from None
    return factory(**kwargs)


# ---- high-level orchestration ----------------------------------------------

import re as _re


def _doctags_for(pdf, backend: OcrBackend, only: Optional[set] = None) -> Dict[int, str]:
    """Run `backend` on every page the Rust core flags for OCR; return {page: DocTags}."""
    out: Dict[int, str] = {}
    for item in pdf.ocr_plan():
        if not item["needs_ocr"] or (only is not None and item["page"] not in only):
            continue
        img = item["image"]
        if not img:
            continue
        out[item["page"]] = backend.ocr_page(bytes(img))
    return out


def _splice(page_html: str, fragments: Dict[int, str]) -> str:
    """Replace the body of each `<section data-page="N">…</section>` whose page has an OCR
    fragment, leaving born-digital pages' sections untouched."""
    def repl(m):
        page = int(m.group(2))
        frag = fragments.get(page)
        return m.group(1) + "\n" + frag + m.group(4) if frag is not None else m.group(0)

    return _re.sub(
        r'(<section data-page="(\d+)"[^>]*>)(.*?)(</section>)',
        repl, page_html, flags=_re.DOTALL,
    )


def to_html(pdf, backend: OcrBackend, *, path: Optional[str] = None,
            return_string: bool = True, image_mode: str = "embed") -> str:
    """OCR-augmented HTML: image-only/scanned pages are rendered from the model's DocTags;
    born-digital pages keep distillPDF's normal extraction. Pages are spliced into the
    page-mode document so structure stays consistent."""
    from ._distillpdf import ocr_doctags_to_html

    fragments = {p: ocr_doctags_to_html(dt) for p, dt in _doctags_for(pdf, backend).items()}
    base = pdf.to_html(mode="page", return_string=True, image_mode=image_mode)
    html = _splice(base, fragments) if fragments else base
    if path:
        with builtins_open(path, "w", encoding="utf-8") as f:
            f.write(html)
        return path
    return html


def to_pdf(pdf, backend: OcrBackend, path: str) -> str:
    """Write a clean, searchable PDF: OCR'd pages are rebuilt as real text + cropped
    figures (raster dropped); born-digital pages are kept verbatim."""
    pdf.to_pdf(path, _doctags_for(pdf, backend))
    return path


# `open` is shadowed by distillpdf.open at the package root; keep the builtin handy.
import builtins as _builtins  # noqa: E402

builtins_open = _builtins.open

# Built-in backends register themselves on import (lazily — importing this module does
# NOT import their heavy dependencies).
from . import _backends_granite  # noqa: E402,F401  (side-effect: registration)
