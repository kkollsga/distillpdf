"""Link extraction on an owned fixture (links.pdf), which carries a real /Names /Dests
name tree: an external URI and an internal GoTo to a NAMED destination. Reproduces the
corpus link guarantees — URI + internal recall, destination resolution, HTML <a> wiring,
and the citation-anchor rule (internal links resolve to a #slug, never #page-N)."""
import re

from _fixtures import GT, doc, html

NAME = "links.pdf"
G = GT[NAME]


def test_uri_link_extracted():
    links = doc(NAME).extract_links()
    uris = [lk for lk in links if lk["kind"] == "uri"]
    assert any(lk["uri"] == G["uri"] for lk in uris), f"URI link not extracted: {G['uri']}"


def test_internal_link_resolved_to_named_destination():
    links = doc(NAME).extract_links()
    internal = [lk for lk in links if lk["kind"] == "internal"]
    assert internal, "no internal link extracted"
    lk = internal[0]
    assert lk["dest_name"] == G["dest_name"], f"dest_name {lk['dest_name']!r} != {G['dest_name']!r}"
    assert lk["dest_page"] == G["dest_page"], f"dest_page {lk['dest_page']} != {G['dest_page']}"


def test_external_link_wired_in_html():
    h = html(NAME)
    assert re.search(r'<a href="' + re.escape(G["uri"]) + r'"', h), \
        "external URI not wired as <a href> in HTML"


def test_remote_links_report_their_target_file():
    """/GoToR and /Launch links point at ANOTHER document, so nothing resolves in this
    one and the rows used to be dropped. They now survive as kind="remote" carrying the
    target file; the destination is not resolved against this PDF's pages."""
    links = doc(NAME).extract_links()
    remote = [lk for lk in links if lk["kind"] == "remote"]
    assert [lk["remote_file"] for lk in remote] == [G["remote_file"], G["launch_file"]], remote
    for lk in remote:
        assert lk["dest_page"] is None and lk["uri"] is None
    # Additive key: every link dict carries it, None for non-remote links.
    assert all("remote_file" in x for x in links)
    assert all(x["remote_file"] is None for x in links if x["kind"] != "remote")


def test_outline_keeps_indirect_utf16be_title():
    """The fixture's first bookmark has ``/Title 14 0 R`` (an indirect UTF-16BE string),
    the hyperref/pdfTeX shape. Matching only a direct string decoded it to "" and dropped
    the entry, so whole outlines came back empty."""
    entries = doc(NAME).outline()
    titles = [e[1] for e in entries]
    assert titles == [G["outline_indirect_title"], G["outline_direct_title"]], titles
    assert entries[0][2] == G["outline_indirect_page"]
    assert entries[1][2] == G["outline_direct_page"]


def test_internal_link_uses_named_anchor_not_page():
    """The citation-anchor guarantee: the internal link resolves to the destination's
    #slug, and a matching id anchor exists — never the coarse #page-N fallback."""
    h = html(NAME)
    slug = "#" + G["dest_name"]
    assert f'href="{slug}"' in h, f"internal link did not resolve to {slug}"
    assert re.search(r'id="' + re.escape(G["dest_name"]) + r'"', h), \
        "no id anchor placed at the named destination"
    assert not re.search(r'href="#page-\d+"', h), "internal link fell back to #page-N"
