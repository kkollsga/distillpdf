#!/usr/bin/env python3
"""Minimal hand-authored PDF writer shared by the table torture corpus.

Everything here exists because reportlab cannot emit it (see the table torture corpus
spec, §9), and every construct has an in-repo
precedent in ``tests/gen_fixtures.py`` — this module is a *port* of those idioms into a
parameterised form, not new research:

  ``assemble_pdf``   <- ``gen_fixtures._assemble_pdf``     (the object/xref writer)
  ``tagged_table``   <- ``gen_fixtures.gen_tagged_table``  (``/StructTreeRoot`` ``/Table``)
  ``/Rotate`` pages  <- ``gen_fixtures.gen_rotated_pages`` ("reportlab cannot set /Rotate")
  ``Separation``     <- ``gen_fixtures.gen_separation``    ("reportlab emits no spot colours")
  glyph-per-``Tj``   <- ``gen_fixtures.gen_glyph_table``   (Courier, 0.6 em advances)
  figure panel       <- ``gen_fixtures.gen_panel_table``   (curve marks keep it a figure)

**Determinism** is a hard requirement (``scripts/check-fixture-drift.sh``): nothing here
reads the clock, the filesystem, ``random``, or ``hash()``, and no path string is ever
written into a PDF — the reportlab image-name defect G1 fixed cannot recur here because
this module embeds no images at all.
"""
from __future__ import annotations

import re
import unicodedata

# ---------------------------------------------------------------------------- primitives

HELV = b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding /WinAnsiEncoding >>"
HELV_B = b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica-Bold /Encoding /WinAnsiEncoding >>"
HELV_O = b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica-Oblique /Encoding /WinAnsiEncoding >>"
COURIER = b"<< /Type /Font /Subtype /Type1 /BaseFont /Courier /Encoding /WinAnsiEncoding >>"
TIMES = b"<< /Type /Font /Subtype /Type1 /BaseFont /Times-Roman /Encoding /WinAnsiEncoding >>"

#: /F1 .. /F5 — the base-14 set every page in this corpus resolves. Five distinct fonts is
#: the documented ceiling (spec §10.4.3: measured medians are 8-9, which needs embedding).
FONTS = {b"F1": HELV, b"F2": HELV_B, b"F3": COURIER, b"F4": TIMES, b"F5": HELV_O}


def assemble_pdf(objs: dict, path: str, info: int | None = None) -> None:
    """Write a minimal PDF from ``{objnum: bytes}`` (numbers contiguous from 1).

    Ported verbatim from ``gen_fixtures._assemble_pdf`` so the two fixture families share
    one writer; ``info`` is an optional object number for the trailer's ``/Info``."""
    body = bytearray(b"%PDF-1.5\n%\xe2\xe3\xcf\xd3\n")
    offsets = {}
    for num in sorted(objs):
        offsets[num] = len(body)
        body += b"%d 0 obj\n" % num + objs[num] + b"\nendobj\n"
    xref_off = len(body)
    n = max(objs) + 1
    body += b"xref\n0 %d\n0000000000 65535 f \n" % n
    for num in range(1, n):
        body += b"%010d 00000 n \n" % offsets[num]
    info_ref = b" /Info %d 0 R" % info if info is not None else b""
    body += (b"trailer\n<< /Size %d /Root 1 0 R%s >>\nstartxref\n%d\n%%%%EOF\n"
             % (n, info_ref, xref_off))
    with open(path, "wb") as f:
        f.write(bytes(body))


def stream(content: bytes) -> bytes:
    return b"<< /Length %d >>\nstream\n%s\nendstream" % (len(content), content)


def esc(s: str) -> bytes:
    """A PDF literal string in WinAnsi. NFC first, so `truth.json` and the page agree."""
    b = unicodedata.normalize("NFC", s).encode("cp1252")
    return b.replace(b"\\", b"\\\\").replace(b"(", b"\\(").replace(b")", b"\\)")


def text_op(x: float, y: float, s: str, font: bytes = b"F1", size: float = 9) -> bytes:
    return b"BT /%s %g Tf %.2f %.2f Td (%s) Tj ET" % (font, size, x, y, esc(s))


def hline(x0: float, x1: float, y: float, w: float = 0.6) -> bytes:
    return b"q %g w 0 G %.2f %.2f m %.2f %.2f l S Q" % (w, x0, y, x1, y)


def vline(x: float, y0: float, y1: float, w: float = 0.6) -> bytes:
    return b"q %g w 0 G %.2f %.2f m %.2f %.2f l S Q" % (w, x, y0, x, y1)


def fill_rect(x: float, y: float, w: float, h: float, rgb=(0.88, 0.88, 0.92)) -> bytes:
    return b"q %g %g %g rg %.2f %.2f %.2f %.2f re f Q" % (rgb + (x, y, w, h))


# ----------------------------------------------------------------- post-emit stroke census

_NUM = re.compile(rb"[-+]?(?:\d+\.?\d*|\.\d+)")
_TOK = re.compile(rb"[-+]?(?:\d+\.?\d*|\.\d+)|/[^\s/\[\]<>(){}]+|<<|>>|\[|\]|\(|[A-Za-z'\"*]+")


def _mul(m, n):
    a, b, c, d, e, f = m
    A, B, C, D, E, F = n
    return (a * A + b * C, a * B + b * D, c * A + d * C,
            c * B + d * D, e * A + f * C + E, e * B + f * D + F)


def _apply(m, x, y):
    a, b, c, d, e, f = m
    return (a * x + c * y + e, b * x + d * y + f)


def stroke_census(path: str) -> dict:
    """``{"h": n, "v": n, "fills": n}`` over every content stream in ``path``.

    This is the assertion instrument for spec §4.2.4: **arXiv booktabs has exactly zero
    vertical rules — not "few", none** — so every ``booktabs`` case is parsed back after
    emission and a single vertical stroke fails the build. It reads the emitted bytes, not
    the generator's intent, which is the only way the assertion can catch a style command
    that draws more than it claims.

    Classification, stated so the number is interpretable: a stroked segment is *vertical*
    when |dx| <= 0.5 and |dy| > 1, *horizontal* when |dy| <= 0.5 and |dx| > 1; a ``re``
    thinner than 2pt in one axis is a rule in the other; a filled ``re`` thicker than 2pt
    in both axes is a *fill*. Diagonals count as neither (the figure-ink marks in
    ``t3_panel_duplicate`` are deliberately curved/slanted)."""
    import pikepdf

    tot = {"h": 0, "v": 0, "fills": 0}
    with pikepdf.open(path) as pdf:
        for page in pdf.pages:
            data = page.Contents
            if isinstance(data, pikepdf.Array):
                raw = b"\n".join(bytes(s.read_bytes()) for s in data)
            else:
                raw = bytes(data.read_bytes())
            _scan(raw, tot)
            # Form XObjects drawn by the page carry their own strokes.
            try:
                xo = page.Resources.get("/XObject", {})
            except Exception:
                xo = {}
            for _, o in dict(xo).items():
                try:
                    if str(o.get("/Subtype", "")) == "/Form":
                        _scan(bytes(o.read_bytes()), tot)
                except Exception:
                    pass
    return tot


def _scan(content: bytes, tot: dict) -> None:
    ident = (1.0, 0.0, 0.0, 1.0, 0.0, 0.0)
    ctm, stack = ident, []
    ops: list = []
    segs: list = []          # ((x0,y0),(x1,y1)) in device space
    rects: list = []         # (x,y,w,h) in device space
    cur = start = None
    in_text = False
    for m in _TOK.finditer(content):
        t = m.group(0)
        if _NUM.fullmatch(t):
            ops.append(float(t))
            continue
        if t.startswith(b"/") or t in (b"<<", b">>", b"[", b"]", b"("):
            # operands we do not need; a literal string can hide operator-looking bytes,
            # so skip to its close paren.
            if t == b"(":
                depth, i = 1, m.end()
                while i < len(content) and depth:
                    ch = content[i:i + 1]
                    if ch == b"\\":
                        i += 2
                        continue
                    depth += (ch == b"(") - (ch == b")")
                    i += 1
            continue
        op, args, ops = t, ops, []
        if op == b"BT":
            in_text = True
        elif op == b"ET":
            in_text = False
        elif in_text:
            continue
        elif op == b"q":
            stack.append(ctm)
        elif op == b"Q":
            ctm = stack.pop() if stack else ident
        elif op == b"cm" and len(args) >= 6:
            ctm = _mul(tuple(args[-6:]), ctm)
        elif op == b"m" and len(args) >= 2:
            cur = start = _apply(ctm, args[-2], args[-1])
        elif op == b"l" and len(args) >= 2 and cur:
            nxt = _apply(ctm, args[-2], args[-1])
            segs.append((cur, nxt))
            cur = nxt
        elif op == b"h" and cur and start:
            segs.append((cur, start))
            cur = start
        elif op == b"re" and len(args) >= 4:
            x, y, w, h = args[-4:]
            p0 = _apply(ctm, x, y)
            p1 = _apply(ctm, x + w, y + h)
            rects.append((min(p0[0], p1[0]), min(p0[1], p1[1]),
                          abs(p1[0] - p0[0]), abs(p1[1] - p0[1])))
        elif op in (b"S", b"s", b"B", b"B*", b"b", b"b*"):
            _classify(segs, rects, tot, stroked=True)
            segs, rects, cur, start = [], [], None, None
        elif op in (b"f", b"F", b"f*"):
            _classify(segs, rects, tot, stroked=False)
            segs, rects, cur, start = [], [], None, None
        elif op in (b"n", b"W", b"W*"):
            if op == b"n":
                segs, rects, cur, start = [], [], None, None


def _classify(segs, rects, tot, stroked):
    for (x0, y0), (x1, y1) in segs:
        dx, dy = abs(x1 - x0), abs(y1 - y0)
        if dx <= 0.5 and dy > 1.0:
            tot["v"] += 1
        elif dy <= 0.5 and dx > 1.0:
            tot["h"] += 1
    for x, y, w, h in rects:
        if min(w, h) <= 2.0 and max(w, h) > 2.0:
            tot["h" if w >= h else "v"] += 1
        elif min(w, h) > 2.0:
            if stroked:
                tot["h"] += 2
                tot["v"] += 2
            else:
                tot["fills"] += 1


# ------------------------------------------------------------- the tagged-table emitter

#: Visual styles the emitter can draw around the same declared geometry. The tagged axis is
#: owner-mandated across every structural type (spec §6.2), so the *visual* is a parameter:
#: a tagged twin has to look like its untagged sibling or it is testing a different thing.
STYLES = ("full_grid", "booktabs", "borderless", "column_ruled", "zebra", "overdecorated")


def _norm_grid(grid):
    """(n_rows, n_cols, cells) from a nested list of cell dicts / None (covered slot)."""
    n_rows = len(grid)
    n_cols = 0
    for row in grid:
        w = 0
        for c in row:
            w += (c or {}).get("colspan", 1) if c is not None else 1
        n_cols = max(n_cols, w)
    return n_rows, n_cols


def tagged_table(path, grid, *, style="full_grid", header_rows=1, x0=72.0, y_top=700.0,
                 col_w=76.0, row_h=18.0, size=9.0, title=None, lead=None, trail=None,
                 page_size=(612.0, 792.0), rotate=None, declare=True, struct_override=None,
                 x_jitter=(), mark_untagged_extra=None):
    """Emit one page carrying ONE table, optionally declared in ``/StructTreeRoot``.

    ``grid`` is the logical grid: ``grid[r][c]`` is either ``None`` (a slot covered by a
    span anchored elsewhere) or ``{"text": str, "colspan": int, "rowspan": int,
    "header": bool}``. Span text lives on the anchor and covered slots are omitted from the
    declaration, exactly as ``truth.json`` records them (spec §7).

    ``declare=False`` emits the identical ink with no structure tree — that is how a
    "tagged twin" and its untagged sibling are held to the same geometry.

    ``struct_override="degenerate"`` declares the observed three-shard 2x12 over the real
    grid (``t3_tagged_degenerate``): the shape is copied from ``gen_tagged_table``'s refused
    block, and the expectation is that the declaration is REJECTED and geometry wins.

    ``x_jitter`` is a per-row x offset applied to the TEXT only, so a case can carry a
    correct declaration over wrong-but-plausible visual alignment: inference reading geometry
    alone lands on a different grid than the file declares.

    Returns the truth record for the table (cells, dims, ``bbox_norm``, ``header_rows``).
    """
    n_rows, n_cols = _norm_grid(grid)
    pw, ph = page_size
    body: list[bytes] = []
    if title:
        body.append(text_op(x0, y_top + 34, title, b"F2", 13))
    if lead:
        body.append(text_op(x0, y_top + 16, lead, b"F1", 9.5))

    tx0, tx1 = x0, x0 + n_cols * col_w
    ty1, ty0 = y_top + row_h, y_top + row_h - n_rows * row_h

    # --- the visual, drawn BEFORE the text so fills sit under their cells ---------------
    if style == "zebra":
        for r in range(header_rows, n_rows):
            if (r - header_rows) % 2 == 0:
                body.append(fill_rect(tx0, ty1 - (r + 1) * row_h, tx1 - tx0, row_h,
                                      (0.90, 0.90, 0.94)))
    if style in ("full_grid", "overdecorated") and header_rows:
        body.append(fill_rect(tx0, ty1 - header_rows * row_h, tx1 - tx0,
                              header_rows * row_h, (0.85, 0.85, 0.91)))
    if style == "full_grid":
        for r in range(n_rows + 1):
            body.append(hline(tx0, tx1, ty1 - r * row_h))
        for c in range(n_cols + 1):
            body.append(vline(tx0 + c * col_w, ty0, ty1))
    elif style == "booktabs":
        body.append(hline(tx0, tx1, ty1, 1.0))
        body.append(hline(tx0, tx1, ty1 - header_rows * row_h, 0.5))
        body.append(hline(tx0, tx1, ty0, 1.0))
    elif style == "column_ruled":
        for c in range(n_cols + 1):
            body.append(vline(tx0 + c * col_w, ty0, ty1))
    elif style == "overdecorated":
        for r in range(n_rows + 1):
            y = ty1 - r * row_h
            body.append(hline(tx0 - 2, tx1 + 2, y))
            body.append(hline(tx0 - 2, tx1 + 2, y - 1.5))
        for c in range(n_cols + 1):
            x = tx0 + c * col_w
            body.append(vline(x, ty0 - 2, ty1 + 2))
            body.append(vline(x + 1.5, ty0 - 2, ty1 + 2))
        for r in range(header_rows, n_rows, 2):
            for c in range(0, n_cols, 2):
                body.append(fill_rect(tx0 + c * col_w + 2, ty1 - (r + 1) * row_h + 2,
                                      col_w - 4, row_h - 4, (0.93, 0.93, 0.86)))
    # "borderless" draws nothing.

    # --- the text, one MCID per cell ---------------------------------------------------
    cells, mcid = [], 0
    struct_rows: list[list[bytes]] = []
    for r, row in enumerate(grid):
        c = 0
        struct_cells: list[bytes] = []
        for item in row:
            if item is None:
                c += 1
                continue
            cs, rs = item.get("colspan", 1), item.get("rowspan", 1)
            txt = item.get("text", "")
            hdr = item.get("header", r < header_rows)
            x = tx0 + c * col_w + 4 + (x_jitter[r % len(x_jitter)] if x_jitter else 0.0)
            y = ty1 - (r + 1) * row_h + 5
            fnt = b"F2" if hdr else b"F1"
            if txt:
                if declare:
                    body.append(b"/P << /MCID %d >> BDC %s EMC"
                                % (mcid, text_op(x, y, txt, fnt, size)))
                else:
                    body.append(text_op(x, y, txt, fnt, size))
            rec = {"r": r, "c": c, "text": txt}
            if cs > 1:
                rec["colspan"] = cs
            if rs > 1:
                rec["rowspan"] = rs
            if hdr:
                rec["header"] = True
            if not txt:
                rec["blank"] = True
            cells.append(rec)
            span = b""
            if cs > 1:
                span += b"/ColSpan %d " % cs
            if rs > 1:
                span += b"/RowSpan %d " % rs
            attr = b"/A << /O /Table %s>> " % span if span else b""
            struct_cells.append(
                b"<< /Type /StructElem /S /%s /Pg 3 0 R %s/K [%d] >>"
                % (b"TH" if hdr else b"TD", attr, mcid))
            mcid += 1
            c += cs
        struct_rows.append(struct_cells)

    if trail:
        body.append(text_op(x0, ty0 - 26, trail, b"F1", 9.5))
    if mark_untagged_extra:
        body.extend(mark_untagged_extra(tx0, ty0))

    content = b"\n".join(body)
    fonts = b" ".join(b"/%s %d 0 R" % (n, 5 + i) for i, n in enumerate(FONTS))
    objs = {
        1: (b"<< /Type /Catalog /Pages 2 0 R >>" if not declare else
            b"<< /Type /Catalog /Pages 2 0 R /StructTreeRoot 10 0 R "
            b"/MarkInfo << /Marked true >> >>"),
        2: b"<< /Type /Pages /Kids [3 0 R] /Count 1 /MediaBox [0 0 %g %g] >>" % (pw, ph),
        3: (b"<< /Type /Page /Parent 2 0 R /Contents 4 0 R /StructParents 0 "
            b"/Resources << /Font << %s >> >>%s >>"
            % (fonts, b" /Rotate %d" % rotate if rotate else b"")),
        4: stream(content),
    }
    for i, ob in enumerate(FONTS.values()):
        objs[5 + i] = ob

    if declare:
        nxt = 11
        if struct_override == "degenerate":
            # The observed rejected declaration: three single-row shards of 12 cells each
            # over a real grid. Shape copied from gen_tagged_table's refused block.
            shard_ids = []
            per = max(1, (mcid + 2) // 3)
            for s in range(3):
                ids = list(range(s * per, min(mcid, (s + 1) * per)))
                tr = b"<< /Type /StructElem /S /TR /Pg 3 0 R /K [%s] >>" % b" ".join(
                    b"<< /Type /StructElem /S /TD /Pg 3 0 R /K [%d] >>" % i for i in ids)
                objs[nxt] = b"<< /Type /StructElem /S /Table /Pg 3 0 R /K [%s] >>" % tr
                shard_ids.append(nxt)
                nxt += 1
            objs[10] = b"<< /Type /StructTreeRoot /K [%d 0 R] >>" % nxt
            objs[nxt] = (b"<< /Type /StructElem /S /Document /Pg 3 0 R /K [%s] >>"
                         % b" ".join(b"%d 0 R" % i for i in shard_ids))
        else:
            tr_ids = []
            for sc in struct_rows:
                objs[nxt] = (b"<< /Type /StructElem /S /TR /Pg 3 0 R /K [%s] >>"
                             % b" ".join(sc))
                tr_ids.append(nxt)
                nxt += 1
            tbl = nxt
            objs[tbl] = (b"<< /Type /StructElem /S /Table /Pg 3 0 R /K [%s] >>"
                         % b" ".join(b"%d 0 R" % i for i in tr_ids))
            nxt += 1
            objs[10] = b"<< /Type /StructTreeRoot /K [%d 0 R] >>" % nxt
            objs[nxt] = (b"<< /Type /StructElem /S /Document /Pg 3 0 R /K [%d 0 R] >>"
                         % tbl)
        for n in range(5 + len(FONTS), 10):
            objs.setdefault(n, b"<< >>")

    assemble_pdf(objs, path)

    # bbox in page fractions, top-left origin, y-down. On a /Rotate page this is stated in
    # POST-rotation display space (spec §7).
    bx0, bx1, by0, by1 = tx0, tx1, ty0, ty1 + 1
    if rotate in (90, 270):
        # display page is ph x pw; a 90 deg clockwise turn maps (x, y) -> (ph - y, ... )
        dx0, dx1 = ph - by1, ph - by0
        dy0, dy1 = bx0, bx1
        bbox = [dx0 / ph, dy0 / pw, dx1 / ph, dy1 / pw]
    else:
        bbox = [bx0 / pw, (ph - by1) / ph, bx1 / pw, (ph - by0) / ph]
    return {"n_rows": n_rows, "n_cols": n_cols, "style": style,
            "header_rows": header_rows, "cells": cells,
            "bbox_norm": [round(v, 4) for v in bbox]}
