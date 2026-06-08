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

from dataclasses import dataclass, field
from typing import Callable, Dict, List, Optional


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
    max_tokens: generation cap per page (a backstop — stop_strings normally terminate).
    stop_strings: strings that end generation (e.g. the DocTags end marker).
    """

    model_id: Optional[str] = None
    model_dir: Optional[str] = None
    hf_token: Optional[str] = None
    device: str = "auto"
    prompt: str = "Convert this page to docling."
    # A full page at native resolution fits well under this; the DocTags end marker
    # (stop_strings) is the real terminator, so this is just a runaway backstop.
    max_tokens: int = 4096
    stop_strings: List[str] = field(default_factory=list)


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


def default_backend_name() -> str:
    """The OCR backend for this platform: MLX granite-docling on Apple Silicon, otherwise
    the (currently placeholder) PyTorch/vLLM backend for Windows/Linux."""
    import platform

    if platform.system() == "Darwin" and platform.machine() == "arm64":
        return "granite-docling"  # MLX, no PyTorch
    return "granite-docling-pytorch"


def get_backend(name: Optional[str] = None, **kwargs) -> OcrBackend:
    """Construct a backend by name (default: the best one for this platform). Raises
    OcrDependencyError if its deps are missing, with the exact install command."""
    if name is None:
        name = default_backend_name()
    try:
        factory = _BACKENDS[name]
    except KeyError:
        raise ValueError(
            f"unknown OCR backend {name!r}; available: {available_backends() or ['(none registered)']}"
        ) from None
    return factory(**kwargs)


# ---- high-level orchestration ----------------------------------------------

import re as _re

# Pure-Rust converters (no model needed): render DocTags — e.g. a model's raw output —
# straight to distillPDF HTML.  `render_doctags(dt)` is one page; `render_doctags_document`
# joins a list of pages into a full HTML document.
try:  # the compiled module is always present in a real install
    from ._distillpdf import ocr_doctags_doc_html as render_doctags_document  # noqa: F401
    from ._distillpdf import ocr_doctags_to_html as render_doctags  # noqa: F401
except ImportError:  # pragma: no cover
    pass


def _doctags_for(pdf, backend: OcrBackend, only: Optional[set] = None,
                 progress: Optional[Callable[[int, int, int], None]] = None) -> Dict[int, str]:
    """Run `backend` on every page the Rust core flags for OCR; return {page: DocTags}.

    `progress(page, done, total)` is called after each page if given."""
    plan = [it for it in pdf.ocr_plan()
            if it["needs_ocr"] and it["image"] and (only is None or it["page"] in only)]
    total = len(plan)
    out: Dict[int, str] = {}
    for i, item in enumerate(plan, 1):
        out[item["page"]] = backend.ocr_page(bytes(item["image"]))
        if progress:
            progress(item["page"], i, total)
    return out


def run(pdf, backend: Optional[OcrBackend] = None, *, only: Optional[set] = None,
        progress: Optional[Callable[[int, int, int], None]] = None) -> Dict[int, str]:
    """OCR every scanned page of `pdf` **once** and cache the result on the `pdf` object.
    After this, ``ocr.to_pdf`` / ``ocr.to_html`` / ``ocr.to_markdown`` reuse the cached text
    — the model never re-runs. `backend` defaults to the bundled granite-docling backend
    (the model is downloaded on first use). Returns the ``{page: DocTags}`` map."""
    if backend is None:
        backend = get_backend()
    doctags = _doctags_for(pdf, backend, only=only, progress=progress)
    pdf.set_ocr(doctags)
    return doctags


def _resolve_doctags(pdf, backend: Optional[OcrBackend]) -> Dict[int, str]:
    """The cached OCR results, running `backend` once (and caching) if none are cached yet."""
    if pdf.has_ocr():
        return pdf.get_ocr()
    if backend is None:
        raise ValueError(
            "no cached OCR results on this pdf; call distillpdf.ocr.run(pdf, backend) first, "
            "or pass a backend."
        )
    return run(pdf, backend)


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


def _augmented_html(pdf, backend: Optional[OcrBackend], image_mode: str) -> str:
    """The page-mode HTML with OCR fragments spliced in, using cached OCR (running the
    backend once if nothing is cached yet)."""
    from ._distillpdf import ocr_doctags_to_html

    doctags = _resolve_doctags(pdf, backend)
    fragments = {p: ocr_doctags_to_html(dt) for p, dt in doctags.items()}
    base = pdf.to_html(mode="page", return_string=True, image_mode=image_mode)
    return _splice(base, fragments) if fragments else base


def to_html(pdf, backend: Optional[OcrBackend] = None, *, path: Optional[str] = None,
            return_string: bool = True, image_mode: str = "embed") -> str:
    """OCR-augmented HTML: image-only/scanned pages are rendered from the model's DocTags;
    born-digital pages keep distillPDF's normal extraction. Pages are spliced into the
    page-mode document so structure stays consistent. Reuses OCR cached on `pdf` (via
    ``ocr.run``); if none is cached, runs `backend` once and caches it."""
    html = _augmented_html(pdf, backend, image_mode)
    if path:
        with builtins_open(path, "w", encoding="utf-8") as f:
            f.write(html)
        return path
    return html


def to_markdown(pdf, backend: Optional[OcrBackend] = None, *, path: Optional[str] = None,
                return_string: bool = True, toc: bool = True, image_mode: str = "drop") -> str:
    """OCR-augmented Markdown, derived from the *same* OCR HTML as ``to_html`` — no extra
    model pass. Reuses OCR cached on `pdf`; runs `backend` once if nothing is cached."""
    from ._distillpdf import html_to_markdown

    # Markdown can't inline figures usefully; "external" writes them next to the .md.
    html = _augmented_html(pdf, backend, "embed" if image_mode == "embed" else "drop")
    md, files = html_to_markdown(html, toc, image_mode)
    if path:
        with builtins_open(path, "w", encoding="utf-8") as f:
            f.write(md)
        if files:
            import os
            base = os.path.dirname(path) or "."
            for rel, data in files:
                fp = os.path.join(base, rel)
                os.makedirs(os.path.dirname(fp), exist_ok=True)
                with builtins_open(fp, "wb") as f:
                    f.write(bytes(data))
        return path
    return md


def to_pdf(pdf, backend: Optional[OcrBackend] = None, path: str = None,
           *, remove_raster: bool = False) -> str:
    """Write a searchable PDF from the OCR results. By default (``remove_raster=False``) the
    original scan is KEPT and the OCR text is added as an invisible, selectable layer over it
    — OCR errors never destroy content (best for archival/legal use). With
    ``remove_raster=True`` the OCR'd pages are reflowed to visible text + cropped figures and
    the raster is dropped (much smaller file). Reuses OCR cached on `pdf` (via ``ocr.run``);
    if none is cached, runs `backend` once and caches it."""
    if path is None:
        raise ValueError("to_pdf requires an output path")
    pdf.to_pdf(path, _resolve_doctags(pdf, backend), remove_raster=remove_raster)
    return path


# `open` is shadowed by distillpdf.open at the package root; keep the builtin handy.
import builtins as _builtins  # noqa: E402

builtins_open = _builtins.open

# Built-in backends register themselves on import (lazily — importing this module does
# NOT import their heavy dependencies).
from . import _backends_mlx  # noqa: E402,F401  (side-effect: registration — Apple Silicon)
from . import _backends_pytorch  # noqa: E402,F401  (side-effect: registration — Win/Linux placeholder)
