"""The `.dpdf` model reader — a thin, queryable view over a loaded `model.json`.

The CLI document-shell verbs (`info` / `toc` / `read` / `find` / `tables` / `figures` /
`ocr-status`) operate on a parsed `.dpdf`; this module is the layer between the raw JSON
(`distillpdf.load_model(path)`) and those verbs. Wave 4 reuses it for the public `Doc` API,
so it is kept clean: a :class:`Model` wrapping the parsed dict with the accessors the design
calls for — `section_by_id`, `blocks_for_section`, `blocks_for_pages`, `find` — plus the
small helpers (id threading, page-label resolution, the section-markdown emitter).

Two distinct markdown views, stated honestly:

* :func:`Model.section_markdown` / :func:`Model.blocks_markdown` are the **QUERY** view —
  derived by slicing blocks and emitting simple markdown (headings by level, paragraphs,
  list items, table pipe-form, figure/caption placeholders). It is what `read` shows.
* ``distillpdf.render_markdown(path)`` is the **FIDELITY** view — the model-only re-render
  that reproduces ``to_markdown`` byte-for-byte. It renders whole documents, not sections.

The query view exists because the fidelity renderer is not addressable per-section/-block in
Rust; the two are intentionally different and the CLI documents which it is showing.
"""
from __future__ import annotations

import json
import re
from typing import Any, Iterable, Iterator, Optional

# The default reading-order kinds that carry body prose for the query-markdown view.
_TEXT_KINDS = ("heading", "para", "list_item", "footnote", "caption", "code")

# `block.text` carries the element's MINIMAL INLINE HTML (`<b>/<i>/<a>/<sup>/<sub>/<code>`) —
# the faithful projection of the render IR. The QUERY views (search, query-markdown) want plain
# text, so they strip those inline tags. `_INLINE_TAG` matches any `<...>` run (the text only
# ever carries inline markup, never block tags).
_INLINE_TAG = re.compile(r"<[^>]+>")


def _plain(text: str) -> str:
    """Strip inline markup from a block's `text` for the plain-text query views."""
    if not text or "<" not in text:
        return text
    s = _INLINE_TAG.sub("", text)
    for a, b in (("&amp;", "&"), ("&lt;", "<"), ("&gt;", ">"), ("&quot;", '"'), ("&#39;", "'")):
        s = s.replace(a, b)
    return s


# ---- chunking (DERIVED, like indexes; no text duplication) ------------------
#
# Chunks group consecutive blocks WITHIN ONE SECTION toward a token target, so a semantic
# search hit lands on a coherent passage rather than a bare sentence. They are DERIVED from
# blocks (regenerable, like the indexes) — a chunk stores only block-id addresses + spans, never
# text; the text is recomposed from the blocks at embed/search time. The policy string makes the
# derivation reproducible and lets a staleness check confirm a stored set still matches blocks.

#: Target chunk size in the cheap token proxy (chars/4). ~400 tokens ≈ a paragraph or two —
#: enough context for a dense retriever without diluting the vector.
CHUNK_TARGET_TOKENS = 400
#: The chunking-policy recipe string stored in the model so chunks are regenerable + checkable.
CHUNK_POLICY = "sec-contig-v1:tgt400"


def _token_proxy(text: str) -> int:
    """A cheap token count: chars / 4. Deliberately NOT the real tokenizer — chunking must be
    derivable without loading a 2 GB model. Good enough to bound chunk size; the exact token
    count only matters at embed time, where the tokenizer truncates to 8192 anyway."""
    return max(1, len(text) // 4)


def _chunk_block_text(b: dict[str, Any]) -> str:
    """The plain text a block contributes to a chunk — its searchable text (prose + flattened
    table cells + caption), inline markup stripped. Mirrors `_block_search_text` so chunk text
    and `find` see the same content."""
    return _block_search_text(b).strip()


def derive_chunks(blocks: list[dict[str, Any]]) -> dict[str, Any]:
    """Group ``blocks`` into chunks: a contiguous run within one section accumulates until it
    reaches ~``CHUNK_TARGET_TOKENS`` (chars/4 proxy), then a new chunk starts. A section boundary
    always starts a new chunk (section purity), and a single block larger than the target is its
    own chunk (a block is never split). Returns ``{"policy", "items"}`` where each item is
    ``{id, block_ids, section, page_start, page_end}`` — addresses + spans only, no text.

    Deterministic and regenerable: same blocks → same chunks, so a stored set can be diff'd
    against a fresh derive to detect drift (the staleness signal)."""
    items: list[dict[str, Any]] = []
    cur: list[dict[str, Any]] = []
    cur_tokens = 0
    cur_section: Any = None

    def flush() -> None:
        if not cur:
            return
        pages = [b.get("page") for b in cur if b.get("page") is not None]
        items.append({
            "id": f"c{len(items) + 1:04d}",
            "block_ids": [b["id"] for b in cur],
            "section": cur[0].get("section"),
            "page_start": min(pages) if pages else 0,
            "page_end": max(pages) if pages else 0,
        })

    for b in blocks:
        sec = b.get("section")
        tok = _token_proxy(_chunk_block_text(b))
        # Start a new chunk on a section change, or when adding this block would overflow the
        # target (but never split: a lone oversize block still forms its own chunk).
        if cur and (sec != cur_section or cur_tokens + tok > CHUNK_TARGET_TOKENS):
            flush()
            cur, cur_tokens = [], 0
        cur.append(b)
        cur_tokens += tok
        cur_section = sec
    flush()
    return {"policy": CHUNK_POLICY, "items": items}


class DpdfError(ValueError):
    """A `.dpdf` could not be read as a model (not a container, wrong shape, …)."""


class Model:
    """A parsed `.dpdf` model, with the accessors the document-shell verbs need.

    Construct from a path with :meth:`load` (the common case — it calls the Rust loader) or
    from an already-parsed dict with :meth:`from_dict` (used by tests and Wave 4)."""

    def __init__(self, data: dict[str, Any]):
        self._d = data
        # Index by id once; the shell threads ids between calls, so id → object is hot.
        self._block_by_id = {b["id"]: b for b in self.blocks}
        self._section_by_id = {s["id"]: s for s in self.sections}
        # Block reading-order position, for prev/next breadcrumbs and --from resume.
        self._block_order = {b["id"]: i for i, b in enumerate(self.blocks)}

    # -- constructors --------------------------------------------------------
    @classmethod
    def load(cls, path: str) -> "Model":
        """Load a `.dpdf` from disk via the Rust container loader. Raises :class:`DpdfError`
        with an actionable message when `path` is not a readable `.dpdf`."""
        from . import load_model

        try:
            raw = load_model(path)
        except Exception as e:  # not a zip / missing model.json / bad shape
            raise DpdfError(str(e)) from e
        return cls.from_dict(json.loads(raw))

    @classmethod
    def from_dict(cls, data: dict[str, Any]) -> "Model":
        for key in ("source", "pages", "sections", "blocks", "indexes"):
            if key not in data:
                raise DpdfError(f"not a distillPDF model: missing {key!r}")
        return cls(data)

    # -- top-level accessors -------------------------------------------------
    @property
    def raw(self) -> dict[str, Any]:
        """The underlying parsed model dict — for the write path (``Doc.embed`` builds the next
        model.json from a shallow copy of this). Treat as read-only."""
        return self._d

    @property
    def schema_version(self) -> int:
        return int(self._d.get("schema_version", 0))

    @property
    def source(self) -> dict[str, Any]:
        return self._d.get("source", {})

    @property
    def metadata(self) -> dict[str, Any]:
        return self._d.get("metadata", {})

    @property
    def pages(self) -> list[dict[str, Any]]:
        return self._d.get("pages", [])

    @property
    def sections(self) -> list[dict[str, Any]]:
        return self._d.get("sections", [])

    @property
    def blocks(self) -> list[dict[str, Any]]:
        return self._d.get("blocks", [])

    @property
    def indexes(self) -> dict[str, Any]:
        return self._d.get("indexes", {})

    @property
    def assets(self) -> list[dict[str, Any]]:
        return self._d.get("assets", [])

    @property
    def ocr_passes(self) -> list[dict[str, Any]]:
        return self._d.get("ocr_passes", [])

    @property
    def chunks(self) -> Optional[dict[str, Any]]:
        """The stored DERIVED chunks ``{policy, items}``, or None if never derived."""
        return self._d.get("chunks")

    @property
    def embedding_spaces(self) -> list[dict[str, Any]]:
        """The embedding-space metadata records (vectors live as container members). Empty for
        an un-embedded model."""
        return self._d.get("embedding_spaces", [])

    # -- chunking (DERIVED) --------------------------------------------------
    def derive_chunks(self) -> dict[str, Any]:
        """A fresh chunk derivation over this model's blocks (see :func:`derive_chunks`)."""
        return derive_chunks(self.blocks)

    def fresh_chunk_ids(self) -> list[str]:
        """The chunk ids a fresh derive would produce — the staleness reference for embedding
        spaces (a space whose ``chunk_ids`` differ from this is stale)."""
        return [c["id"] for c in self.derive_chunks()["items"]]

    def chunks_stale(self) -> bool:
        """True when the STORED chunks differ from a fresh derive (blocks changed since chunking,
        or never derived). Used to surface staleness in `info` and to refuse stale search."""
        stored = self.chunks
        if stored is None:
            return True
        fresh = self.derive_chunks()
        return stored.get("policy") != fresh["policy"] or stored.get("items") != fresh["items"]

    def chunk_text(self, chunk: dict[str, Any]) -> str:
        """Recompose a chunk's text from its blocks — the blocks' searchable text (inline markup
        stripped) joined by blank lines. This is what gets embedded / shown as a snippet; the
        text is NOT stored on the chunk (no duplication)."""
        parts = []
        for bid in chunk.get("block_ids", []):
            b = self._block_by_id.get(bid)
            if b is not None:
                t = _chunk_block_text(b)
                if t:
                    parts.append(t)
        return "\n\n".join(parts)

    def space_by_id(self, space_id: str) -> Optional[dict[str, Any]]:
        for sp in self.embedding_spaces:
            if sp.get("id") == space_id:
                return sp
        return None

    # -- id lookups ----------------------------------------------------------
    def section_by_id(self, sid: str) -> Optional[dict[str, Any]]:
        return self._section_by_id.get(sid)

    def block_by_id(self, bid: str) -> Optional[dict[str, Any]]:
        return self._block_by_id.get(bid)

    def section_ids(self) -> list[str]:
        return [s["id"] for s in self.sections]

    def block_ids(self) -> list[str]:
        return [b["id"] for b in self.blocks]

    # -- block selection -----------------------------------------------------
    def section_subtree_ids(self, sid: str) -> list[str]:
        """`sid` plus every descendant section id (the heading tree under it), so reading a
        parent section yields its whole subtree, not just its bare title block."""
        children: dict[str, list[str]] = {}
        for s in self.sections:
            children.setdefault(s.get("parent"), []).append(s["id"])
        out: list[str] = []
        stack = [sid]
        while stack:
            cur = stack.pop(0)
            out.append(cur)
            stack[:0] = children.get(cur, [])  # preserve document order among siblings
        return out

    def _section_own_blocks(self, sid: str) -> list[dict[str, Any]]:
        ids = self.indexes.get("sections", {}).get(sid)
        if ids is None:
            return [b for b in self.blocks if b.get("section") == sid]
        return [self._block_by_id[i] for i in ids if i in self._block_by_id]

    def blocks_for_section(self, sid: str) -> list[dict[str, Any]]:
        """The blocks of a section AND its descendant sections, in reading order — "read a
        section" means the whole subtree (its title block alone is rarely what an agent wants).
        Sourced from the stored section index, re-sorted to the authoritative block order."""
        want = set(self.section_subtree_ids(sid))
        picked = [b for b in self.blocks if b.get("section") in want]
        return sorted(picked, key=lambda b: self._block_order[b["id"]])

    def blocks_for_pages(self, pages: Iterable[int]) -> list[dict[str, Any]]:
        """The blocks on the given physical page numbers, in reading order."""
        want = set(pages)
        return [b for b in self.blocks if b.get("page") in want]

    # -- page labels ---------------------------------------------------------
    def page_label(self, n: int, scheme: str = "pdf") -> Optional[str]:
        """The page's label under `scheme` (default the `/PageLabels` "pdf" label), or None."""
        for p in self.pages:
            if p.get("n") == n:
                return p.get("labels", {}).get(scheme)
        return None

    def page_label_suffix(self, n: int) -> str:
        """`" (xii)"` when page `n` carries a pdf label, else `""` — for hit/breadcrumb lines."""
        lbl = self.page_label(n)
        return f" ({lbl})" if lbl else ""

    def resolve_page_token(self, token: str) -> Optional[int]:
        """Map a single page token to a physical page number: a bare integer is the physical
        page; anything else is matched against the page labels (e.g. `xii`, `fls.249`)."""
        token = token.strip()
        if token.isdigit():
            n = int(token)
            return n if any(p.get("n") == n for p in self.pages) else None
        for p in self.pages:
            if token in p.get("labels", {}).values():
                return p.get("n")
        return None

    def resolve_page_range(self, spec: str) -> list[int]:
        """Parse a `--pages` spec into physical page numbers. Accepts `N`, `N-M`, and label
        tokens on either side of a range (`xii-xv`). A label that itself contains `-` is tried
        whole FIRST, so `A-1` resolves as a single page before being read as a range. Raises
        :class:`DpdfError` on an unresolvable token so "not found" is never silent."""
        spec = spec.strip()
        whole = self.resolve_page_token(spec)
        if whole is not None:
            return [whole]
        if "-" in spec:
            lo_tok, _, hi_tok = spec.partition("-")
            lo, hi = self.resolve_page_token(lo_tok.strip()), self.resolve_page_token(hi_tok.strip())
            if lo is None:
                raise DpdfError(f"page {lo_tok.strip()!r} not in this document")
            if hi is None:
                raise DpdfError(f"page {hi_tok.strip()!r} not in this document")
            if hi < lo:
                lo, hi = hi, lo
            return list(range(lo, hi + 1))
        raise DpdfError(f"page {spec!r} not in this document")

    # -- find ----------------------------------------------------------------
    def find(
        self,
        query: str,
        *,
        regex: bool = False,
        section: Optional[str] = None,
        kind: Optional[str] = None,
        pages: Optional[list[int]] = None,
    ) -> "FindResult":
        """Lexical search over block text. Case-insensitive substring by default; `regex=True`
        compiles `query` as a regex. Scoped by `section` / `kind` / `pages`. Returns a
        :class:`FindResult` carrying the matches AND the coverage accounting (blocks searched,
        pages spanned, pages with no searchable text) so "not found" is a measured statement."""
        if regex:
            try:
                pat = re.compile(query, re.IGNORECASE)
            except re.error as e:
                raise DpdfError(f"invalid regex {query!r}: {e}") from e

            def matcher(text: str):
                return pat.search(text)
        else:
            needle = query.lower()

            def matcher(text: str):
                i = text.lower().find(needle)
                if i < 0:
                    return None
                return _Span(i, i + len(needle))

        scope = self.blocks
        if section is not None:
            scope = self.blocks_for_section(section)
        if pages is not None:
            want = set(pages)
            scope = [b for b in scope if b.get("page") in want]
        if kind is not None:
            scope = [b for b in scope if b.get("kind") == kind]

        hits: list[Hit] = []
        searched_pages: set[int] = set()
        pages_with_text: set[int] = set()
        for b in scope:
            text = _block_search_text(b)
            page = b.get("page")
            if page is not None:
                searched_pages.add(page)
            if text.strip():
                if page is not None:
                    pages_with_text.add(page)
                m = matcher(text)
                if m is not None:
                    hits.append(Hit(block=b, page=page, start=_span_start(m), end=_span_end(m), text=text))

        no_text_pages = sorted(searched_pages - pages_with_text)
        return FindResult(
            hits=hits,
            searched_blocks=len(scope),
            searched_pages=len(searched_pages),
            no_text_pages=no_text_pages,
        )

    # -- navigation breadcrumbs ----------------------------------------------
    def section_neighbours(self, sid: str) -> tuple[Optional[str], Optional[str], Optional[str]]:
        """`(prev, next, parent)` section ids for a section, by document section order."""
        ids = self.section_ids()
        if sid not in ids:
            return (None, None, None)
        i = ids.index(sid)
        prev = ids[i - 1] if i > 0 else None
        nxt = ids[i + 1] if i + 1 < len(ids) else None
        parent = (self._section_by_id[sid] or {}).get("parent")
        return (prev, nxt, parent)

    def block_neighbours(self, bid: str) -> tuple[Optional[str], Optional[str], Optional[str]]:
        """`(prev, next, parent-section)` for a block, by reading order."""
        i = self._block_order.get(bid)
        if i is None:
            return (None, None, None)
        prev = self.blocks[i - 1]["id"] if i > 0 else None
        nxt = self.blocks[i + 1]["id"] if i + 1 < len(self.blocks) else None
        parent = self.blocks[i].get("section")
        return (prev, nxt, parent)

    # -- the QUERY-markdown emitter -----------------------------------------
    def blocks_markdown(self, blocks: list[dict[str, Any]]) -> str:
        """Render a block slice to simple markdown — the QUERY view (see the module docstring).
        Headings by level, paragraphs, list items, tables as pipe-form, figures/captions as
        placeholders. NOT the fidelity render (`distillpdf.render_markdown`)."""
        return "\n\n".join(_block_md(b) for b in blocks).strip()

    def section_markdown(self, sid: str) -> str:
        return self.blocks_markdown(self.blocks_for_section(sid))

    def closest_section_ids(self, sid: str, n: int = 3) -> list[str]:
        """The `n` section ids closest to a (mistyped) `sid`, for actionable errors."""
        import difflib

        return difflib.get_close_matches(sid, self.section_ids(), n=n, cutoff=0.0)


# ---- find result types ------------------------------------------------------

class _Span:
    """A minimal (start, end) match span, so substring and regex matches share a shape."""

    __slots__ = ("_start", "_end")

    def __init__(self, start: int, end: int):
        self._start, self._end = start, end

    def start(self) -> int:
        return self._start

    def end(self) -> int:
        return self._end


def _span_start(m: Any) -> int:
    return m.start()


def _span_end(m: Any) -> int:
    return m.end()


class Hit:
    """One `find` match: the block it occurred in, the page, and the matched char span."""

    __slots__ = ("block", "page", "start", "end", "text")

    def __init__(self, block: dict[str, Any], page: Optional[int], start: int, end: int, text: str):
        self.block, self.page, self.start, self.end, self.text = block, page, start, end, text

    @property
    def block_id(self) -> str:
        return self.block["id"]


class FindResult:
    """The matches plus the coverage accounting — `find` never silently truncates, and "not
    found" is paired with how much was searched."""

    __slots__ = ("hits", "searched_blocks", "searched_pages", "no_text_pages")

    def __init__(self, hits: list[Hit], searched_blocks: int, searched_pages: int, no_text_pages: list[int]):
        self.hits = hits
        self.searched_blocks = searched_blocks
        self.searched_pages = searched_pages
        self.no_text_pages = no_text_pages


# ---- block text helpers -----------------------------------------------------

def _block_search_text(b: dict[str, Any]) -> str:
    """The searchable text of a block: its `text`, plus a table's flattened cells and a
    figure/table caption (so `find` reaches table contents and captions, not just prose)."""
    parts: list[str] = []
    if b.get("text"):
        parts.append(_plain(b["text"]))
    if b.get("kind") == "table" and b.get("cells"):
        parts.append(" ".join(c for row in b["cells"] for c in row))
    if b.get("caption"):
        parts.append(b["caption"])
    return "\n".join(parts)


def _block_md(b: dict[str, Any]) -> str:
    """One block as query-markdown. `block.text` carries inline HTML markup (the render IR
    projection); the query-markdown view strips it to plain text."""
    kind = b.get("kind")
    text = _plain(b.get("text", ""))
    if kind == "heading":
        level = min(int(b.get("heading_level") or 1), 6)
        return f"{'#' * level} {text}".rstrip()
    if kind == "list_item":
        return f"- {text}"
    if kind == "table":
        return _table_md(b)
    if kind == "figure":
        cap = b.get("caption") or b.get("label") or "figure"
        return f"![{cap}]({b.get('image', '')})" if b.get("image") else f"_[figure: {cap}]_"
    if kind == "caption":
        return f"_{text}_" if text else ""
    if kind == "footnote":
        return f"> {text}" if text else ""
    if kind == "code":
        # The code block's text is `<pre><code>…</code></pre>`; show the inner as a fenced block.
        inner = _plain(text)
        return f"```\n{inner.rstrip(chr(10))}\n```" if inner.strip() else ""
    return text


def _table_md(b: dict[str, Any]) -> str:
    """A table block as a GitHub pipe table when it has cells, else its text / a placeholder."""
    cells = b.get("cells")
    if not cells:
        return b.get("text") or "_[table]_"
    width = max(len(r) for r in cells)
    out: list[str] = []
    head = cells[0] + [""] * (width - len(cells[0]))
    out.append("| " + " | ".join(_md_cell(c) for c in head) + " |")
    out.append("| " + " | ".join(["---"] * width) + " |")
    for row in cells[1:]:
        row = row + [""] * (width - len(row))
        out.append("| " + " | ".join(_md_cell(c) for c in row) + " |")
    return "\n".join(out)


def _md_cell(c: str) -> str:
    """Escape a table cell for pipe-form (newlines flattened, pipes escaped)."""
    return (c or "").replace("\n", " ").replace("|", "\\|").strip()


def iter_blocks_from(blocks: list[dict[str, Any]], from_id: Optional[str]) -> Iterator[dict[str, Any]]:
    """Yield `blocks` starting at `from_id` (inclusive); raises if `from_id` is not in the
    slice (a resume token that doesn't belong to this read is a loud error, not a silent skip)."""
    if from_id is None:
        yield from blocks
        return
    ids = [b["id"] for b in blocks]
    if from_id not in ids:
        raise DpdfError(f"--from {from_id} is not in this selection (its blocks are {ids[0]}–{ids[-1]})"
                        if ids else f"--from {from_id}: this selection has no blocks")
    yield from blocks[ids.index(from_id):]
