"""The regression gate's verdict logic, on hand-built results: no corpus, no wheel needed.

Each test names the way the gate could wave a regression through. The end-to-end proof (a deliberate
break in the extractor turning the real gate red) is recorded in scripts/regression_gate/README.md,
because the corpus it needs is not in this repo.
"""
import copy
import importlib.util
import os

import pytest

_PATH = os.path.join(os.path.dirname(__file__), "..", "scripts", "regression_gate", "compare.py")
_SPEC = importlib.util.spec_from_file_location("regression_gate_compare", _PATH)
C = importlib.util.module_from_spec(_SPEC)
_SPEC.loader.exec_module(C)


def table_score(got, want):
    """Stand-in for bench100_score.table_score: 1.0 only on exact grid agreement."""
    same = [g[:2] for g in got] == [[w["rows"], w["cols"]] for w in want]
    return {"score": 1.0 if same else 0.5}


def op(sha="a", **kw):
    m = {"n": 3, "wall": [1.0, 0.9, 1.1], "cpu": [1.0, 0.9, 1.1],
         "peak": [100.0, 99.0, 101.0], "instr": [5000.0, 4990.0, 5010.0]}
    return {"sha": sha, "chars": 100, "page_count": 2, "m": m, **kw}


def doc():
    return {
        "ops": {"eager": {"text": op(), "pages": op(page_hashes=["p1", "p2"], empty_pages=[]),
                          "html": op(route="eager")}},
        "scores": {"eager": {"recall": 0.9, "order": 0.8, "headings": ["h1", "h2"],
                             "grids": [[3, 2, True]], "gt_headings": 0.5}},
    }


def run_of(d, baseline=None):
    return {"docs": {"x": d}, "scope": "all"}


def verdict(now, named=None):
    return C.compare({"docs": {"x": doc()}}, run_of(now), named or [], table_score)


def test_identical_run_passes():
    """Failure caught: the gate going red, or reporting changes, with nothing changed."""
    r = verdict(doc())
    assert (r["verdict"], r["exit"]) == ("PASS", 0)
    assert r["identical"] == ["x"] and not r["changed"]


def test_any_score_drop_fails_however_small():
    """Failure caught: a real drop hidden under a tolerance. There is none: -0.0001 fails."""
    now = doc()
    now["scores"]["eager"]["order"] = 0.7999
    now["ops"]["eager"]["text"]["sha"] = "b"
    r = verdict(now)
    assert r["exit"] == 1
    assert r["unnamed_drops"]["x"] == [("eager", "order", 0.8, 0.7999)]


def test_a_score_that_disappears_is_a_drop():
    """Failure caught: a scorer crash or an empty output reading as nothing to complain about."""
    now = doc()
    del now["scores"]["eager"]["recall"]
    assert verdict(now)["exit"] == 1


def test_changed_output_with_no_drop_is_not_a_quiet_pass():
    """Failure caught: scores are coarse, so a reorder they can't see would pass silently. It
    must come back non-zero until someone names it."""
    now = doc()
    now["ops"]["eager"]["text"]["sha"] = "b"
    r = verdict(now)
    assert (r["verdict"], r["exit"]) == ("UNNAMED", 3)
    assert r["unnamed"]["x"] == [("eager", "text", "a", "b")]


def test_name_matches_the_exact_hash_pair_only():
    """Failure caught: a name given to one change silently covering the next change."""
    named = [{"name": "n", "changes": {"x": [{"engine": "eager", "op": "text", "from": "a", "to": "b"}]}}]
    now = doc()
    now["ops"]["eager"]["text"]["sha"] = "b"
    assert verdict(now, named)["exit"] == 0
    now["ops"]["eager"]["text"]["sha"] = "c"
    assert verdict(now, named)["exit"] == 3


def test_naming_output_does_not_excuse_its_drop():
    """Failure caught: naming a change becoming a way past a score drop. A drop needs its own
    explicit entry, by metric and exact values."""
    named = [{"name": "n", "changes": {"x": [{"engine": "eager", "op": "text", "from": "a", "to": "b"}]}}]
    now = doc()
    now["ops"]["eager"]["text"]["sha"] = "b"
    now["scores"]["eager"]["recall"] = 0.8
    assert verdict(now, named)["exit"] == 1
    named[0]["accepted_drops"] = {"x": [{"engine": "eager", "metric": "recall", "from": 0.9, "to": 0.8}]}
    r = verdict(now, named)
    assert r["exit"] == 0 and r["named_drops"]["x"]
    now["scores"]["eager"]["recall"] = 0.7
    assert verdict(now, named)["exit"] == 1


@pytest.mark.parametrize("mutate, phrase", [
    (lambda d: d["ops"]["eager"]["text"].update(page_count=1), "page count"),
    (lambda d: d["ops"]["eager"]["text"].update(chars=0), "empty"),
    (lambda d: d["ops"]["eager"]["pages"].update(empty_pages=[2]), "lost on page"),
    (lambda d: d["ops"]["eager"].update(text={"error": "PdfError: broken"}), "now fails"),
    (lambda d: d["ops"]["eager"].update(text={"crash": "exit -11"}), "now fails"),
])
def test_lost_documents_and_pages_fail_and_cannot_be_named(mutate, phrase):
    """Failure caught: losing a page or a document without any score noticing. Naming every
    op hash must not rescue it."""
    now = doc()
    mutate(now)
    named = [{"name": "n", "changes": {"x": [
        {"engine": "eager", "op": o, "from": "a", "to": r.get("sha", "error")}
        for o, r in now["ops"]["eager"].items()]}}]
    r = verdict(now, named)
    assert r["exit"] == 1
    assert any(phrase in line for line in r["lost"]["x"])


def test_document_missing_from_a_full_run_fails():
    """Failure caught: a crash in the harness shrinking the corpus and everything still matching."""
    r = C.compare({"docs": {"x": doc(), "y": doc()}}, {"docs": {"x": doc()}, "scope": "all"}, [], table_score)
    assert r["exit"] == 1 and r["missing"] == ["y"]


def test_fast_subset_may_leave_documents_out():
    """Failure caught: the fast mode failing for the documents it deliberately skips."""
    r = C.compare({"docs": {"x": doc(), "y": doc()}}, {"docs": {"x": doc()}, "scope": "subset"}, [], table_score)
    assert r["exit"] == 0


def test_dropped_heading_on_an_unjudged_document_fails():
    """Failure caught: a dropped heading rule on a document with no ground truth, where recall
    is unchanged because the text is still there as a paragraph."""
    now = doc()
    now["scores"]["eager"]["headings"] = ["h1"]
    r = verdict(now)
    assert r["exit"] == 1
    assert ("eager", "headings_kept", 1.0, 0.5) in r["unnamed_drops"]["x"]


def test_collapsed_table_fails():
    """Failure caught: a table keeping its count and losing its columns."""
    now = doc()
    now["scores"]["eager"]["grids"] = [[3, 1, True]]
    assert verdict(now)["exit"] == 1


def test_gt_scores_not_run_are_not_read_as_zero():
    """Failure caught: fast mode, which skips the judged-page scorer, reporting every judged
    document as a drop."""
    now = doc()
    del now["scores"]["eager"]["gt_headings"]
    now["scores"]["eager"]["_gt_not_run"] = True
    assert verdict(now)["exit"] == 0


def test_footprint_growth_is_reported_above_tolerance_only_and_never_fails():
    """Failure caught: a memory increase going unreported, and repeat noise being reported as one."""
    grown = doc()
    grown["ops"]["eager"]["text"]["m"]["peak"] = [120.0, 119.0, 121.0]
    r = verdict(grown)
    assert r["footprint"]["x"][0][:2] == ("eager", "text") and r["exit"] == 0
    small = doc()
    small["ops"]["eager"]["text"]["m"]["peak"] = [101.0, 100.0, 102.0]
    assert not verdict(small)["footprint"]


def test_time_compares_instructions_not_the_wall_clock():
    """Failure caught: a busy machine reading as a slowdown. Wall clock tripled by other agents'
    builds must not flag; retired instructions growing must."""
    busy = doc()
    busy["ops"]["eager"]["text"]["m"]["wall"] = [3.0, 2.9, 3.1]
    assert not verdict(busy)["time"]
    slower = doc()
    slower["ops"]["eager"]["text"]["m"]["instr"] = [6000.0, 5990.0, 6010.0]
    assert verdict(slower)["time"]["x"][0][:3] == ("eager", "text", "Minstr")


def test_one_repeat_reports_no_noise_figure():
    """Failure caught: printing 0% noise from a single measurement."""
    one = copy.deepcopy(doc())
    for ops in one["ops"].values():
        for o in ops.values():
            o["m"]["n"] = 1
    assert C.noise_summary({"docs": {"x": one}}) == {}


def test_diff_is_cut_rather_than_filling_the_disk(tmp_path):
    """Failure caught: a 139 MiB diff per operation, from one-line-per-page HTML, on every run."""
    spec = importlib.util.spec_from_file_location(
        "regression_gate_main", os.path.join(os.path.dirname(_PATH), "gate.py"))
    gate = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(gate)
    a, b = tmp_path / "a", tmp_path / "b"
    a.write_text("x" * 200_000 + "\n")
    b.write_text("y" * 200_000 + "\n")
    out = gate.capped_git_diff(str(a), str(b), cap=1000)
    assert len(out) < 1100 and b"diff cut at 1000 bytes" in out
    assert b"diff cut" not in gate.capped_git_diff(str(a), str(b))
