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


# Built-in backends register themselves on import (lazily — importing this module does
# NOT import their heavy dependencies).
from . import _backends_granite  # noqa: E402,F401  (side-effect: registration)
