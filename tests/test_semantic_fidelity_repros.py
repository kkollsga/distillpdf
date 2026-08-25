"""Owned regressions for semantic-table, list, and page-chrome fidelity defects."""
import os
import re
import json

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


def test_numeric_and_nested_marker_rails_form_nonempty_items():
    rendered = html("list_marker_rail.pdf")
    page = re.search(
        r'<section data-page="3"[^>]*>.*?</section>', rendered, re.DOTALL
    ).group(0)
    gt = GT["list_marker_rail.pdf"]
    assert cells(page, "li") == gt["numeric"] + gt["nested"]
    assert "<table" not in page


def test_numeric_marker_fixture_preserves_following_prose():
    rendered = html("list_marker_rail.pdf")
    assert GT["list_marker_rail.pdf"]["after_numeric"] in text(rendered)


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


def test_running_logo_rule_chain_does_not_emit_figures():
    rendered = html("logo_rule_chain.pdf")
    assert rendered.count("<figure") == 0
    assert rendered.count("<table") == GT["logo_rule_chain.pdf"]["tables"]


def test_logo_rule_chain_fixture_keeps_tables_and_body_content():
    rendered = html("logo_rule_chain.pdf")
    gt = GT["logo_rule_chain.pdf"]
    assert rendered.count("<table") >= gt["pages"] - gt["near_pages"]
    assert text(rendered).count(gt["body"]) == gt["pages"]


def test_mixed_image_text_cells_remain_one_semantic_table():
    pdf = distillpdf.Pdf.open(os.path.join(FIX, "mixed_cell_table.pdf"))
    rendered = pdf.to_html(mode="page", image_mode="embed", return_string=True)
    gt = GT["mixed_cell_table.pdf"]
    assert rendered.count("<table") == 1
    table = re.search(r"<table\b.*?</table>", rendered, re.DOTALL).group(0)
    assert len(cells(table, "th")) + len(cells(table, "td")) == gt["rows"] * gt["cols"]
    assert table.count("<img ") == gt["images"]
    assert all(label in table for label in gt["labels"])
    assert re.search(rf"<caption\b[^>]*>{re.escape(gt['caption'])}</caption>", table)
    assert rendered.count("<figure") == 0
    dropped = pdf.to_html(mode="page", image_mode="drop", return_string=True)
    assert re.findall(r'data-asset="([^"]+)', dropped) == re.findall(
        r'data-asset="([^"]+)', rendered)


def test_mixed_cell_fixture_retains_all_source_components():
    pdf = distillpdf.Pdf.open(os.path.join(FIX, "mixed_cell_table.pdf"))
    gt = GT["mixed_cell_table.pdf"]
    assert len(pdf.extract_images()) == gt["assets"]
    visible = text(pdf.to_html(mode="page", image_mode="drop", return_string=True))
    assert gt["caption"] in visible
    assert all(label in visible for label in gt["labels"])


def test_mixed_image_text_cells_are_structured_in_raw_analysis():
    pdf = distillpdf.Pdf.open(os.path.join(FIX, "mixed_cell_table.pdf"))
    gt = GT["mixed_cell_table.pdf"]
    analyzed = pdf.analyze_tables()
    assert len(analyzed) == 1
    table = analyzed[0]
    assert (table["n_rows"], table["n_cols"]) == (gt["rows"], gt["cols"])
    anchors = table["cells"]
    assert len(anchors) == gt["rows"] * gt["cols"]
    assert all(cell["content"]["text"] == cell["text"] for cell in anchors)
    images = [image for cell in anchors for image in cell["content"]["images"]]
    assert len(images) == gt["images"]
    assert len({image["asset"] for image in images}) == gt["assets"]
    assert all(len(image["bbox_norm"]) == 4 and image["order"] >= 0 for image in images)


def test_mixed_image_text_cells_roundtrip_through_model(tmp_path):
    pdf = distillpdf.Pdf.open(os.path.join(FIX, "mixed_cell_table.pdf"))
    gt = GT["mixed_cell_table.pdf"]
    live = pdf.to_html(mode="page", toc=False, image_mode="drop", return_string=True)
    dpdf = pdf.distill(str(tmp_path / "mixed-cell.dpdf"))
    assert distillpdf.render_html(dpdf, mode="page", toc=False) == live

    model = json.loads(distillpdf.load_model(dpdf))
    blocks = [block for block in model["blocks"] if block["kind"] == "table"]
    assert len(blocks) == 1
    content = blocks[0]["table_cell_content"]
    images = [image for row in content for cell in row for image in cell["images"]]
    assert len(images) == gt["images"]
    assert len({image["asset"] for image in images}) == gt["assets"]
    assert len(model["assets"]) == gt["assets"]
    assert model["assets"][0]["kind"] == "table_cell"
    assert model["assets"][0]["storage"] == "embedded"

    markdown = distillpdf.render_markdown(
        dpdf, mode="page", toc=False, image_mode="drop")
    assert all(label in markdown for label in gt["labels"])
    assert markdown.count("![](#fig-") == gt["images"]


def test_mixed_sidecar_column_is_one_semantic_table(tmp_path):
    pdf = distillpdf.Pdf.open(os.path.join(FIX, "mixed_sidecar_table.pdf"))
    gt = GT["mixed_sidecar_table.pdf"]
    rendered = pdf.to_html(mode="page", toc=False, image_mode="drop", return_string=True)
    assert rendered.count("<table") == 1
    assert rendered.count("<figure") == 0
    table = re.search(r"<table\b.*?</table>", rendered, re.DOTALL).group(0)
    assert len(cells(table, "th")) + len(cells(table, "td")) == gt["rows"] * gt["cols"]
    assert table.count("<img ") == gt["images"]
    assert re.search(rf"<caption\b[^>]*>{re.escape(gt['caption'])}</caption>", table)

    analyzed = pdf.analyze_tables()
    assert len(analyzed) == 1
    assert (analyzed[0]["n_rows"], analyzed[0]["n_cols"]) == (gt["rows"], gt["cols"])
    images = [
        image
        for cell in analyzed[0]["cells"]
        for image in cell.get("content", {}).get("images", [])
    ]
    assert len(images) == gt["images"]

    dpdf = pdf.distill(str(tmp_path / "mixed-sidecar.dpdf"))
    assert distillpdf.render_html(dpdf, mode="page", toc=False) == rendered
    model = json.loads(distillpdf.load_model(dpdf))
    blocks = [block for block in model["blocks"] if block["kind"] == "table"]
    assert len(blocks) == 1
    content = blocks[0]["table_cell_content"]
    durable_images = [
        image for row in content for cell in row for image in cell.get("images", [])
    ]
    assert len(durable_images) == gt["images"]
    assert len({image["asset"] for image in durable_images}) == gt["assets"]
    assert len(model["assets"]) == gt["assets"]
    assert model["assets"][0]["kind"] == "table_cell"


def test_mixed_sidecar_fixture_retains_source_components():
    pdf = distillpdf.Pdf.open(os.path.join(FIX, "mixed_sidecar_table.pdf"))
    gt = GT["mixed_sidecar_table.pdf"]
    assert len(pdf.extract_images()) == gt["assets"]
    extracted = pdf.extract_text()
    assert all(value in extracted for value in gt["headers"] + gt["labels"])
    assert gt["caption"] in extracted
