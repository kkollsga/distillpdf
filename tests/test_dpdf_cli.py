"""The `.dpdf` document shell (engine track, Wave 3): the CLI verbs `info / toc / read /
find / tables / figures / ocr-status` that apply when the CLI input is a distilled `.dpdf`
instead of a source PDF.

These pin the agent-ergonomics contract from docs/datamodel-design.md ("The agent CLI"):

  * every listing emits ids that thread into the next call (toc id → read);
  * `read` bounds its output and truncates HONESTLY — it says where it stopped and how to
    resume, and `--from` continues with no gap or overlap;
  * `find` never silently truncates and always ends with a coverage line; it scopes by
    section/kind/pages and supports --regex;
  * `--json` is valid JSON on every verb;
  * errors are actionable (an unknown id suggests the closest ones);
  * a plain PDF still drives the converter (the existing convert CLI is untouched).

The shell drives cli.main() in-process (no subprocess), like test_cli.py. Fixtures are the
owned, committed PDFs distilled to a .dpdf in tmp_path (like test_model.py).
"""
import json
import os

import pytest

import distillpdf
from distillpdf import cli
from distillpdf.dpdf import Model
from _fixtures import FIX

SEC_STRUCT = os.path.join(FIX, "sec_structure.pdf")
NUMERIC = os.path.join(FIX, "numeric.pdf")
FIGURES = os.path.join(FIX, "figures.pdf")
FOOTNOTES = os.path.join(FIX, "footnotes.pdf")


def _dpdf(pdf, tmp_path, name="m.dpdf"):
    return distillpdf.Pdf.open(pdf).distill(os.path.join(str(tmp_path), name))


# ---- info -------------------------------------------------------------------

def test_info_human(tmp_path, capsys):
    d = _dpdf(SEC_STRUCT, tmp_path)
    rc = cli.main([d, "info"])
    out = capsys.readouterr().out
    assert rc == 0
    assert "sec_structure.pdf" in out
    assert "schema v0" in out
    assert "pages:" in out and "sections:" in out
    assert "OCR: none" in out  # born-digital


def test_info_json_valid(tmp_path, capsys):
    d = _dpdf(SEC_STRUCT, tmp_path)
    cli.main([d, "info", "--json"])
    obj = json.loads(capsys.readouterr().out)
    assert obj["schema_version"] == 0
    assert obj["pages"] == distillpdf.Pdf.open(SEC_STRUCT).page_count()
    assert len(obj["source"]["sha256_prefix"]) == 12  # prefix, not the full hash
    assert obj["sections"] >= 1 and "tables" in obj and "figures" in obj


def test_info_counts_tables(tmp_path, capsys):
    d = _dpdf(NUMERIC, tmp_path)
    cli.main([d, "info", "--json"])
    obj = json.loads(capsys.readouterr().out)
    assert obj["tables"] == 1


# ---- toc + id threading -----------------------------------------------------

def test_toc_emits_section_ids(tmp_path, capsys):
    d = _dpdf(SEC_STRUCT, tmp_path)
    cli.main([d, "toc"])
    out = capsys.readouterr().out
    assert "sec-" in out
    # the ids the toc emits are exactly the model's section ids.
    model = Model.load(d)
    for sid in model.section_ids():
        assert sid in out


def test_toc_id_threads_into_read(tmp_path, capsys):
    """A toc id is directly usable as a read target — one call's output is the next's input."""
    d = _dpdf(SEC_STRUCT, tmp_path)
    cli.main([d, "toc", "--json"])
    sid = json.loads(capsys.readouterr().out)["sections"][0]["id"]
    rc = cli.main([d, "read", sid])
    out = capsys.readouterr().out
    assert rc == 0 and out.strip()
    assert "prev:" in out and "next:" in out and "parent:" in out  # breadcrumbs


# ---- read: sections, blocks, pages -----------------------------------------

def test_read_block_table_as_pipe_markdown(tmp_path, capsys):
    d = _dpdf(NUMERIC, tmp_path)
    cli.main([d, "tables", "--json"])
    tid = json.loads(capsys.readouterr().out)["tables"][0]["id"]
    cli.main([d, "read", tid])
    out = capsys.readouterr().out
    assert "| System |" in out and "| --- |" in out  # GitHub pipe table
    assert "84.6" in out


def test_read_pages_range(tmp_path, capsys):
    d = _dpdf(SEC_STRUCT, tmp_path)
    cli.main([d, "read", "--pages", "1", "--max-chars", "100000"])
    out = capsys.readouterr().out
    assert "Acme Corporation Annual Report" in out


def test_read_breadcrumbs_present(tmp_path, capsys):
    d = _dpdf(SEC_STRUCT, tmp_path)
    model = Model.load(d)
    sid = model.section_ids()[1]  # a non-first section so prev resolves
    cli.main([d, "read", sid])
    out = capsys.readouterr().out
    assert "prev:" in out and "·" in out


# ---- read: honest truncation + resume --------------------------------------

def test_read_truncation_is_honest_and_resumable(tmp_path, capsys):
    """A small --max-chars truncates with an explicit 'showing … continue: … --from <id>'
    line, and --from resumes with NO gap and NO overlap (the resume token is shown next)."""
    d = _dpdf(SEC_STRUCT, tmp_path)
    parent = Model.load(d).section_ids()[0]

    cli.main([d, "read", parent, "--max-chars", "120"])
    first = capsys.readouterr().out
    assert "showing blocks" in first and "continue:" in first and "--from" in first
    resume_id = first.split("--from", 1)[1].strip().split()[0]

    # collect the block ids shown across the resumed reads; they must tile the section's
    # blocks contiguously (resume token reappears as the first block of the next page).
    cli.main([d, "read", parent, "--max-chars", "120", "--json"])
    page1 = json.loads(capsys.readouterr().out)
    assert page1["truncated"] and page1["next"] == resume_id
    cli.main([d, "read", parent, "--max-chars", "120", "--from", resume_id, "--json"])
    page2 = json.loads(capsys.readouterr().out)
    # the resume id is the first block of page 2 (inclusive resume), and page-1's ids end
    # right before it — no gap, no overlap.
    assert page2["block_ids"][0] == resume_id
    assert resume_id not in page1["block_ids"]
    assert page1["block_ids"][-1] != resume_id


def test_read_resume_walks_to_completion(tmp_path, capsys):
    """Resuming repeatedly visits every block of the section exactly once, in order."""
    d = _dpdf(SEC_STRUCT, tmp_path)
    model = Model.load(d)
    parent = model.section_ids()[0]
    full = [b["id"] for b in model.blocks_for_section(parent)]

    seen = []
    frm = None
    for _ in range(50):  # bounded guard against a non-progressing loop
        argv = [d, "read", parent, "--max-chars", "100", "--json"]
        if frm:
            argv += ["--from", frm]
        cli.main(argv)
        page = json.loads(capsys.readouterr().out)
        seen += page["block_ids"]
        if not page["truncated"]:
            break
        frm = page["next"]
    assert seen == full  # every block, once, in reading order


def test_read_bad_from_token_errors(tmp_path, capsys):
    d = _dpdf(SEC_STRUCT, tmp_path)
    rc = cli.main([d, "read", "--pages", "1", "--from", "b9999"])
    assert rc == 1
    assert "not in this selection" in capsys.readouterr().err


# ---- tables / figures -------------------------------------------------------

def test_tables_listing_threads_id(tmp_path, capsys):
    d = _dpdf(NUMERIC, tmp_path)
    cli.main([d, "tables"])
    out = capsys.readouterr().out
    assert out.strip().startswith("b")  # a block id leads each row
    cli.main([d, "tables", "--json"])
    rows = json.loads(capsys.readouterr().out)["tables"]
    assert rows and rows[0]["id"] in {b["id"] for b in Model.load(d).blocks}


def test_figures_listing(tmp_path, capsys):
    d = _dpdf(FIGURES, tmp_path)
    cli.main([d, "figures", "--json"])
    rows = json.loads(capsys.readouterr().out)["figures"]
    assert rows
    assert any(r.get("label") for r in rows)
    assert all(r["page"] >= 1 for r in rows)


def test_figures_empty_when_none(tmp_path, capsys):
    d = _dpdf(NUMERIC, tmp_path)
    cli.main([d, "figures"])
    assert "(no figures)" in capsys.readouterr().out


# ---- find -------------------------------------------------------------------

def test_find_basic_and_coverage_line(tmp_path, capsys):
    d = _dpdf(SEC_STRUCT, tmp_path)
    cli.main([d, "find", "business"])
    out = capsys.readouterr().out
    assert "«business»" in out.lower() or "«Business»" in out  # match highlighted
    assert "searched" in out and "blocks across" in out and "pages" in out  # coverage line


def test_find_case_insensitive_substring(tmp_path, capsys):
    d = _dpdf(SEC_STRUCT, tmp_path)
    cli.main([d, "find", "BUSINESS", "--json"])
    obj = json.loads(capsys.readouterr().out)
    assert obj["total_matches"] >= 1


def test_find_no_match_is_measured(tmp_path, capsys):
    d = _dpdf(SEC_STRUCT, tmp_path)
    cli.main([d, "find", "zzz_no_such_phrase_qqq"])
    out = capsys.readouterr().out
    assert "no matches" in out
    assert "searched" in out  # "not found" paired with how much was searched


def test_find_regex(tmp_path, capsys):
    d = _dpdf(NUMERIC, tmp_path)
    cli.main([d, "find", r"\d+\.\d", "--regex", "--json"])
    obj = json.loads(capsys.readouterr().out)
    assert obj["regex"] is True
    assert obj["total_matches"] >= 1  # the table has decimals like 84.6


def test_find_bad_regex_errors(tmp_path, capsys):
    d = _dpdf(NUMERIC, tmp_path)
    rc = cli.main([d, "find", "(unclosed", "--regex"])
    assert rc == 1
    assert "invalid regex" in capsys.readouterr().err


def test_find_section_scope(tmp_path, capsys):
    d = _dpdf(SEC_STRUCT, tmp_path)
    model = Model.load(d)
    # scoping to a leaf section reaches fewer blocks than the whole document.
    cli.main([d, "find", "the", "--json"])
    whole = json.loads(capsys.readouterr().out)["coverage"]["searched_blocks"]
    leaf = next(s for s in model.sections if s["id"] == "sec-item-1.-business")
    cli.main([d, "find", "the", "--section", leaf["id"], "--json"])
    scoped = json.loads(capsys.readouterr().out)["coverage"]["searched_blocks"]
    assert 0 < scoped < whole


def test_find_kind_scope(tmp_path, capsys):
    d = _dpdf(NUMERIC, tmp_path)
    cli.main([d, "find", "84.6", "--kind", "table", "--json"])
    obj = json.loads(capsys.readouterr().out)
    assert obj["total_matches"] >= 1
    assert all(m["kind"] == "table" for m in obj["matches"])


def test_find_unknown_section_suggests(tmp_path, capsys):
    d = _dpdf(SEC_STRUCT, tmp_path)
    rc = cli.main([d, "find", "x", "--section", "sec-bogus"])
    assert rc == 1
    assert "closest section ids" in capsys.readouterr().err


def test_find_limit_never_silent(tmp_path, capsys):
    d = _dpdf(SEC_STRUCT, tmp_path)
    # "e" matches many blocks; --limit 1 must hold back the rest visibly.
    cli.main([d, "find", "e", "--limit", "1"])
    out = capsys.readouterr().out
    assert "showing 1 of" in out and "raise --limit" in out


def test_find_context_lists_neighbours(tmp_path, capsys):
    d = _dpdf(SEC_STRUCT, tmp_path)
    cli.main([d, "find", "business", "--context", "1", "--json"])
    obj = json.loads(capsys.readouterr().out)
    assert any(m.get("context") for m in obj["matches"])


# ---- ocr-status -------------------------------------------------------------

def test_ocr_status_none_recorded(tmp_path, capsys):
    d = _dpdf(SEC_STRUCT, tmp_path)
    cli.main([d, "ocr-status"])
    assert "no OCR passes recorded" in capsys.readouterr().out


def test_ocr_status_json_valid(tmp_path, capsys):
    d = _dpdf(SEC_STRUCT, tmp_path)
    cli.main([d, "ocr-status", "--json"])
    obj = json.loads(capsys.readouterr().out)
    assert obj["pages"] == [] and obj["passes"] == []  # born-digital → empty


# ---- --json validity on every verb -----------------------------------------

@pytest.mark.parametrize("verb", ["info", "toc", "tables", "figures", "ocr-status"])
def test_every_listing_verb_json_loads(tmp_path, capsys, verb):
    d = _dpdf(FIGURES, tmp_path)
    cli.main([d, verb, "--json"])
    json.loads(capsys.readouterr().out)  # raises if not valid JSON


def test_read_json_loads(tmp_path, capsys):
    d = _dpdf(SEC_STRUCT, tmp_path)
    sid = Model.load(d).section_ids()[0]
    cli.main([d, "read", sid, "--json"])
    obj = json.loads(capsys.readouterr().out)
    assert "markdown" in obj and "block_ids" in obj


# ---- errors: not a .dpdf ----------------------------------------------------

def test_unknown_id_suggests_closest(tmp_path, capsys):
    d = _dpdf(SEC_STRUCT, tmp_path)
    rc = cli.main([d, "read", "sec-acme-corp"])  # a near-miss of the real id
    assert rc == 1
    err = capsys.readouterr().err
    assert "unknown id" in err and "closest section ids" in err


def test_dpdf_verb_on_nonexistent_file(tmp_path, capsys):
    rc = cli.main([os.path.join(str(tmp_path), "missing.dpdf"), "info"])
    assert rc == 1
    err = capsys.readouterr().err
    assert "missing.dpdf" in err
    assert "make one with" in err  # actionable: how to produce a .dpdf


# ---- page-label resolution (model-level; no labeled PDF fixture exists) -----

def test_page_label_resolution_unit():
    """Page labels resolve both ways: a label token (e.g. 'xii') maps to its physical page,
    and read/find page suffixes show the label. Exercised at the Model level because no owned
    PDF carries a /PageLabels tree."""
    data = {
        "source": {"file": "x.pdf", "sha256": "abc", "pages": 2, "distillpdf": "0", "generated_at": "Z"},
        "metadata": {},
        "pages": [
            {"n": 1, "width_pts": 1, "height_pts": 1, "labels": {"pdf": "xii"}},
            {"n": 2, "width_pts": 1, "height_pts": 1, "labels": {"pdf": "xiii"}},
        ],
        "sections": [],
        "blocks": [],
        "indexes": {"pages": {}, "sections": {}, "kinds": {}, "coverage": {"sectioned": 1.0}},
    }
    m = Model.from_dict(data)
    assert m.resolve_page_token("xii") == 1
    assert m.resolve_page_token("2") == 2
    assert m.resolve_page_range("xii-xiii") == [1, 2]
    assert m.page_label_suffix(1) == " (xii)"
    assert m.page_label_suffix(2) == " (xiii)"


# ---- plain-PDF passthrough still intact -------------------------------------

def test_plain_pdf_convert_untouched(tmp_path, capsys):
    """A source PDF still drives the converter — the shell routing must not capture it."""
    cli.main([SEC_STRUCT])
    out = capsys.readouterr().out
    assert out.startswith("<!doctype html>") and "</html>" in out


def test_pdf_named_like_verb_still_converts(capsys):
    """The routing requires BOTH a .dpdf extension AND a known verb; a PDF path never trips it
    even if a later arg happens to look like a verb."""
    cli.main([SEC_STRUCT, "--text"])
    out = capsys.readouterr().out
    assert "<html>" not in out and "Acme" in out
