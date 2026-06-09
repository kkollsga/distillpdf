"""Command-line interface: convert PDFs to clean, LLM-ready HTML or Markdown.

    distillpdf paper.pdf                       # HTML to stdout (images inline)
    distillpdf paper.pdf -o paper.html         # HTML to a file
    distillpdf *.pdf -o out/                   # batch: out/<name>.html per input
    distillpdf paper.pdf --markdown            # Markdown instead of HTML
    distillpdf paper.pdf -o paper.md           # Markdown (inferred from .md extension)
    distillpdf paper.pdf --mode page           # page-first (default is section-first)
    distillpdf paper.pdf -o p.html --image-mode external   # lean HTML + an img/ folder
    distillpdf paper.pdf --image-mode drop     # replace images with placeholder text
    distillpdf paper.pdf --no-toc              # omit the table-of-contents nav
    distillpdf paper.pdf --text                # plain text instead of HTML
    distillpdf paper.pdf --toc                 # print the table of contents
    distillpdf paper.pdf --section abstract

    distillpdf scan.pdf --ocr                  # OCR scanned pages -> scan.searchable.pdf
    distillpdf scan.pdf --ocr -o out.html      # ...or OCR'd HTML / Markdown (by extension)
    distillpdf scan.pdf --ocr --remove-raster  # reflow to clean text, drop the page images
"""
import argparse
import os
import sys

from . import __version__, open as _open


def _fmt(args, dest):
    """The output format for this run: explicit --markdown/--text, or inferred from -o."""
    if args.text:
        return "text"
    if args.markdown:
        return "markdown"
    if dest and dest.lower().endswith((".md", ".markdown")):
        return "markdown"
    return "html"


def _ext(fmt):
    return {"text": ".txt", "markdown": ".md", "html": ".html"}[fmt]


def _out_path(src, args, multiple, fmt):
    """Where one input's output goes: explicit -o, a dir for batches, or None (stdout)."""
    ext = _ext(fmt)
    stem = os.path.splitext(os.path.basename(src))[0]
    if args.output is None:
        # A single input with no -o streams to stdout; a batch defaults to writing
        # <stem><ext> beside each source.
        return os.path.join(os.path.dirname(src), stem + ext) if multiple else None
    if multiple or os.path.isdir(args.output) or args.output.endswith(os.sep):
        os.makedirs(args.output, exist_ok=True)
        return os.path.join(args.output, stem + ext)
    return args.output


def main(argv=None):
    p = argparse.ArgumentParser(
        prog="distillpdf",
        description="Convert PDFs into clean, LLM-ready HTML or Markdown (or plain text).",
    )
    p.add_argument("pdf", nargs="+", help="input PDF file(s)")
    p.add_argument(
        "-o", "--output",
        help="output file (single input) or directory (multiple inputs); "
        "default: stdout for one PDF, <name>.<ext> beside each for many",
    )
    p.add_argument("--markdown", "--md", action="store_true", help="emit Markdown (also inferred from a .md output path)")
    p.add_argument(
        "--mode", choices=("section", "page"), default="section",
        help="structure: section-first (default) or page-first",
    )
    p.add_argument(
        "--image-mode", choices=("embed", "external", "drop"), default=None,
        help="figures: embed inline data: URIs, external img/ folder, or drop to placeholders "
        "(default: embed for HTML, external for Markdown)",
    )
    p.add_argument(
        "--no-toc", dest="include_toc", action="store_false",
        help="omit the auto table-of-contents from the output",
    )
    p.add_argument("--text", action="store_true", help="emit plain text instead of HTML")
    p.add_argument("--toc", action="store_true", help="print the table of contents")
    p.add_argument("--section", metavar="NAME", help="emit only the named section")
    p.add_argument(
        "--ocr", action="store_true",
        help="OCR scanned / image-only pages first (needs the 'distillpdf[ocr]' extra; "
        "downloads a model on first use). Output is a searchable PDF by default, or HTML/"
        "Markdown if -o ends in .html/.md (or --markdown/--text is given)",
    )
    p.add_argument(
        "--remove-raster", action="store_true",
        help="with --ocr and a PDF output: reflow scanned pages to clean text + cropped "
        "figures and drop the page raster (much smaller file) instead of keeping the scan "
        "with a hidden text layer",
    )
    p.add_argument("--version", action="version", version=f"distillpdf {__version__}")
    args = p.parse_args(argv)

    if args.ocr:
        return _run_ocr(args)

    multiple = len(args.pdf) > 1
    rc = 0
    for src in args.pdf:
        try:
            doc = _open(src)
            # Pass image_mode only when the user set it, so each method's own default
            # applies (embed for HTML, external for Markdown).
            img_kw = {} if args.image_mode is None else {"image_mode": args.image_mode}
            # --toc / --section / --text are string-only outputs; resolve the file format
            # for the rest so Markdown image extraction can target the right folder.
            if args.toc:
                content, dest = _toc(doc, args), _out_path(src, args, multiple, "text")
            elif args.section is not None:
                html = doc.section(args.section, mode=args.mode, **img_kw)
                if html is None:
                    raise SystemExit(f"distillpdf: no section matching {args.section!r}")
                content, dest = html, _out_path(src, args, multiple, "html")
            elif args.text:
                content, dest = doc.extract_text(), _out_path(src, args, multiple, "text")
            else:
                dest = _out_path(src, args, multiple, _fmt(args, args.output))
                fmt = _fmt(args, dest)
                if dest is None:
                    # stdout: get the rendered string (no file to write).
                    content = (
                        doc.to_markdown(mode=args.mode, toc=args.include_toc, return_string=True, **img_kw)
                        if fmt == "markdown"
                        else doc.to_html(mode=args.mode, toc=args.include_toc, return_string=True, **img_kw)
                    )
                else:
                    # Write directly through the library so img/ is extracted next to the file.
                    if fmt == "markdown":
                        doc.to_markdown(dest, mode=args.mode, toc=args.include_toc, **img_kw)
                    else:
                        doc.to_html(dest, mode=args.mode, toc=args.include_toc, **img_kw)
                    print(f"distillpdf: wrote {dest}", file=sys.stderr)
                    continue
        except SystemExit:
            raise
        except Exception as e:  # malformed PDF, unreadable path, etc.
            print(f"distillpdf: {src}: {e}", file=sys.stderr)
            rc = 1
            continue

        if dest is None:
            sys.stdout.write(content)
            if not content.endswith("\n"):
                sys.stdout.write("\n")
        else:
            with open(dest, "w", encoding="utf-8") as f:
                f.write(content)
            print(f"distillpdf: wrote {dest}", file=sys.stderr)
    return rc


def _ocr_fmt(args, dest):
    """OCR output format: .pdf/.md/.html from an explicit -o, else --markdown, else a
    searchable PDF (the flagship OCR output)."""
    if dest:
        low = dest.lower()
        if low.endswith(".pdf"):
            return "pdf"
        if low.endswith((".md", ".markdown")):
            return "md"
        if low.endswith((".html", ".htm")):
            return "html"
    return "md" if args.markdown else "pdf"


def _ocr_out_path(src, args, multiple, fmt):
    """Where one OCR output goes: explicit -o (file), a directory for batches, or
    ``<stem>.searchable.<ext>`` beside the source."""
    ext = {"pdf": ".searchable.pdf", "html": ".searchable.html", "md": ".searchable.md"}[fmt]
    stem = os.path.splitext(os.path.basename(src))[0]
    if args.output is None:
        return os.path.join(os.path.dirname(src), stem + ext)
    if multiple or os.path.isdir(args.output) or args.output.endswith(os.sep):
        os.makedirs(args.output, exist_ok=True)
        return os.path.join(args.output, stem + ext)
    return args.output


def _run_ocr(args):
    """`--ocr`: OCR each input's scanned pages (progress bar shown automatically) and write a
    searchable PDF (default) or OCR'd HTML/Markdown. The model is downloaded on first use and
    needs the ``distillpdf[ocr]`` extra; a missing-extra error is reported per file."""
    multiple = len(args.pdf) > 1
    img_kw = {} if args.image_mode is None else {"image_mode": args.image_mode}
    rc = 0
    for src in args.pdf:
        try:
            doc = _open(src)
            fmt = _ocr_fmt(args, None if multiple else args.output)
            dest = _ocr_out_path(src, args, multiple, fmt)
            n_scanned = sum(1 for p in doc.ocr_plan() if p["needs_ocr"])
            if n_scanned == 0:
                print(f"distillpdf: {src}: no scanned/image-only pages detected; nothing to OCR",
                      file=sys.stderr)
                continue
            print(f"distillpdf: {src}: OCR'ing {n_scanned} scanned page(s) -> {dest}",
                  file=sys.stderr)
            if fmt == "pdf":
                doc.to_pdf(dest, remove_raster=args.remove_raster, ocr=True)
            elif fmt == "md":
                doc.to_markdown(dest, ocr=True, **img_kw)
            else:
                doc.to_html(dest, ocr=True, **img_kw)
            print(f"distillpdf: wrote {dest}", file=sys.stderr)
        except Exception as e:  # missing [ocr] extra, malformed PDF, unreadable path, etc.
            print(f"distillpdf: {src}: {e}", file=sys.stderr)
            rc = 1
    return rc


def _toc(doc, args):
    """The table of contents as indented text."""
    def loc(page, anchor):
        return f"#{anchor}" if page == 0 else f"p{page}, #{anchor}"
    return "\n".join(
        f"{'  ' * (lvl - 1)}{title}  ({loc(page, anchor)})"
        for lvl, title, page, anchor in doc.toc(mode=args.mode)
    )


if __name__ == "__main__":
    sys.exit(main())
