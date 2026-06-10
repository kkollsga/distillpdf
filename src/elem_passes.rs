//! Cross-page transforms on the typed element IR (the single-stream replacement for the
//! legacy HTML-string post-processing passes in [`crate::postprocess`]).
//!
//! The document pipeline is: per-page walks produce `Vec<Vec<PageElement>>` → these cross-page
//! element transforms rewrite that IR in place → each page is emitted to HTML and merged. Doing
//! the transforms on the elements (not on the merged string) is what lets the model's block
//! projection see the SAME post-transform elements the HTML is emitted from — no HTML
//! round-trip, no lost structure.
//!
//! Each pass is ported from its string twin in [`crate::postprocess`] and is gated on
//! byte-identity of the emitted HTML (page AND section mode) across the whole corpus.
//!
//! **Page-locality (why per-page is faithful).** In page mode the merged body separates pages
//! with `</section>\n<section data-page=…>\n`, which is NOT whitespace, so the legacy string
//! passes never merged a construct ACROSS a page boundary. In section mode the per-page bodies
//! ARE concatenated bare, but the model stores each page's PAGE-mode body and the section-mode
//! re-render reassembles those — so the round-trip already requires that no cross-page merge
//! changes the output. The adjacency passes therefore run PER PAGE here, which reproduces the
//! legacy output byte-for-byte in both modes (verified across the corpus).

use crate::html::{bbox_union, ElKind, PageElement};
use crate::postprocess;

/// Run every cross-page transform that has been ported to the element IR, in place. `pages` is
/// `(page_number, elements, image_uris)` per page in reading order. `_mode` is threaded for the
/// passes whose effect is mode-dependent (none currently — the adjacency passes are page-local
/// in both modes, see the module docs).
pub(crate) fn run_cross_page_passes(pages: &mut [(u32, Vec<PageElement>, Vec<String>)], mode: crate::html::Mode) {
    // Passes run in the legacy string-pipeline order — demote_running_headings, then
    // merge_adjacent_links, then …, then merge_math_fragments, … — because an earlier pass's
    // output can be a later pass's input (e.g. a math merge that joins two paragraphs can create
    // an adjacent-anchor pattern; the legacy order ran links FIRST so that join is NOT
    // link-merged). Keeping the IR order identical preserves that interaction byte-for-byte.
    // demote_running_headings runs FIRST (legacy order), document-wide: it can turn a recurring
    // heading into a `Para`, which the later merge passes then see (matching the legacy pipeline,
    // where demote preceded the merges).
    demote_running_headings(pages);
    for (_pno, els, _uris) in pages.iter_mut() {
        merge_adjacent_links(els);
        merge_fragmented_lists(els);
        merge_math_fragments(els);
        merge_adjacent_figures(els);
    }
    // In SECTION mode the per-page bodies are concatenated bare (no `<section>` wrapper, no
    // inter-page separator), so a math-fragment paragraph at the end of one page and one at the
    // start of the next are ADJACENT in the merged body — the legacy string pass merged them
    // across the page boundary. Page mode separates pages with `</section>…<section>`, which is
    // not whitespace, so no cross-page merge happens there. Reproduce the section-mode cross-page
    // merge on the element stream (the merged `Para` is folded into the earlier page; section
    // mode emits no per-page framing, so which page owns it is invisible in the output, and the
    // model — built in PAGE mode — never sees this transform).
    if mode == crate::html::Mode::Section {
        merge_fragmented_lists_across_pages(pages);
        merge_math_fragments_across_pages(pages);
    }
}

/// Fold a fragmented list that straddles a page boundary into one `List` on the earlier page
/// (section mode only — same rationale as [`merge_math_fragments_across_pages`]). When page N
/// ends with a `List` and the following pages begin with `[≤3 continuation Para][same-type
/// List]` (the continuation paras may themselves straddle the boundary), append the legacy
/// ` ` + joined-conts to page N's last list item and absorb the later lists' items, dropping the
/// consumed elements from the later pages.
fn merge_fragmented_lists_across_pages(pages: &mut [(u32, Vec<PageElement>, Vec<String>)]) {
    let cont_ok = |text: &str| text.len() < 400 && !postprocess::introduces_list(text);
    let mut p = 0;
    while p < pages.len() {
        // Page p must END with a List for a straddle to start.
        let Some(PageElement { kind: ElKind::List { ordered, .. }, .. }) = pages[p].1.last() else {
            p += 1;
            continue;
        };
        let ordered = *ordered;
        // Repeatedly absorb [conts][same-type List] from the pages after p (each iteration of
        // this loop folds one reopening list into page p's trailing list; it exits when the
        // next thing after the continuation paras is not a same-type list).
        loop {
            // Gather, across pages, the leading run of qualifying continuation paras starting at
            // (p+1, 0), then require a same-type List immediately after.
            let mut conts: Vec<String> = Vec::new();
            // cursor over (page, idx) starting just past page p's content.
            let mut cp = p + 1;
            let mut ci = 0usize;
            let mut ok = true;
            loop {
                // advance cursor past empty pages
                while cp < pages.len() && ci >= pages[cp].1.len() {
                    cp += 1;
                    ci = 0;
                }
                if cp >= pages.len() {
                    ok = false;
                    break;
                }
                match &pages[cp].1[ci] {
                    PageElement { kind: ElKind::List { ordered: o2, .. }, .. } if *o2 == ordered => break, // reopening list
                    PageElement { kind: ElKind::Para { text }, .. } if conts.len() < 3 && cont_ok(text) => {
                        conts.push(text.clone());
                        ci += 1;
                    }
                    _ => {
                        ok = false;
                        break;
                    }
                }
            }
            if !ok {
                break;
            }
            // cursor (cp, ci) points at the reopening same-type List. Append conts to page p's
            // last item, absorb the reopening list's items, and remove the consumed elements
            // (the conts paras + the reopening list) from their pages.
            let (items2, box2) = match &pages[cp].1[ci] {
                PageElement { kind: ElKind::List { items, .. }, bbox } => (items.clone(), *bbox),
                _ => unreachable!(),
            };
            if let Some(PageElement { kind: ElKind::List { items, .. }, bbox }) = pages[p].1.last_mut() {
                if let Some(last) = items.last_mut() {
                    last.push(' ');
                    last.push_str(&conts.join(" "));
                }
                items.extend(items2);
                *bbox = bbox_union(*bbox, box2); // the merged list spans both pages' regions
            }
            // Remove consumed: every element from (p+1,0) up to and INCLUDING (cp, ci).
            for page in pages.iter_mut().take(cp).skip(p + 1) {
                page.1.clear();
            }
            pages[cp].1.drain(..=ci);
        }
        p += 1;
    }
}

/// Fold a math-fragment paragraph run that straddles a page boundary into a single `Para` on
/// the earlier page (section mode only — see [`run_cross_page_passes`]). Walks page boundaries
/// left to right; at each boundary, if page N ends with a math-fragment `Para` and page N+1
/// begins with one, the run continues across — gather the whole straddling run (it may cross
/// several pages), merge it into one `Para` placed at the run's start page, and drop the rest.
fn merge_math_fragments_across_pages(pages: &mut [(u32, Vec<PageElement>, Vec<String>)]) {
    let is_frag = |e: &PageElement| matches!(&e.kind, ElKind::Para { text } if postprocess::is_math_fragment(text));
    let mut p = 0;
    while p < pages.len() {
        // A straddling run requires page p to END with a fragment paragraph.
        if pages[p].1.last().is_none_or(|e| !is_frag(e)) {
            p += 1;
            continue;
        }
        // Count the trailing fragment run on page p (after per-page merge this is exactly one
        // Para when the run was wholly on-page; a single trailing fragment when it began mid-page).
        let start_run = pages[p].1.iter().rposition(|e| !is_frag(e)).map(|i| i + 1).unwrap_or(0);
        // Gather following pages whose LEADING elements continue the fragment run. `merged_box`
        // unions every fragment's region so the merged `Para` carries the run's full span.
        let mut merged_box = None;
        let mut collected: Vec<String> = pages[p].1[start_run..]
            .iter()
            .map(|e| {
                merged_box = bbox_union(merged_box, e.bbox);
                match &e.kind {
                    ElKind::Para { text } => text.clone(),
                    _ => unreachable!(),
                }
            })
            .collect();
        let mut last_page = p;
        let mut q = p + 1;
        while q < pages.len() {
            let lead = pages[q].1.iter().position(|e| !is_frag(e)).unwrap_or(pages[q].1.len());
            if lead == 0 {
                break; // page q does not begin with a fragment — run ends at page p..=last_page
            }
            for e in &pages[q].1[..lead] {
                if let ElKind::Para { text } = &e.kind {
                    collected.push(text.clone());
                    merged_box = bbox_union(merged_box, e.bbox);
                }
            }
            // If the whole page was fragments, the run may continue onto q+1.
            let whole = lead == pages[q].1.len();
            last_page = q;
            if whole {
                q += 1;
            } else {
                break;
            }
        }
        if last_page > p && collected.len() >= 2 {
            // Merge: replace page p's trailing run with one Para, strip the consumed leading
            // fragments from the intervening/last pages.
            let merged = PageElement::at(ElKind::Para { text: collected.join(" ") }, merged_box);
            pages[p].1.truncate(start_run);
            pages[p].1.push(merged);
            for (qi, page) in pages.iter_mut().enumerate().take(last_page + 1).skip(p + 1) {
                let lead = page.1.iter().position(|e| !is_frag(e)).unwrap_or(page.1.len());
                let drop = if qi == last_page { lead } else { page.1.len() };
                page.1.drain(..drop);
            }
            p = last_page + 1;
        } else {
            p += 1;
        }
    }
}

/// Collapse adjacent same-href anchors into one (the element twin of
/// [`postprocess::merge_adjacent_links`]). A link split across styled runs or a line-break
/// emits `…</a><a href="H">…` WITHIN one element's inner HTML — anchors never straddle two
/// elements (the `</p><p>` / `</li>` boundary between elements is not whitespace, so the legacy
/// pass never merged across it). So this applies the same string transform to every inner-HTML-
/// bearing field of each element, which is byte-identical to the legacy whole-document pass.
fn merge_adjacent_links(els: &mut [PageElement]) {
    for e in els.iter_mut() {
        map_inline_html(e, postprocess::merge_adjacent_links);
    }
}

/// Apply `f` to each inner-/fragment-HTML string an element CONTRIBUTES TO ITS EMITTED HTML
/// (the fields [`PageElement::html`] reads), so a string transform over those fields is
/// byte-identical to running the legacy transform on the emitted document. Escaped-only fields
/// (list items, table cells, code) carry no inline markup but are still mapped — `f` is a no-op
/// on text with no anchors, keeping the surface uniform.
fn map_inline_html(e: &mut PageElement, f: impl Fn(&str) -> String) {
    use ElKind::*;
    match &mut e.kind {
        DestAnchors(s) | Header(s) | Code { text: s } => *s = f(s),
        Heading { text, .. } | Para { text } => *text = f(text),
        List { items, .. } => {
            for it in items.iter_mut() {
                *it = f(it);
            }
        }
        Footnotes { notes } => {
            for n in notes.iter_mut() {
                *n = f(n);
            }
        }
        Table { header, grid, caption } => {
            for row in header.iter_mut() {
                for (t, _) in row.iter_mut() {
                    *t = f(t);
                }
            }
            for row in grid.iter_mut() {
                for c in row.iter_mut() {
                    *c = f(c);
                }
            }
            if let Some((_, cap, _)) = caption {
                *cap = f(cap);
            }
        }
        Figure { html, .. } | Caption { html, .. } => *html = f(html),
    }
}

/// Demote running page-headers across the document (the element twin of the legacy
/// `demote_running_headings`). Counts the normalized [`postprocess::demote_key`] of every
/// [`PageElement::Heading`] across ALL pages; any heading whose key (length ≥ 4) recurs ≥ 3
/// times is a running head and is rewritten `Heading → Para` — EXCEPT the first level-1 (`<h1>`)
/// occurrence of each such key, which is the document's real title and is kept. Headings at this
/// stage carry no id (ids are minted later in `assemble`), so the demoted `Para` is `text` only,
/// exactly as the legacy `<p>{inner}</p>`.
fn demote_running_headings(pages: &mut [(u32, Vec<PageElement>, Vec<String>)]) {
    use std::collections::{HashMap, HashSet};
    let mut counts: HashMap<String, usize> = HashMap::new();
    for (_p, els, _u) in pages.iter() {
        for e in els {
            if let ElKind::Heading { text, .. } = &e.kind {
                let k = postprocess::demote_key(text);
                if k.len() >= 4 {
                    *counts.entry(k).or_insert(0) += 1;
                }
            }
        }
    }
    let mut kept_h1: HashSet<String> = HashSet::new();
    for (_p, els, _u) in pages.iter_mut() {
        for e in els.iter_mut() {
            let ElKind::Heading { level, text, .. } = &mut e.kind else { continue };
            let k = postprocess::demote_key(text);
            if counts.get(&k).copied().unwrap_or(0) < 3 {
                continue;
            }
            // Keep the first <h1> (the real title that legitimately recurs in the running head).
            if *level == 1 && kept_h1.insert(k) {
                continue;
            }
            // Demote to a `Para`, preserving the heading's bbox (the demote keeps its region).
            e.kind = ElKind::Para { text: std::mem::take(text) };
        }
    }
}

/// Merge a graphic-only `<figure>` adjacent to a caption-only `<figure>` (the element twin of
/// the legacy `merge_adjacent_figures`). Both [`PageElement::Figure`] and a figure-shell
/// [`PageElement::Caption`] (`is_figure = true`) emit a `<figure>…</figure>`; the legacy pass
/// keyed on two such fragments being adjacent (only whitespace between — and IR elements emit
/// with NO whitespace between, so element-adjacency IS the legacy adjacency). When a pair merges
/// (one graphic-only, one caption-only), the result is one `Figure` element carrying the merged
/// fragment plus the merged structured projection (graphic asset/svg from the graphic side, id +
/// caption from the caption side).
fn merge_adjacent_figures(els: &mut Vec<PageElement>) {
    let is_fig_shell = |e: &PageElement| e.html().starts_with("<figure");
    let src = std::mem::take(els);
    let mut out: Vec<PageElement> = Vec::with_capacity(src.len());
    let mut iter = src.into_iter().peekable();
    while let Some(e) = iter.next() {
        if !is_fig_shell(&e) {
            out.push(e);
            continue;
        }
        // `e` is a figure shell; if the next element is also a figure shell and the two pair,
        // merge them into one Figure.
        let pair_next = iter.peek().is_some_and(is_fig_shell);
        if pair_next {
            let f1_html = e.html();
            // SAFETY: peek confirmed a next figure shell.
            let next = iter.peek().expect("peeked figure shell");
            let f2_html = next.html();
            if let Some(merged_html) = postprocess::pair_adjacent_figures(&f1_html, &f2_html) {
                let second = iter.next().expect("peeked figure shell");
                out.push(merge_figure_pair(merged_html, e, second));
                continue;
            }
        }
        out.push(e);
    }
    *els = out;
}

/// Build the merged [`PageElement::Figure`] from a paired graphic/caption shell. `html` is the
/// already-merged fragment; the structured fields take the graphic (image/svg) from whichever
/// side carried it and the id/caption from whichever side carried the caption — so the block
/// projection (Stage B) sees one figure with both its graphic asset and its caption.
fn merge_figure_pair(html: String, a: PageElement, b: PageElement) -> PageElement {
    // The merged figure spans both shells' regions.
    let bbox = bbox_union(a.bbox, b.bbox);
    // Pull (id, caption, image, svg) from each side; the graphic side has image/svg, the caption
    // side has the caption (and usually the id, which the merged html already keeps).
    let dissolve = |e: PageElement| -> (String, Option<String>, Option<String>, Option<String>) {
        match e.kind {
            ElKind::Figure { id, caption, image, svg, .. } => (id, caption, image, svg),
            ElKind::Caption { id, text, is_figure, .. } => {
                // A figure-shell caption: it carries the caption text + the fig-N id (the
                // Caption.id is the full "fig-N"; strip the prefix to match Figure.id's "N").
                let num = if is_figure { id.strip_prefix("fig-").unwrap_or(&id).to_string() } else { id };
                (num, Some(text), None, None)
            }
            _ => (String::new(), None, None, None),
        }
    };
    let (ida, capa, imga, svga) = dissolve(a);
    let (idb, capb, imgb, svgb) = dissolve(b);
    PageElement::at(ElKind::Figure {
        html,
        // The id the merged html keeps is the caption figure's; mirror that in the projection:
        // prefer the side that carries a caption (it owns the id), else either.
        id: if capa.is_some() { ida } else if capb.is_some() { idb } else if !ida.is_empty() { ida } else { idb },
        caption: capa.or(capb),
        image: imga.or(imgb),
        svg: svga.or(svgb),
    }, bbox)
}

/// Rejoin a list fragmented into single-item lists (the element twin of
/// [`postprocess::merge_fragmented_lists`]): a `List` followed by up to 3 short, non-introducing
/// `Para` continuation lines and then a SAME-`ordered` `List` is one list — the intervening
/// paragraphs are the wrapped continuation of the first list's last item. The legacy string pass
/// replaced `…A</li></ul> <p>c</p> <ul><li>B…` with `…A c</li><li>B…`, i.e. it appended a single
/// space plus the joined continuations to the first list's LAST item, then concatenated the
/// second list's items. It ran the `ul` then the `ol` shape and chained greedily (a run of
/// fragmented lists collapses); this mirrors both.
fn merge_fragmented_lists(els: &mut Vec<PageElement>) {
    // A `<p>` qualifies as a wrapped-list continuation when it is short (< 400 bytes of inner,
    // matching the legacy `rel < 400`) and does NOT introduce the next list (does not end ':').
    let cont_ok = |text: &str| text.len() < 400 && !postprocess::introduces_list(text);

    let src = std::mem::take(els);
    let mut out: Vec<PageElement> = Vec::with_capacity(src.len());
    let mut i = 0;
    while i < src.len() {
        let ElKind::List { ordered, items } = &src[i].kind else {
            out.push(src[i].clone());
            i += 1;
            continue;
        };
        let ordered = *ordered;
        let mut items = items.clone();
        let mut bbox = src[i].bbox;
        i += 1;
        // Greedily absorb [≤3 qualifying continuation Paras][same-type List] runs.
        loop {
            let mut j = i;
            let mut conts: Vec<String> = Vec::new();
            let mut conts_box = None;
            while j < src.len() && conts.len() < 3 {
                match &src[j].kind {
                    ElKind::Para { text } if cont_ok(text) => {
                        conts.push(text.clone());
                        conts_box = bbox_union(conts_box, src[j].bbox);
                        j += 1;
                    }
                    _ => break,
                }
            }
            match src.get(j) {
                Some(PageElement { kind: ElKind::List { ordered: o2, items: items2 }, bbox: lbox }) if *o2 == ordered => {
                    if let Some(last) = items.last_mut() {
                        last.push(' ');
                        last.push_str(&conts.join(" "));
                    }
                    items.extend(items2.iter().cloned());
                    bbox = bbox_union(bbox_union(bbox, conts_box), *lbox);
                    i = j + 1;
                }
                _ => break,
            }
        }
        out.push(PageElement::at(ElKind::List { ordered, items }, bbox));
    }
    *els = out;
}

/// Merge a maximal run of ≥2 consecutive math-fragment inner-HTML strings into one (joined by a
/// single space); a lone fragment or a non-fragment passes through unchanged. This is the core
/// of [`merge_math_fragments`], shared between the top-level `<p>` stream and a footnote
/// `<aside>`'s notes — the legacy string pass merged math-fragment `<p>`s wherever they sat
/// (including inside an `<aside>`, bounded by the non-`<p>` `</aside>`).
fn merge_fragment_run(items: Vec<String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(items.len());
    let mut iter = items.into_iter().peekable();
    while let Some(first) = iter.next() {
        if !postprocess::is_math_fragment(&first) {
            out.push(first);
            continue;
        }
        let mut run = vec![first];
        while iter.peek().is_some_and(|n| postprocess::is_math_fragment(n)) {
            run.push(iter.next().unwrap());
        }
        if run.len() >= 2 {
            out.push(run.join(" "));
        } else {
            out.push(run.into_iter().next().unwrap_or_default());
        }
    }
    out
}

/// Run the one cross-page pass that DELIBERATELY stays a string pass: [`postprocess::dedup_ids`].
///
/// **Why dedup_ids is not an element pass.** It is a whole-document HTML-hygiene pass over the
/// LITERAL `id="…"` namespace of the emitted body — and that namespace includes ids the element
/// IR does not model: SVG-internal `id`s (e.g. `<clipPath id="clip0">`, which collide across
/// figures/pages and must be renumbered) live inside the opaque `Figure.svg`/`Figure.html`
/// fragment, not as structured IR fields. It also must run HERE — on the merged body, before
/// `assemble`'s `build_toc` mints the `sec-…` heading ids — because `build_toc` seeds its
/// uniqueness set from exactly these already-deduped ids (see `nav::build_toc`). Reproducing
/// that contract in the IR would mean parsing SVG content back out of the fragment — an HTML
/// round-trip of the kind this refactor exists to remove. So it stays a literal-string pass at
/// its original boundary; it is byte-identical and does not feed the block projection (block ids
/// are ordinal `b0001…`, and figure/table element ids are read from the IR's `Figure.id` /
/// `Caption.id` BEFORE this runs).
pub(crate) fn run_residual_string_passes(body: String) -> String {
    postprocess::dedup_ids(&body)
}

// ---- ported passes ---------------------------------------------------------

/// Rejoin a display equation shattered into per-token paragraphs: merge a RUN of ≥2 consecutive
/// math-fragment [`PageElement::Para`]s into one `Para` (inner joined by a single space), so the
/// equation is a single block. A lone fragment is left alone. The element twin of
/// [`postprocess::merge_math_fragments`]: the legacy pass keyed on consecutive `<p>…</p>` with
/// only whitespace between, which is exactly a run of adjacent `Para` elements here.
fn merge_math_fragments(els: &mut Vec<PageElement>) {
    let src = std::mem::take(els);
    let mut out: Vec<PageElement> = Vec::with_capacity(src.len());
    let mut iter = src.into_iter().peekable();
    while let Some(e) = iter.next() {
        // A footnote `<aside>`'s notes are each emitted as `<p>`; the legacy pass merged a
        // math-fragment run INSIDE the aside too (bounded by `</aside>`). Mirror that.
        if let ElKind::Footnotes { notes } = e.kind {
            out.push(PageElement::at(ElKind::Footnotes { notes: merge_fragment_run(notes) }, e.bbox));
            continue;
        }
        let frag_inner = match &e.kind {
            ElKind::Para { text } if postprocess::is_math_fragment(text) => Some(text.clone()),
            _ => None,
        };
        let Some(first) = frag_inner else {
            out.push(e);
            continue;
        };
        // `e` is a math-fragment paragraph; gather the maximal run of further fragment paras
        // (unioning their boxes onto the merged `Para`).
        let mut run = vec![first];
        let mut box_acc = e.bbox;
        while matches!(iter.peek(), Some(PageElement { kind: ElKind::Para { text }, .. }) if postprocess::is_math_fragment(text)) {
            if let Some(PageElement { kind: ElKind::Para { text }, bbox }) = iter.next() {
                run.push(text);
                box_acc = bbox_union(box_acc, bbox);
            }
        }
        if run.len() >= 2 {
            out.push(PageElement::at(ElKind::Para { text: run.join(" ") }, box_acc));
        } else {
            out.push(PageElement::at(ElKind::Para { text: run.into_iter().next().unwrap_or_default() }, box_acc));
        }
    }
    *els = out;
}
