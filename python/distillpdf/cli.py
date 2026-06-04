"""Command-line interface: convert PDFs to clean, LLM-ready HTML in one command.

    distillpdf paper.pdf                  # HTML to stdout
    distillpdf paper.pdf -o paper.html    # HTML to a file
    distillpdf *.pdf -o out/              # batch: out/<name>.html per input
    distillpdf paper.pdf --mode page      # page-first HTML (default is section-first)
    distillpdf paper.pdf --no-images      # <image N> placeholders, no base64
    distillpdf paper.pdf --no-toc         # omit the table-of-contents nav
    distillpdf paper.pdf --text           # plain text instead of HTML
    distillpdf paper.pdf --toc            # table of contents
    distillpdf paper.pdf --section abstract
"""
import argparse
import os
import sys

from . import __version__, open as _open


def _render(doc, args):
    """The chosen output for one opened document, as a string."""
    if args.toc:
        # Section mode carries no page numbers (page == 0); show just the anchor there.
        def loc(page, anchor):
            return f"#{anchor}" if page == 0 else f"p{page}, #{anchor}"
        return "\n".join(
            f"{'  ' * (lvl - 1)}{title}  ({loc(page, anchor)})"
            for lvl, title, page, anchor in doc.toc(mode=args.mode)
        )
    if args.section is not None:
        html = doc.section(args.section, mode=args.mode, images=args.images)
        if html is None:
            raise SystemExit(f"distillpdf: no section matching {args.section!r}")
        return html
    if args.text:
        return doc.extract_text()
    return doc.to_html(mode=args.mode, images=args.images, toc=args.include_toc)


def _out_path(src, args, multiple):
    """Where one input's output goes: explicit -o, a dir for batches, or None (stdout)."""
    ext = ".txt" if args.text else ".html"
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
        description="Convert PDFs into clean, LLM-ready HTML (or plain text).",
    )
    p.add_argument("pdf", nargs="+", help="input PDF file(s)")
    p.add_argument(
        "-o", "--output",
        help="output file (single input) or directory (multiple inputs); "
        "default: stdout for one PDF, <name>.html beside each for many",
    )
    p.add_argument(
        "--mode", choices=("section", "page"), default="section",
        help="HTML structure: section-first (default) or page-first",
    )
    p.add_argument(
        "--no-images", dest="images", action="store_false",
        help="replace embedded images with <image N> placeholders (no base64)",
    )
    p.add_argument(
        "--no-toc", dest="include_toc", action="store_false",
        help="omit the auto table-of-contents <nav> from the HTML output",
    )
    p.add_argument("--text", action="store_true", help="emit plain text instead of HTML")
    p.add_argument("--toc", action="store_true", help="print the table of contents")
    p.add_argument("--section", metavar="NAME", help="emit only the named section")
    p.add_argument("--version", action="version", version=f"distillpdf {__version__}")
    args = p.parse_args(argv)

    multiple = len(args.pdf) > 1
    rc = 0
    for src in args.pdf:
        try:
            doc = _open(src)
            content = _render(doc, args)
        except SystemExit:
            raise
        except Exception as e:  # malformed PDF, unreadable path, etc.
            print(f"distillpdf: {src}: {e}", file=sys.stderr)
            rc = 1
            continue

        dest = _out_path(src, args, multiple)
        if dest is None:
            sys.stdout.write(content)
            if not content.endswith("\n"):
                sys.stdout.write("\n")
        else:
            with open(dest, "w", encoding="utf-8") as f:
                f.write(content)
            print(f"distillpdf: wrote {dest}", file=sys.stderr)
    return rc


if __name__ == "__main__":
    sys.exit(main())
