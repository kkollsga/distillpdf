"""The public ``Doc`` — a loaded ``.dpdf`` document model.

``distillpdf.load("case.dpdf")`` returns a :class:`Doc`: the agent-and-human reading surface
over a distilled document (Tier 2 of docs/datamodel-design.md). One document, no corpus
machinery — list its sections, read one as markdown, search it with a coverage-honest
``find``, inspect its OCR state, and re-render the *fidelity* HTML / Markdown / text views
straight from the file.

``Doc`` is a **thin wrapper over** :class:`distillpdf.dpdf.Model` (the internal reader the
CLI shell uses). That is deliberate: reading logic lives in one place, so the Python API and
the ``distillpdf <file.dpdf> …`` CLI cannot drift. ``Doc.section(id)`` returns exactly what
``distillpdf <file>.dpdf read <id>`` prints; ``Doc.find(...)`` is the same search the
``find`` verb runs.

Two markdown views, named honestly (same split the CLI documents):

* :meth:`Doc.section` / the ``read`` verb are the **QUERY** view — block slices emitted as
  simple markdown, addressable per section / block / page.
* :meth:`Doc.to_markdown` (and :meth:`Doc.to_html` / :meth:`Doc.text`) are the **FIDELITY**
  view — the whole-document model-only re-render, byte-identical to ``distillpdf.render_*``
  and to the source PDF's ``to_markdown(image_mode="drop")``. These delegate to the Rust
  renderers, which are pure functions of the model.
"""
from __future__ import annotations

import os
from typing import Any, Optional

from .dpdf import DpdfError, FindResult, Model


class Doc:
    """A loaded ``.dpdf`` document model — the single-document reading surface.

    Construct with :func:`distillpdf.load` (the common case) or :meth:`Doc.load`. ``Doc`` is
    immutable: a ``.dpdf`` is a snapshot of extractor quality at distill time (re-distill the
    source PDF to upgrade). Everything written *after* distillation — annotations, cross-
    document search, embeddings — is the corpus layer's job, not this one's.

    The model is **experimental** (``schema_version == 0``) until the first downstream cutover
    proves the shape; check :attr:`schema_version` if you persist anything keyed on it.
    """

    def __init__(self, model: Model, path: Optional[str] = None):
        # `path` is retained so the fidelity renders (which the Rust side reads from the
        # container on disk) can be served without a source PDF. None when built from a dict
        # in-memory — the fidelity renders then raise an actionable error.
        self._model = model
        self._path = path

    # -- constructors --------------------------------------------------------
    @classmethod
    def load(cls, path: str) -> "Doc":
        """Load a ``.dpdf`` from disk. Raises :class:`~distillpdf.dpdf.DpdfError` with an
        actionable message when ``path`` is not a readable ``.dpdf`` container (e.g. a PDF or
        an HTML file passed by mistake)."""
        return cls(Model.load(path), path=path)

    @classmethod
    def from_model(cls, model: Model, path: Optional[str] = None) -> "Doc":
        """Wrap an already-parsed :class:`~distillpdf.dpdf.Model` (tests, in-memory pipelines).
        Pass ``path`` to keep the fidelity renders working; without it they raise."""
        return cls(model, path=path)

    def __repr__(self) -> str:
        src = self.source.get("file", "?")
        return (f"<distillpdf.Doc {src!r} pages={len(self.pages)} "
                f"sections={len(self.sections)} blocks={len(self._model.blocks)} "
                f"schema_v{self.schema_version}>")

    # -- raw escape hatch ----------------------------------------------------
    @property
    def model(self) -> Model:
        """The underlying :class:`~distillpdf.dpdf.Model`. The accessors below cover the
        documented surface; reach for this only for ad-hoc traversal of the parsed model."""
        return self._model

    # -- top-level metadata --------------------------------------------------
    @property
    def schema_version(self) -> int:
        """The model schema version. ``0`` is experimental (see the class docstring)."""
        return self._model.schema_version

    @property
    def source(self) -> dict[str, Any]:
        """The source binding: ``{file, sha256, pages, distillpdf, generated_at}`` — which PDF
        this model was distilled from, by which distillpdf version, when."""
        return self._model.source

    @property
    def metadata(self) -> dict[str, Any]:
        """Parsed front matter: ``{title, authors, abstract, keywords, …}`` (fields the
        extractor found; empty when none)."""
        return self._model.metadata

    # -- pages / sections ----------------------------------------------------
    @property
    def pages(self) -> list[dict[str, Any]]:
        """Per-page records: ``n`` (1-based), ``width_pts`` / ``height_pts``, ``labels`` (the
        ``/PageLabels`` map, e.g. ``{"pdf": "xii"}``), and OCR fields when present."""
        return self._model.pages

    @property
    def sections(self) -> list[dict[str, Any]]:
        """The flat section list in document order: each ``{id, level, title, parent,
        page_start, page_end}``. The id threads straight into :meth:`section` / :meth:`find`."""
        return self._model.sections

    def toc(self) -> list[tuple[int, str, int, str]]:
        """The section tree as ``(level, title, page_start, id)`` tuples — the ``.dpdf``
        analogue of :meth:`distillpdf.Pdf.toc`, but the trailing element is the **section id**
        (not an HTML anchor), because that is what reads back through :meth:`section`. The
        ``toc`` CLI verb prints the same rows."""
        return [
            (int(s.get("level", 1)), s.get("title", ""),
             int(s.get("page_start") or 0), s["id"])
            for s in self.sections
        ]

    def page_label(self, n: int, scheme: str = "pdf") -> Optional[str]:
        """The label physical page ``n`` carries under ``scheme`` (default the ``/PageLabels``
        ``"pdf"`` label, e.g. ``"xii"``), or ``None`` when the page has no such label."""
        return self._model.page_label(n, scheme)

    def resolve_pages(self, spec: str) -> list[int]:
        """Resolve a page spec (``"5"``, ``"3-7"``, a label like ``"xii"`` or a label range
        ``"xii-xv"``) to physical page numbers. Raises :class:`~distillpdf.dpdf.DpdfError` on
        an unresolvable token — a page that isn't here is a loud error, never a silent empty."""
        return self._model.resolve_page_range(spec)

    # -- block / section reading (QUERY view) --------------------------------
    def section(self, sid: str) -> str:
        """The section (and its whole subtree) as **query-markdown** — byte-identical to what
        ``distillpdf <file>.dpdf read <sid>`` prints as its body. Headings by level,
        paragraphs, list items, tables as pipe-form, figure/caption placeholders. Raises
        :class:`~distillpdf.dpdf.DpdfError` (with the closest section ids) on an unknown id."""
        if self._model.section_by_id(sid) is None:
            close = self._model.closest_section_ids(sid)
            hint = f" closest section ids: {', '.join(close)}" if close else ""
            raise DpdfError(f"unknown section {sid!r}.{hint}")
        return self._model.section_markdown(sid)

    def blocks(self, kind: Optional[str] = None, section: Optional[str] = None,
               pages: Optional[list[int]] = None) -> list[dict[str, Any]]:
        """The blocks (reading-order units — the source of truth) matching every filter given:
        ``kind`` (``"para"`` / ``"heading"`` / ``"table"`` / ``"figure"`` / …), ``section`` (a
        section id; includes its subtree), ``pages`` (physical page numbers). No filter → every
        block. Each block is the raw model dict (``id``, ``kind``, ``text``, ``page``,
        ``section``, ``confidence``, ``ocr_pass``, …)."""
        if section is not None:
            if self._model.section_by_id(section) is None:
                close = self._model.closest_section_ids(section)
                hint = f" closest section ids: {', '.join(close)}" if close else ""
                raise DpdfError(f"unknown section {section!r}.{hint}")
            out = self._model.blocks_for_section(section)
        else:
            out = list(self._model.blocks)
        if pages is not None:
            want = set(pages)
            out = [b for b in out if b.get("page") in want]
        if kind is not None:
            out = [b for b in out if b.get("kind") == kind]
        return out

    def block(self, bid: str) -> dict[str, Any]:
        """One block by id. Raises :class:`~distillpdf.dpdf.DpdfError` on an unknown id."""
        b = self._model.block_by_id(bid)
        if b is None:
            raise DpdfError(f"unknown block id {bid!r}")
        return b

    def block_markdown(self, bid: str) -> str:
        """A single block as query-markdown (what ``read <block-id>`` prints) — convenience over
        ``blocks_markdown([block(bid)])``."""
        return self._model.blocks_markdown([self.block(bid)])

    # -- kind indexes --------------------------------------------------------
    def tables(self) -> list[dict[str, Any]]:
        """The table index: one ``{id, label, page, page_label, caption}`` per table, in
        document order. The id reads back via :meth:`block` / :meth:`block_markdown`. Same rows
        as the ``tables`` CLI verb."""
        return self._kind_index("table")

    def figures(self) -> list[dict[str, Any]]:
        """The figure index: one ``{id, label, page, page_label, caption}`` per figure. Same
        rows as the ``figures`` CLI verb."""
        return self._kind_index("figure")

    def _kind_index(self, kind: str) -> list[dict[str, Any]]:
        out: list[dict[str, Any]] = []
        for e in self._model.indexes.get("kinds", {}).get(kind, []):
            b = self._model.block_by_id(e["id"]) or {}
            caption = (b.get("caption") or "").splitlines()
            page = e.get("page")
            out.append({
                "id": e["id"],
                "label": e.get("label"),
                "page": page,
                "page_label": self._model.page_label(page) if page else None,
                "caption": caption[0] if caption else None,
            })
        return out

    # -- find (coverage-honest search) ---------------------------------------
    def find(self, query: str, *, regex: bool = False, section: Optional[str] = None,
             kind: Optional[str] = None, pages: Optional[list[int]] = None,
             limit: Optional[int] = None) -> FindResult:
        """Lexical search over block text — case-insensitive substring by default, ``regex=True``
        for a regex. Scope with ``section`` / ``kind`` / ``pages``. Returns a
        :class:`~distillpdf.dpdf.FindResult` carrying the hits **and** the coverage accounting
        (``searched_blocks``, ``searched_pages``, ``no_text_pages``) so "not found" is a
        measured statement, never a silent one. This is the same search the ``find`` verb runs.

        ``limit`` truncates the returned ``hits`` list (the coverage counts still reflect the
        full search); ``len(result.hits)`` after truncation vs the pre-truncation total is
        visible because the slice is applied here, not inside the search — pass ``limit=None``
        (the default) for every hit."""
        if section is not None and self._model.section_by_id(section) is None:
            close = self._model.closest_section_ids(section)
            hint = f" closest section ids: {', '.join(close)}" if close else ""
            raise DpdfError(f"unknown section {section!r}.{hint}")
        res = self._model.find(query, regex=regex, section=section, kind=kind, pages=pages)
        if limit is not None:
            res = FindResult(
                hits=res.hits[:limit],
                searched_blocks=res.searched_blocks,
                searched_pages=res.searched_pages,
                no_text_pages=res.no_text_pages,
            )
        return res

    # -- info ----------------------------------------------------------------
    def info(self) -> dict[str, Any]:
        """A compact roll-up of the model — the ``info`` CLI verb as data: source binding,
        counts (pages / sections / blocks / tables / figures / footnotes), OCR state, the asset
        storage profile, and index coverage. One call to size up a document."""
        from .shell import _asset_profile, _ocr_state_summary  # one source of truth with the CLI

        m = self._model
        kinds = m.indexes.get("kinds", {})
        src = m.source
        return {
            "schema_version": m.schema_version,
            "source": {
                "file": src.get("file"),
                "sha256_prefix": (src.get("sha256") or "")[:12],
                "distillpdf": src.get("distillpdf"),
                "generated_at": src.get("generated_at"),
            },
            "pages": len(m.pages),
            "sections": len(m.sections),
            "blocks": len(m.blocks),
            "tables": len(kinds.get("table", [])),
            "figures": len(kinds.get("figure", [])),
            "footnotes": len(kinds.get("footnote", [])),
            "ocr": _ocr_state_summary(m),
            "assets": _asset_profile(m),
            "coverage": m.indexes.get("coverage", {}),
        }

    # -- OCR -----------------------------------------------------------------
    def ocr_passes(self) -> list[dict[str, Any]]:
        """The recorded OCR passes (append-only history): each ``{id, engine, params, results,
        …}``. Empty for a born-digital model (no scanned pages OCR'd)."""
        return self._model.ocr_passes

    def ocr_status(self) -> dict[str, Any]:
        """Per-page OCR decisions and per-pass outcomes — the ``ocr-status`` CLI verb as data:
        ``{"pages": [{page, decision, active_pass}], "passes": [{pass, engine, page, outcome,
        legible_chars, confidence}]}``. Both lists empty for a born-digital model."""
        m = self._model
        pages = [
            {"page": p.get("n"), "decision": p.get("ocr_decision"),
             "active_pass": p.get("active_ocr_pass")}
            for p in m.pages
            if p.get("ocr_decision") or p.get("active_ocr_pass")
        ]
        passes = []
        for ocp in m.ocr_passes:
            for r in ocp.get("results", []):
                passes.append({
                    "pass": ocp.get("id"), "engine": ocp.get("engine"),
                    "page": r.get("page"), "outcome": r.get("outcome"),
                    "legible_chars": r.get("legible_chars"), "confidence": r.get("confidence"),
                })
        return {"pages": pages, "passes": passes}

    # -- fidelity renders (model-only, pure functions of the model) ----------
    def _require_path(self, what: str) -> str:
        if self._path is None:
            raise DpdfError(
                f"{what} re-renders from the .dpdf container on disk, but this Doc was built "
                "in memory (no path). Load it with distillpdf.load('file.dpdf') instead.")
        return self._path

    def _derive_out(self, path: Optional[str], ext: str) -> str:
        """Where a fidelity render writes when ``path`` is None or a directory: ``<stem>.<ext>``
        beside the .dpdf (mirrors ``to_html``'s name derivation)."""
        src = self._require_path("write")
        stem = os.path.splitext(os.path.basename(src))[0]
        if path is None:
            return os.path.join(os.path.dirname(src), stem + "." + ext)
        if os.path.isdir(path) or path.endswith(os.sep):
            os.makedirs(path, exist_ok=True)
            return os.path.join(path, stem + "." + ext)
        return path

    def to_html(self, path: Optional[str] = None, return_string: bool = False, *,
                mode: str = "section", toc: bool = True) -> str:
        """Re-render the **fidelity** HTML from the model — byte-identical to
        :func:`distillpdf.render_html` and to the source PDF's
        ``to_html(image_mode="drop")``. ``return_string=True`` returns the HTML and writes
        nothing; otherwise it writes a file (``path``, or ``<stem>.html`` beside the .dpdf) and
        returns the path. ``mode`` (``"section"`` / ``"page"``) and ``toc`` match
        :meth:`distillpdf.Pdf.to_html`. Figures render as drop-placeholders — the born-digital
        model carries no embedded figure bytes (a regenerable stub)."""
        from . import render_html
        html = render_html(self._require_path("to_html"), mode, toc)
        if return_string:
            return html
        dest = self._derive_out(path, "html")
        with open(dest, "w", encoding="utf-8") as f:
            f.write(html)
        return dest

    def to_markdown(self, path: Optional[str] = None, return_string: bool = False, *,
                    mode: str = "section", toc: bool = True, image_mode: str = "external") -> str:
        """Re-render the **fidelity** Markdown from the model — byte-identical to
        :func:`distillpdf.render_markdown` (the HTML→Markdown transform over the model-only
        HTML). ``return_string=True`` returns the string; otherwise writes ``<stem>.md`` (or
        ``path``) and returns the path. ``image_mode`` matches :meth:`distillpdf.Pdf.to_markdown`
        (the model has no figure bytes, so ``"external"`` degrades to caption placeholders)."""
        from . import render_markdown
        md = render_markdown(self._require_path("to_markdown"), mode, toc, image_mode)
        if return_string:
            return md
        dest = self._derive_out(path, "md")
        with open(dest, "w", encoding="utf-8") as f:
            f.write(md)
        return dest

    def text(self) -> str:
        """The document's plain text in reading order (one page per line) — byte-identical to
        :func:`distillpdf.render_text`, the model-only analogue of
        :meth:`distillpdf.Pdf.extract_text`."""
        from . import render_text
        return render_text(self._require_path("text"))
