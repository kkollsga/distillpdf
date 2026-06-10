"""The agent document shell over a `.dpdf` — the CLI verbs `info / toc / read / find /
tables / figures / ocr-status`.

These are what makes a `.dpdf` *better* than HTML/Markdown for an agent (see
docs/datamodel-design.md, "The agent CLI"): every listing emits ids that thread into the
next call, `read` carries navigation breadcrumbs and bounds its output with honest resumable
truncation, and `find` is the exact-phrase primitive with a coverage line so "not found" is
a measured statement. `--json` on every verb mirrors the human form for structured callers.

The shell is a sibling of the PDF→HTML/Markdown converter in `cli.py`: `cli.main` detects a
`.dpdf` input and routes here (the convert CLI is untouched). Each verb is a small function
taking the parsed :class:`~distillpdf.dpdf.Model` and the verb's argparse namespace, and
returns an exit code after printing.
"""
from __future__ import annotations

import argparse
import json
import sys
from typing import Any, Optional

from .dpdf import DpdfError, FindResult, Model, iter_blocks_from

# `read` defaults to a sane bound; truncation is reported and resumable (never a silent cut).
DEFAULT_MAX_CHARS = 6000
# `find` never silently truncates; it caps the printed rows and says how many it held back.
DEFAULT_FIND_LIMIT = 20


def _emit_json(obj: Any) -> None:
    """Structured output: compact-but-readable JSON, sorted keys for determinism."""
    json.dump(obj, sys.stdout, indent=2, sort_keys=True, ensure_ascii=False)
    sys.stdout.write("\n")


def _breadcrumbs(prev: Optional[str], nxt: Optional[str], parent: Optional[str]) -> str:
    """`prev: <id> · next: <id> · parent: <id>` — navigation in the output, not the agent's
    memory. A missing neighbour shows as an em dash."""
    def show(x: Optional[str]) -> str:
        return x if x else "—"
    return f"prev: {show(prev)} · next: {show(nxt)} · parent: {show(parent)}"


# ---- info -------------------------------------------------------------------

def _ocr_state_summary(model: Model) -> dict[str, Any]:
    """A compact OCR-state roll-up for `info`: page decisions, pass count, per-pass outcome
    tallies. Empty/none for a born-digital model (no passes)."""
    decisions: dict[str, int] = {}
    for p in model.pages:
        d = p.get("ocr_decision")
        if d:
            decisions[d] = decisions.get(d, 0) + 1
    outcomes: dict[str, int] = {}
    for ocp in model.ocr_passes:
        for r in ocp.get("results", []):
            o = r.get("outcome")
            if o:
                outcomes[o] = outcomes.get(o, 0) + 1
    return {"decisions": decisions, "passes": len(model.ocr_passes), "outcomes": outcomes}


def _asset_profile(model: Model) -> dict[str, int]:
    """Asset storage roll-up (embedded / external / dropped counts)."""
    prof: dict[str, int] = {}
    for a in model.assets:
        s = a.get("storage", "?")
        prof[s] = prof.get(s, 0) + 1
    return prof


def verb_info(model: Model, args: argparse.Namespace) -> int:
    # One source of truth for the info roll-up: Doc.info() shapes it, the CLI prints it.
    from .doc import Doc

    data = Doc.from_model(model).info()
    src = model.source
    ocr = data["ocr"]
    if args.json:
        _emit_json(data)
        return 0
    print(f"{src.get('file', '?')}  (schema v{model.schema_version}, distillpdf {src.get('distillpdf', '?')})")
    print(f"  source sha256: {data['source']['sha256_prefix']}…   generated {src.get('generated_at', '?')}")
    print(f"  pages: {data['pages']}   sections: {data['sections']}   blocks: {data['blocks']}")
    print(f"  tables: {data['tables']}   figures: {data['figures']}   footnotes: {data['footnotes']}")
    cov = data["coverage"]
    if "sectioned" in cov:
        print(f"  sectioned coverage: {cov['sectioned'] * 100:.0f}%"
              + (f"   ({len(cov.get('unsectioned_blocks', []))} unsectioned)" if cov.get("unsectioned_blocks") else ""))
    if ocr["passes"] or ocr["decisions"]:
        dec = ", ".join(f"{k}={v}" for k, v in sorted(ocr["decisions"].items())) or "—"
        out = ", ".join(f"{k}={v}" for k, v in sorted(ocr["outcomes"].items())) or "—"
        print(f"  OCR: {ocr['passes']} pass(es); page decisions: {dec}; outcomes: {out}")
    else:
        print("  OCR: none (born-digital text layer)")
    prof = data["assets"]
    if prof:
        print("  assets: " + ", ".join(f"{k}={v}" for k, v in sorted(prof.items())))
    return 0


# ---- toc --------------------------------------------------------------------

def verb_toc(model: Model, args: argparse.Namespace) -> int:
    rows = [
        {
            "id": s["id"],
            "level": s.get("level", 1),
            "title": s.get("title", ""),
            "page_start": s.get("page_start"),
            "page_end": s.get("page_end"),
        }
        for s in model.sections
    ]
    if args.json:
        _emit_json({"sections": rows})
        return 0
    if not rows:
        print("(no sections)")
        return 0
    for r in rows:
        indent = "  " * (int(r["level"]) - 1)
        ps, pe = r["page_start"], r["page_end"]
        span = f"p{ps}" if ps == pe else f"p{ps}-{pe}"
        print(f"{indent}{r['id']}  {r['title']}  ({span})")
    return 0


# ---- tables / figures (kind index listings) --------------------------------

def _kind_listing(model: Model, kind: str, args: argparse.Namespace) -> int:
    from .doc import Doc

    # Shared row shape with Doc.tables()/figures() (each row carries an extra page_label the
    # human listing recomputes via page_label_suffix); the id threads into the next call.
    rows = Doc.from_model(model)._kind_index(kind)
    if args.json:
        _emit_json({kind + "s": rows})
        return 0
    if not rows:
        print(f"(no {kind}s)")
        return 0
    for r in rows:
        label = r["label"] or ""
        cap = f"  — {r['caption']}" if r["caption"] else ""
        suffix = model.page_label_suffix(r["page"]) if r["page"] else ""
        print(f"{r['id']}  {label}  p{r['page']}{suffix}{cap}".replace("   ", "  "))
    return 0


def verb_tables(model: Model, args: argparse.Namespace) -> int:
    return _kind_listing(model, "table", args)


def verb_figures(model: Model, args: argparse.Namespace) -> int:
    return _kind_listing(model, "figure", args)


# ---- read -------------------------------------------------------------------

def _bounded_blocks(blocks: list[dict[str, Any]], from_id: Optional[str], max_chars: int):
    """Walk `blocks` from `from_id`, accumulating query-markdown until `max_chars` is reached.
    Returns `(shown_blocks, next_id)` where `next_id` is the first block NOT shown (the resume
    token) or None when everything fit. Always shows at least one block so progress is
    guaranteed even when a single block exceeds the bound."""
    shown: list[dict[str, Any]] = []
    total = 0
    walk = list(iter_blocks_from(blocks, from_id))
    for i, b in enumerate(walk):
        from .dpdf import _block_md  # one emitter, shared with the markdown view

        piece = _block_md(b)
        if shown and total + len(piece) + 2 > max_chars:
            return shown, b["id"]
        shown.append(b)
        total += len(piece) + 2
    return shown, None


def _read_target(model: Model, target: str) -> tuple[str, list[dict[str, Any]], Any]:
    """Resolve a `read` target to `(kind, blocks, nav)` where `kind` is 'section'|'block' and
    `nav` is the breadcrumb tuple source. Raises :class:`DpdfError` (with suggestions for a
    near-miss section id) when the target resolves to nothing."""
    if model.section_by_id(target):
        return "section", model.blocks_for_section(target), model.section_neighbours(target)
    if model.block_by_id(target):
        return "block", [model.block_by_id(target)], model.block_neighbours(target)
    # Not an id — suggest close section ids (the common mistype).
    close = model.closest_section_ids(target)
    hint = f" closest section ids: {', '.join(close)}" if close else ""
    raise DpdfError(f"unknown id {target!r}.{hint}")


def verb_read(model: Model, args: argparse.Namespace) -> int:
    # `read --pages X-Y` reads a physical/label page range; otherwise `read <id>` reads a
    # section or a single block.
    if args.pages is not None:
        pages = model.resolve_page_range(args.pages)
        blocks = model.blocks_for_pages(pages)
        target_desc = f"--pages {args.pages}"
        nav = None
    elif args.target is not None:
        kind, blocks, nav = _read_target(model, args.target)
        target_desc = args.target
    else:
        raise DpdfError("read needs a <section-id>, a <block-id>, or --pages N-M")

    if not blocks:
        if args.json:
            _emit_json({"target": target_desc, "blocks": [], "markdown": ""})
        else:
            print(f"(no blocks for {target_desc})")
        return 0

    shown, next_id = _bounded_blocks(blocks, args.from_id, args.max_chars)
    md = model.blocks_markdown(shown)
    first_id, last_id = shown[0]["id"], shown[-1]["id"]
    full_first, full_last = blocks[0]["id"], blocks[-1]["id"]

    if args.json:
        out: dict[str, Any] = {
            "target": target_desc,
            "block_ids": [b["id"] for b in shown],
            "markdown": md,
            "truncated": next_id is not None,
        }
        if next_id is not None:
            out["next"] = next_id
            out["full_range"] = [full_first, full_last]
        if nav is not None:
            out["nav"] = {"prev": nav[0], "next": nav[1], "parent": nav[2]}
        _emit_json(out)
        return 0

    print(md)
    print()
    if next_id is not None:
        # Honest truncation: exactly where it stopped, the full range, and how to resume.
        print(f"showing blocks {first_id}–{last_id} of {full_first}–{full_last}; "
              f"continue: read {target_desc} --from {next_id}")
    if nav is not None:
        print(_breadcrumbs(*nav))
    return 0


# ---- find -------------------------------------------------------------------

def _snippet(hit, width: int = 80) -> str:
    """A one-line snippet around the match, the matched span wrapped in «…»."""
    text = hit.text.replace("\n", " ")
    s, e = hit.start, hit.end
    half = max(0, (width - (e - s)) // 2)
    lo = max(0, s - half)
    hi = min(len(text), e + half)
    pre = ("…" if lo > 0 else "") + text[lo:s]
    mid = text[s:e]
    post = text[e:hi] + ("…" if hi < len(text) else "")
    return f"{pre}«{mid}»{post}".strip()


def _coverage_line(res: FindResult) -> str:
    line = f"searched {res.searched_blocks} blocks across {res.searched_pages} pages"
    if res.no_text_pages:
        pages = ", ".join(str(p) for p in res.no_text_pages)
        line += f"; {len(res.no_text_pages)} page(s) have no text [pages {pages}]"
    return line


def verb_find(model: Model, args: argparse.Namespace) -> int:
    pages = model.resolve_page_range(args.pages) if args.pages else None
    if args.section is not None and model.section_by_id(args.section) is None:
        close = model.closest_section_ids(args.section)
        hint = f" closest section ids: {', '.join(close)}" if close else ""
        raise DpdfError(f"unknown section {args.section!r}.{hint}")
    res = model.find(
        args.query, regex=args.regex, section=args.section, kind=args.kind, pages=pages
    )

    def hit_obj(h) -> dict[str, Any]:
        nav_block = h.block
        return {
            "block_id": h.block_id,
            "section": nav_block.get("section"),
            "page": h.page,
            "page_label": model.page_label(h.page) if h.page else None,
            "kind": nav_block.get("kind"),
            "snippet": _snippet(h),
            "context": _context_ids(model, h.block_id, args.context) if args.context else None,
        }

    shown = res.hits[: args.limit]
    if args.json:
        _emit_json({
            "query": args.query,
            "regex": args.regex,
            "matches": [hit_obj(h) for h in shown],
            "total_matches": len(res.hits),
            "shown": len(shown),
            "coverage": {
                "searched_blocks": res.searched_blocks,
                "searched_pages": res.searched_pages,
                "no_text_pages": res.no_text_pages,
            },
        })
        return 0

    if not res.hits:
        print(f"no matches for {args.query!r}")
        print(_coverage_line(res))
        return 0
    for h in shown:
        sec = h.block.get("section") or "—"
        suffix = model.page_label_suffix(h.page) if h.page else ""
        print(f"{h.block_id}  [{sec}]  p{h.page}{suffix}  {_snippet(h)}")
        if args.context:
            ctx = _context_ids(model, h.block_id, args.context)
            if ctx:
                print(f"    context: {' '.join(ctx)}")
    print()
    if len(res.hits) > len(shown):
        # find never silently truncates: it says how many it held back and how to see them.
        print(f"showing {len(shown)} of {len(res.hits)} matches; "
              f"narrow with --section/--pages or raise --limit")
    print(_coverage_line(res))
    return 0


def _context_ids(model: Model, bid: str, n: int) -> list[str]:
    """The `n` block ids on each side of `bid` in reading order (neighbouring blocks)."""
    order = model._block_order
    i = order.get(bid)
    if i is None:
        return []
    lo = max(0, i - n)
    hi = min(len(model.blocks), i + n + 1)
    return [b["id"] for b in model.blocks[lo:hi] if b["id"] != bid]


# ---- ocr-status -------------------------------------------------------------

def verb_ocr_status(model: Model, args: argparse.Namespace) -> int:
    page_rows = [
        {"page": p.get("n"), "decision": p.get("ocr_decision"), "active_pass": p.get("active_ocr_pass")}
        for p in model.pages
        if p.get("ocr_decision") or p.get("active_ocr_pass")
    ]
    pass_rows = []
    for ocp in model.ocr_passes:
        for r in ocp.get("results", []):
            pass_rows.append({
                "pass": ocp.get("id"),
                "engine": ocp.get("engine"),
                "page": r.get("page"),
                "outcome": r.get("outcome"),
                "legible_chars": r.get("legible_chars"),
                "confidence": r.get("confidence"),
            })
    if args.json:
        _emit_json({"pages": page_rows, "passes": pass_rows})
        return 0
    if not pass_rows and not page_rows:
        print("no OCR passes recorded")
        return 0
    if page_rows:
        print("per-page decisions:")
        for r in page_rows:
            ap = f"  active: {r['active_pass']}" if r["active_pass"] else ""
            print(f"  p{r['page']}: {r['decision'] or '—'}{ap}")
    if pass_rows:
        print("per-pass outcomes:")
        print(f"  {'pass':6} {'engine':24} {'page':>5} {'outcome':14} {'legible':>8} conf")
        for r in pass_rows:
            conf = "" if r["confidence"] is None else f"{r['confidence']:.2f}"
            print(f"  {r['pass'] or '?':6} {(r['engine'] or '?')[:24]:24} {r['page']:>5} "
                  f"{r['outcome'] or '?':14} {r['legible_chars']:>8} {conf}")
    return 0


# ---- dispatch ---------------------------------------------------------------

VERBS = {
    "info": verb_info,
    "toc": verb_toc,
    "read": verb_read,
    "find": verb_find,
    "tables": verb_tables,
    "figures": verb_figures,
    "ocr-status": verb_ocr_status,
}


def _build_parser() -> argparse.ArgumentParser:
    """The document-shell argparse: `distillpdf <file.dpdf> <verb> [args]`. Separate from the
    converter parser in cli.py — the converter has no notion of these verbs and vice versa."""
    p = argparse.ArgumentParser(prog="distillpdf", add_help=True,
                                description="Query a .dpdf document model.")
    p.add_argument("dpdf", help="a .dpdf document model")
    sub = p.add_subparsers(dest="verb", required=True)

    def add_json(sp):
        sp.add_argument("--json", action="store_true", help="structured JSON output")

    add_json(sub.add_parser("info", help="pages, sections, tables/figures, OCR state, assets"))
    add_json(sub.add_parser("toc", help="section tree: ids, titles, page ranges"))
    add_json(sub.add_parser("tables", help="table index: id, label, page, caption"))
    add_json(sub.add_parser("figures", help="figure index: id, label, page, caption"))
    add_json(sub.add_parser("ocr-status", help="per-page OCR decisions + per-pass outcomes"))

    rd = sub.add_parser("read", help="a section / block as markdown, or a page range")
    rd.add_argument("target", nargs="?", help="a section id or a block id")
    rd.add_argument("--pages", metavar="N-M", help="a page range (physical or label, e.g. xii)")
    rd.add_argument("--max-chars", type=int, default=DEFAULT_MAX_CHARS, dest="max_chars",
                    help=f"bound the output (default {DEFAULT_MAX_CHARS}); truncation is resumable")
    rd.add_argument("--from", metavar="BLOCK-ID", dest="from_id", help="resume reading at this block id")
    add_json(rd)

    fd = sub.add_parser("find", help="lexical search over block text, with a coverage line")
    fd.add_argument("query", help="substring (default) or regex (with --regex)")
    fd.add_argument("--regex", action="store_true", help="treat the query as a regex")
    fd.add_argument("--section", metavar="ID", help="scope to a section id")
    fd.add_argument("--kind", metavar="KIND", help="scope to a block kind (table, figure, …)")
    fd.add_argument("--pages", metavar="N-M", help="scope to a page range")
    fd.add_argument("--context", type=int, default=0, metavar="N",
                    help="also list N neighbouring block ids per hit")
    fd.add_argument("--limit", type=int, default=DEFAULT_FIND_LIMIT,
                    help=f"max matches to print (default {DEFAULT_FIND_LIMIT}); never silent")
    add_json(fd)
    return p


def run(argv: list[str]) -> int:
    """Run the document shell. `argv` is the full CLI argv (file + verb + args). Errors are
    actionable: an unreadable / wrong-shaped file says what it looks like; an unknown id
    suggests the closest ones."""
    args = _build_parser().parse_args(argv)
    try:
        model = Model.load(args.dpdf)
    except DpdfError as e:
        print(f"distillpdf: {args.dpdf}: {e}", file=sys.stderr)
        print("distillpdf: this verb needs a .dpdf model — make one with "
              "`distillpdf <file.pdf> -o <file>.dpdf`.", file=sys.stderr)
        return 1
    try:
        return VERBS[args.verb](model, args)
    except DpdfError as e:
        print(f"distillpdf: {e}", file=sys.stderr)
        return 1
