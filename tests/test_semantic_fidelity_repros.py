"""Owned reproductions for residual semantic-table, list, and page-chrome defects.

The strict expected failures are Phase 1 evidence, not accepted behavior.  Each is removed
by the phase that fixes its production decision path.  Precision controls stay ordinary
tests throughout so a broad suppression/reordering change cannot make an xfail disappear by
deleting unrelated content.
"""
import os
import re

import pytest
import distillpdf

from _fixtures import FIX, GT, cells, html, text


TABLE_CORPUS = os.path.join(os.path.dirname(__file__), "table_corpus")


def test_close_table_label_is_its_caption():
    pdf = distillpdf.Pdf.open(
        os.path.join(TABLE_CORPUS, "t3_crosspage_independent_caption.pdf"))
    rendered = pdf.to_html(mode="page", image_mode="drop", return_string=True)
    assert re.search(
        r'<table id="tab-8"[^>]*>\s*<caption[^>]*>Table 8\. Audit outcomes</caption>',
        rendered,
    )


def test_close_caption_fixture_keeps_both_tables_and_labels_once():
    pdf = distillpdf.Pdf.open(
        os.path.join(TABLE_CORPUS, "t3_crosspage_independent_caption.pdf"))
    rendered = pdf.to_html(mode="page", image_mode="drop", return_string=True)
    assert rendered.count("<table") == 2
    assert rendered.count("Table 7. Permit register") == 1
    assert rendered.count("Table 8. Audit outcomes") == 1


def test_detached_marker_rail_forms_nonempty_items():
    rendered = html("list_marker_rail.pdf")
    page = re.search(r'<section data-page="1".*?</section>', rendered, re.DOTALL).group(0)
    items = cells(page, "li")
    assert items == GT["list_marker_rail.pdf"]["items"]
    assert all(items)


def test_detached_marker_fixture_preserves_true_columns_and_following_prose():
    rendered = html("list_marker_rail.pdf")
    visible = text(rendered)
    gt = GT["list_marker_rail.pdf"]
    assert gt["after"] in visible
    positions = [visible.index(value) for value in gt["column_order"]]
    assert positions == sorted(positions)


@pytest.mark.xfail(strict=True, reason="whole-row recurrence spares the singleton footer key")
def test_variable_prefix_footer_is_removed_by_its_recurring_tail():
    rendered = html("variable_footer_chrome.pdf")
    visible = text(rendered)
    gt = GT["variable_footer_chrome.pdf"]
    assert gt["singleton"] not in visible
    assert "of 12" not in visible


def test_variable_footer_fixture_preserves_body_and_bottom_band_footnote():
    rendered = html("variable_footer_chrome.pdf")
    visible = text(rendered)
    gt = GT["variable_footer_chrome.pdf"]
    assert gt["footnote"] in visible
    assert "Distinct body prose for synthetic page 1 remains visible." in visible
    assert "Distinct body prose for synthetic page 12 remains visible." in visible


@pytest.mark.xfail(strict=True, reason="logo curves borrow strong-figure extent from nearby rules")
def test_running_logo_rule_chain_does_not_emit_figures():
    rendered = html("logo_rule_chain.pdf")
    assert rendered.count("<figure") == 0
    assert rendered.count("<table") == GT["logo_rule_chain.pdf"]["tables"]


def test_logo_rule_chain_fixture_keeps_tables_and_body_content():
    rendered = html("logo_rule_chain.pdf")
    gt = GT["logo_rule_chain.pdf"]
    assert rendered.count("<table") >= gt["pages"] - gt["near_pages"]
    assert text(rendered).count(gt["body"]) == gt["pages"]


@pytest.mark.xfail(strict=True, reason="raster-bearing table is discarded and cells are text-only")
def test_mixed_image_text_cells_remain_one_semantic_table():
    rendered = distillpdf.Pdf.open(os.path.join(FIX, "mixed_cell_table.pdf")).to_html(
        mode="page", image_mode="inline", return_string=True)
    gt = GT["mixed_cell_table.pdf"]
    assert rendered.count("<table") == 1
    table = re.search(r"<table\b.*?</table>", rendered, re.DOTALL).group(0)
    assert len(cells(table, "td")) == gt["rows"] * gt["cols"]
    assert table.count("<img ") == gt["images"]
    assert all(label in table for label in gt["labels"])
    assert f"<caption>{gt['caption']}</caption>" in table


def test_mixed_cell_fixture_retains_all_source_components_before_fix():
    pdf = distillpdf.Pdf.open(os.path.join(FIX, "mixed_cell_table.pdf"))
    gt = GT["mixed_cell_table.pdf"]
    assert len(pdf.extract_images()) == gt["assets"]
    visible = text(pdf.to_html(mode="page", image_mode="drop", return_string=True))
    assert gt["caption"] in visible
    assert all(label in visible for label in gt["labels"])
