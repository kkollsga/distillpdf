"""Semantic HTML header depth, distinct from table cell placement/content metrics."""

import re

from _fixtures import html


def leading_header_rows(name):
    depths = []
    for table in re.findall(r"<table\b.*?</table>", html(name), re.DOTALL):
        depth = 0
        for row in re.findall(r"<tr\b.*?</tr>", table, re.DOTALL):
            tags = re.findall(r"<(t[dh])\b", row)
            if tags and all(tag == "th" for tag in tags):
                depth += 1
            else:
                break
        depths.append(depth)
    return depths


def test_a_uniform_parameter_table_has_one_semantic_header_row():
    assert leading_header_rows("alpha_header_data_table.pdf") == [1]


def test_a_genuine_multitier_header_keeps_both_semantic_rows():
    assert leading_header_rows("multitier_header_table.pdf") == [2]


def test_section_breaks_do_not_reclassify_prior_runs_as_headers():
    depths = leading_header_rows("header_backwalk_table.pdf")
    assert depths and all(n == 1 for n in depths), depths


def test_table_boundaries_keep_ordinary_single_headers():
    assert leading_header_rows("heading_ends_table.pdf") == [1, 1]
    assert leading_header_rows("stacked_tables.pdf") == [1, 1]
