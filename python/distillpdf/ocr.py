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
    languages:  ISO codes an engine should read, e.g. ["eng", "por"] (Tesseract). Empty =
                the engine's default. Ignored by engines that are language-agnostic.
    dpi:        render-resolution hint; None = engine default. Ignored where irrelevant.

    Fields an engine doesn't use are simply ignored, so this stays one shared dataclass
    across every backend.
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
    languages: List[str] = field(default_factory=list)
    dpi: Optional[int] = None


@dataclass(frozen=True)
class OcrCapabilities:
    """What an engine is and what it can do — read straight off the backend class, so it
    can be queried (for defaulting, the CLI, ``--help``) WITHOUT importing the engine's
    heavy dependencies or downloading any model."""

    name: str
    tier: str                    # "fast" | "accurate"
    structure_aware: bool        # emits tables (OTSL) / tagged headings, vs flat bare-dialect text
    bundled: bool                # works on a base `pip install distillpdf` — no extra, no download
    offline: bool                # no network / model download at use time
    languages: tuple             # ISO codes it can read; () = unspecified / any
    available: bool              # can it actually run in THIS install right now?
    output: str = "doctags"      # the immutable wire contract
    detail: str = ""             # one-line human description


class OcrBackend:
    """Abstract OCR backend: a page image → DocTags string.

    The class-level capability attributes (``tier``/``structure_aware``/``bundled``/
    ``offline``/``languages``) describe the engine and are deliberately cheap to read —
    inspecting them never triggers the engine's heavy imports (those load lazily inside
    ``ocr_page``). ``structure_aware`` is advisory metadata for defaulting/docs only; it is
    NOT a pipeline switch — a flat-text engine simply emits the bare DocTags dialect the
    Rust parser already handles."""

    #: human-facing name used in the registry and error messages
    name: str = "abstract"
    #: the model output dialect the Rust core should expect ("doctags")
    output: str = "doctags"

    # -- capability metadata (class-level, cheap to read) --------------------
    tier: str = "accurate"
    structure_aware: bool = True
    bundled: bool = False
    offline: bool = False
    languages: tuple = ()
    detail: str = ""

    @classmethod
    def is_available(cls) -> bool:
        """Whether this engine can actually run in the current install — checked
        import-light (no heavy import, no download). Default True; backends with optional
        deps override (e.g. via ``importlib.util.find_spec``) or native engines query the
        compiled-in feature set."""
        return True

    @classmethod
    def descriptor(cls) -> "OcrCapabilities":
        return OcrCapabilities(
            name=cls.name, tier=cls.tier, structure_aware=cls.structure_aware,
            bundled=cls.bundled, offline=cls.offline, languages=tuple(cls.languages),
            available=cls.is_available(), output=cls.output, detail=cls.detail,
        )

    def __init__(self, config: Optional[OcrConfig] = None, **kwargs):
        self.config = config or OcrConfig(**kwargs)

    def ocr_page(self, image: bytes) -> str:
        """Return the model's DocTags for one page image (PNG/JPEG bytes)."""
        raise NotImplementedError

    def prepare(self, samples: "List[bytes]") -> None:
        """One-time setup before the per-page loop, given a few sample page images. Default
        no-op; the bundled Tesseract backend uses it to auto-detect the document language."""

    def close(self) -> None:
        """Release any held resources (model handle, server process)."""

    def __enter__(self):
        return self

    def __exit__(self, *exc):
        self.close()


# ---- registry --------------------------------------------------------------

#: name → factory. Backends self-register here. Engines (granite VLM, the bundled
#: Tesseract, a bring-your-own server, …) plug in without touching callers.
_BACKENDS: Dict[str, Callable[..., OcrBackend]] = {}
#: name → backend CLASS, kept so capabilities can be read without instantiating. For all
#: current backends the factory *is* the class; when it isn't, pass ``descriptor_cls``.
_BACKENDS_DESC: Dict[str, type] = {}


def register_backend(name: str, factory: Callable[..., OcrBackend],
                     descriptor_cls: Optional[type] = None) -> None:
    """Register an engine backend under ``name``. ``descriptor_cls`` is the class whose
    capability attributes describe the engine (defaults to ``factory`` when it is itself a
    class, which is the common case)."""
    _BACKENDS[name] = factory
    cls = descriptor_cls or (factory if isinstance(factory, type) else None)
    if cls is not None:
        _BACKENDS_DESC[name] = cls


def available_backends() -> list[str]:
    return sorted(_BACKENDS)


def backend_descriptors() -> "list[OcrCapabilities]":
    """Capability descriptors for every registered backend — import-light (reads class
    attributes only; never instantiates a backend or imports its heavy deps). Drives the
    CLI ``--list-ocr-engines`` and tier defaulting."""
    return [_BACKENDS_DESC[n].descriptor() for n in available_backends() if n in _BACKENDS_DESC]


# ---- selection -------------------------------------------------------------

def _accurate_backend_name() -> str:
    """The platform's granite-docling backend: native MLX on Apple Silicon (Metal, no
    PyTorch), else the GGUF backend via llama-cpp-python (Win/Linux/Intel-Mac, no PyTorch)."""
    import platform

    if platform.system() == "Darwin" and platform.machine() == "arm64":
        return "granite-docling"  # MLX
    return "granite-docling-gguf"  # llama.cpp GGUF


def _fast_backend_name() -> Optional[str]:
    """The bundled fast engine (Tesseract) if it's compiled into this wheel, else None."""
    return "tesseract" if "tesseract" in _BACKENDS else None


def default_backend_name(tier: Optional[str] = None) -> str:
    """Resolve a backend name for a tier. ``tier="fast"`` (the default) picks the bundled
    Tesseract engine when it's compiled in, otherwise gracefully falls back to the platform
    granite backend — so a wheel built without the fast engine behaves exactly as before, and
    the default flips to fast automatically the day that engine ships. ``tier="accurate"``
    always picks granite-docling."""
    tier = tier or "fast"
    if tier == "fast":
        return _fast_backend_name() or _accurate_backend_name()
    if tier == "accurate":
        return _accurate_backend_name()
    raise ValueError(f"unknown OCR tier {tier!r}; expected 'fast' or 'accurate'")


def get_backend(name: Optional[str] = None, *, tier: Optional[str] = None, **kwargs) -> OcrBackend:
    """Construct a backend. Precedence: explicit ``name`` > ``tier`` > the platform default
    (fast tier). Existing names (``granite-docling``, ``granite-docling-gguf``) resolve as
    before. Raises OcrDependencyError when the chosen engine's optional deps are missing,
    with the exact install command."""
    if name is None:
        name = default_backend_name(tier)
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


def _auto_progress(total: int):
    """Default page-progress: a tqdm bar on an interactive terminal, else nothing.

    Returns ``(callback, closer)``. The bar starts labelled "loading model" so the silent
    first-call cost (model download + load, which happens inside the first ``ocr_page``) is
    visible rather than dead air; it switches to "OCR" once the first page returns. On a
    non-TTY (pipes, CI, notebooks without a live stderr) or when tqdm is unavailable this is
    a no-op, so piped/automated use stays silent and callers passing their own ``progress``
    are unaffected."""
    import sys

    if total <= 0 or not sys.stderr.isatty():
        return None, None
    try:
        from tqdm.auto import tqdm
    except ImportError:  # tqdm ships with huggingface-hub, but degrade gracefully
        return None, None

    bar = tqdm(total=total, desc="OCR (loading model)", unit="page")

    def cb(page: int, done: int, total: int) -> None:
        if done == 1:
            bar.set_description("OCR")
        bar.update(1)

    return cb, bar.close


def _doctags_for(pdf, backend: OcrBackend, only: Optional[set] = None,
                 progress: Optional[Callable[[int, int, int], None]] = None) -> Dict[int, str]:
    """Run `backend` on every page the Rust core flags for OCR; return {page: DocTags}.

    `progress(page, done, total)` is called after each page. If `progress` is None (the
    default) a tqdm bar is shown automatically on an interactive terminal; pass
    ``progress=False`` to force silence, or your own callable to handle it yourself."""
    plan = [it for it in pdf.ocr_plan()
            if it["needs_ocr"] and it["image"] and (only is None or it["page"] in only)]
    total = len(plan)
    out: Dict[int, str] = {}
    # One-time backend setup on a sample (e.g. language auto-detection) before the loop.
    backend.prepare([bytes(it["image"]) for it in plan[:3]])
    closer = None
    if progress is None:
        progress, closer = _auto_progress(total)
    try:
        for i, item in enumerate(plan, 1):
            out[item["page"]] = backend.ocr_page(bytes(item["image"]))
            if progress:
                progress(item["page"], i, total)
    finally:
        if closer:
            closer()
    return out


def run(pdf, backend: Optional[OcrBackend] = None, *, only: Optional[set] = None,
        progress: Optional[Callable[[int, int, int], None]] = None) -> Dict[int, str]:
    """OCR every scanned page of `pdf` **once** and cache the result on the `pdf` object.
    After this, ``ocr.to_pdf`` / ``ocr.to_html`` / ``ocr.to_markdown`` reuse the cached text
    — the model never re-runs. `backend` defaults to the bundled granite-docling backend
    (the model is downloaded on first use). On an interactive terminal a tqdm progress bar is
    shown by default; pass ``progress=False`` to silence it or your own ``progress(page, done,
    total)`` callable. Returns the ``{page: DocTags}`` map."""
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
# NOT import their heavy dependencies). Native (Rust-compiled) engines register FIRST so
# the fast tier can pick them as the default when they're built into the wheel.
from . import _backends_native  # noqa: E402,F401  (side-effect: registration — bundled Tesseract / server)
from . import _backends_mlx  # noqa: E402,F401  (side-effect: registration — Apple Silicon, MLX)
from . import _backends_granite  # noqa: E402,F401  (side-effect: registration — Win/Linux/Intel-Mac, GGUF)
from . import _backends_pytorch  # noqa: E402,F401  (side-effect: registration — optional PyTorch/vLLM accelerator)
