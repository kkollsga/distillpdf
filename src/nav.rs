//! Table-of-contents / <nav> / <section> assembly. Reads the assembled HTML's headings
//! into a TOC, builds the <nav>, and wraps the body into per-section/per-page <section>s.
//! Pure string→string (+ Mode / outline entries); extracted from html.rs.

use crate::html::{esc, slug, Mode};
use crate::links;

/// The document outline parsed from the auto-TOC: `(level, title, page, anchor-id)`
/// per heading, in document order. Drives section navigation/extraction.
pub(crate) fn toc(html: &str) -> Vec<(u8, String, u32, String)> {
    let mut out = Vec::new();
    let nav = match (html.find("<nav>"), html.find("</nav>")) {
        (Some(a), Some(b)) if b > a => &html[a..b],
        _ => return out,
    };
    let field = |h: &str, key: &str| -> Option<String> {
        let s = h.find(key)? + key.len();
        let e = h[s..].find('"')?;
        Some(h[s..s + e].to_string())
    };
    for li in nav.split("<li ").skip(1) {
        let level = field(li, "data-level=\"").and_then(|s| s.parse().ok()).unwrap_or(0u8);
        let page = field(li, "data-page=\"").and_then(|s| s.parse().ok()).unwrap_or(0u32);
        let id = field(li, "href=\"#").unwrap_or_default();
        let after_li = li.split_once('>').map(|x| x.1).unwrap_or(""); // <a …>Label</a></li>
        let after_a = after_li.split_once('>').map(|x| x.1).unwrap_or(""); // Label</a>…
        let label = strip_inline(after_a.split("</a>").next().unwrap_or(""));
        if !id.is_empty() {
            out.push((level, label.trim().to_string(), page, id));
        }
    }
    out
}

/// The HTML of one section. `name` matches the `sec-…` slug, an id prefix, or a
/// case-insensitive title substring (so `section("abstract")` works); None if no match.
///
/// In **section mode** the id sits on a `<section>` wrapper, so the whole balanced
/// `<section>…</section>` element (including any nested subsections) is returned. In
/// **page mode** the id sits on the heading, so the heading plus content up to the next
/// same-or-higher heading is returned.
pub(crate) fn section(html: &str, name: &str) -> Option<String> {
    let entries = toc(html);
    let nl = name.to_lowercase();
    let want = {
        let s = format!("sec-{}", slug(&nl));
        s.trim_matches('-').to_string()
    };
    let idx = entries
        .iter()
        .position(|(_, t, _, i)| *i == want || i.starts_with(&want) || t.to_lowercase().contains(&nl))?;
    let id = &entries[idx].3;
    // Section mode: the id is on a <section> wrapper — return that balanced element.
    if let Some(open) = html.find(&format!("<section id=\"{id}\">")) {
        return Some(balanced_section(html, open));
    }
    // Page mode: the id is on the heading — slice to the next same-or-higher heading.
    let level = entries[idx].0;
    let start = {
        let p = html.find(&format!("id=\"{id}\""))?;
        html[..p].rfind("<h")?
    };
    let end = entries[idx + 1..]
        .iter()
        .find(|(l, _, _, _)| *l <= level)
        .and_then(|(_, _, _, nid)| html.find(&format!("id=\"{nid}\"")).map(|p| html[..p].rfind("<h").unwrap_or(p)))
        .unwrap_or_else(|| html.find("</body>").unwrap_or(html.len()));
    Some(html[start..end].trim().to_string())
}

/// From the byte offset of a `<section …>` open tag, return the full balanced
/// `<section>…</section>` element, accounting for nested sections.
pub(crate) fn balanced_section(html: &str, open: usize) -> String {
    let b = html.as_bytes();
    let mut depth = 0i32;
    let mut i = open;
    while i < b.len() {
        if b[i..].starts_with(b"<section") {
            depth += 1;
            i += "<section".len();
        } else if b[i..].starts_with(b"</section>") {
            depth -= 1;
            i += "</section>".len();
            if depth == 0 {
                return html[open..i].trim().to_string();
            }
        } else {
            i += 1;
        }
    }
    html[open..].trim().to_string()
}

/// Plain text of a fragment of inline HTML (drop tags, unescape the basic entities).
pub(crate) fn strip_inline(html: &str) -> String {
    let mut s = String::with_capacity(html.len());
    let mut intag = false;
    for c in html.chars() {
        match c {
            '<' => intag = true,
            '>' => intag = false,
            _ if !intag => s.push(c),
            _ => {}
        }
    }
    s.replace("&amp;", "&").replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&#39;", "'")
}

/// Give every heading a stable `sec-*` anchor id and prepend an auto table of
/// contents. Page is the primary organiser (headings stay inside their
/// `<section data-page>`), so each TOC entry carries its page. A FLAT `<ol>` (level
/// recorded as `data-level`, not nested sub-lists) keeps the outline queryable and
/// avoids spurious single-item sub-lists. Runs last — after `dedup_ids` — so the ids
/// it mints are deduped against the final id set.
pub(crate) fn build_toc(html: String, include_nav: bool) -> String {
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    {
        let b = html.as_bytes();
        let mut i = 0;
        while i + 4 < b.len() {
            if b[i..].starts_with(b"id=\"") {
                let s = i + 4;
                let mut e = s;
                while e < b.len() && b[e] != b'"' {
                    e += 1;
                }
                seen.insert(html[s..e].to_string());
                i = e;
            } else {
                i += 1;
            }
        }
    }
    let b = html.as_bytes();
    let mut out = String::with_capacity(html.len() + 512);
    let mut entries: Vec<(u8, String, u32, String)> = Vec::new(); // level, label, page, id
    let mut page = 0u32;
    let mut i = 0usize;
    let mut copied = 0usize;
    // Skip the leading front-matter <header>: the title <h1> inside it gets no sec- id.
    let header_end = html
        .find("<body>\n")
        .and_then(|p| html[p..].find("</header>").map(|q| p + q + "</header>".len()))
        .unwrap_or(0);
    while i < b.len() {
        if b[i..].starts_with(b"data-page=\"") {
            let s = i + 11;
            let mut e = s;
            while e < b.len() && b[e] != b'"' {
                e += 1;
            }
            page = html[s..e].parse().unwrap_or(page);
            i = e;
            continue;
        }
        // A heading open tag `<hL>` (L in 1..=6) — `b[i]=='<'` so `i` is a char boundary.
        if i >= header_end && b[i] == b'<' && i + 3 < b.len() && b[i + 1] == b'h' && (b'1'..=b'6').contains(&b[i + 2]) && b[i + 3] == b'>' {
            let level = b[i + 2] - b'0';
            let close = format!("</h{level}>");
            if let Some(rel) = html[i..].find(&close) {
                let inner = &html[i + 4..i + rel];
                let label = strip_inline(inner);
                let label = label.trim();
                if !label.is_empty() {
                    out.push_str(&html[copied..i]);
                    let base = {
                        let s = format!("sec-{}", slug(&label.to_lowercase()));
                        s.trim_matches('-').to_string()
                    };
                    let mut id = base.clone();
                    let mut k = 2;
                    while seen.contains(&id) {
                        id = format!("{base}-{k}");
                        k += 1;
                    }
                    seen.insert(id.clone());
                    entries.push((level, label.to_string(), page, id.clone()));
                    out.push_str(&format!("<h{level} id=\"{id}\">{inner}{close}"));
                    i += rel + close.len();
                    copied = i;
                    continue;
                }
            }
        }
        i += 1;
    }
    out.push_str(&html[copied..]);
    // The heading-id assignment above always runs (anchors/`section()` depend on it);
    // only the visible `<nav>` is gated. `entries` empty ⇒ nothing to outline.
    if entries.is_empty() || !include_nav {
        return out;
    }
    // Nested TOC: title (h1) and sections (h2) at the top level; subsections (h3)
    // indented under their section; deeper levels (h4+) omitted (too fine for an
    // outline). A `<ul>` (the outline order is positional, not enumerated). Each
    // <li> keeps data-level/data-page so the toc()/section() API still reads the
    // true heading level and page.
    insert_nav(out, &build_nav(&entries, true))
}

/// Build the `<nav><ul>…</ul></nav>` outline from heading entries. Title (level 1) and
/// sections (level 2) sit at the top level; subsections (level 3) nest under their
/// section; deeper levels are omitted. Each `<li>` carries `data-level` (and `data-page`
/// when `with_pages`) so the `toc()`/`section()` API still reads the true level/page.
pub(crate) fn build_nav(entries: &[(u8, String, u32, String)], with_pages: bool) -> String {
    let mut nav = String::from("<nav><ul>");
    let mut li_open = false; // a top-level <li> awaiting its </li>
    let mut sub_open = false; // a nested <ul> (h3 children) is open
    for (level, label, pg, id) in entries {
        if *level > 3 {
            continue;
        }
        // An empty id (an outline entry with no matching heading) renders as plain text
        // rather than a dead `#` link.
        let a = if id.is_empty() {
            esc(label)
        } else {
            format!("<a href=\"#{id}\">{}</a>", esc(label))
        };
        let li = if with_pages {
            format!("<li data-level=\"{level}\" data-page=\"{pg}\">{a}")
        } else {
            format!("<li data-level=\"{level}\">{a}")
        };
        if *level <= 2 {
            if sub_open {
                nav.push_str("</ul>");
                sub_open = false;
            }
            if li_open {
                nav.push_str("</li>");
            }
            nav.push_str(&li);
            li_open = true;
        } else if li_open {
            // h3 → nest under the current section
            if !sub_open {
                nav.push_str("<ul>");
                sub_open = true;
            }
            nav.push_str(&li);
            nav.push_str("</li>");
        } else {
            // a stray h3 before any section: keep it at top level
            nav.push_str(&li);
            nav.push_str("</li>");
        }
    }
    if sub_open {
        nav.push_str("</ul>");
    }
    if li_open {
        nav.push_str("</li>");
    }
    nav.push_str("</ul></nav>\n");
    nav
}

/// Whitespace/punctuation-insensitive title key (lowercased alphanumerics only). Lets a
/// PDF bookmark title match the detected heading even when the bookmark has cosmetic
/// defects (missing spaces, smart quotes) — the structure comes from the outline, the
/// clean title + working anchor from the matched heading.
pub(crate) fn title_key(s: &str) -> String {
    s.chars().filter(|c| c.is_alphanumeric()).flat_map(|c| c.to_lowercase()).collect()
}

/// Replace the generated `<nav>` with one built from the PDF's own outline (bookmarks).
/// The outline supplies the TRUE TOC structure; each entry is matched to a detected
/// heading (exact normalized title, else the heading title being a prefix of the
/// bookmark's — handles a bookmark like "…on and around…" vs a heading wrapped to "…on")
/// so it links to that heading's real `#sec-…` anchor. Unmatched entries appear as plain
/// text (no dead link). No `#page-N` is used, so this works the same in both modes and
/// doesn't re-introduce page identity. A no-op if there's no `<nav>` to replace.
pub(crate) fn nav_from_outline(html: String, entries: &[links::OutlineEntry], _mode: Mode) -> String {
    let detected = toc(&html);
    let keyed: Vec<(String, String)> = detected.iter().map(|(_l, t, _p, id)| (title_key(t), id.clone())).collect();
    let nav_entries: Vec<(u8, String, u32, String)> = entries
        .iter()
        .map(|e| {
            let level = (e.level + 1).min(6);
            let ek = title_key(&e.title);
            // exact match first, then a heading key that is a (≥8-char) prefix of the
            // bookmark key — a heading whose title wrapped and got truncated.
            let id = keyed
                .iter()
                .find(|(k, _)| *k == ek)
                .or_else(|| keyed.iter().find(|(k, _)| k.len() >= 8 && ek.starts_with(k.as_str())))
                .map(|(_, id)| id.clone())
                .unwrap_or_default();
            (level, e.title.clone(), e.page, id)
        })
        .collect();
    let nav = build_nav(&nav_entries, true); // outline entries always carry a page
    match (html.find("<nav>"), html.find("</nav>")) {
        (Some(a), Some(b)) if b > a => {
            let end = b + "</nav>".len();
            let mut out = String::with_capacity(html.len() + nav.len());
            out.push_str(&html[..a]);
            out.push_str(nav.trim_end_matches('\n'));
            out.push_str(&html[end..]);
            out
        }
        _ => html,
    }
}

/// Insert a `<nav>` block immediately after `<body>\n` (a no-op if there is no body tag).
pub(crate) fn insert_nav(html: String, nav: &str) -> String {
    match html.find("<body>\n") {
        Some(p) => {
            let at = p + "<body>\n".len();
            let mut res = String::with_capacity(html.len() + nav.len());
            res.push_str(&html[..at]);
            res.push_str(nav);
            res.push_str(&html[at..]);
            res
        }
        None => html,
    }
}

/// Section-mode assembly: regroup the flat content stream into nested `<section
/// id="sec-…">` wrappers — one per heading, with HTML-outline nesting (a heading at
/// level L closes every open section of level ≥ L, then opens a new one). The `sec-…`
/// id lives on the `<section>` wrapper; the inner `<hN>` is left bare. Optionally
/// prepends a pageless `<nav>` outline. Page identity is not used here at all.
pub(crate) fn build_sections(html: String, include_nav: bool) -> String {
    // Existing ids (figures/tables/named-destination anchors) — section ids dedupe
    // against them so a `sec-…` slug can never collide with one already in the document.
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    {
        let b = html.as_bytes();
        let mut i = 0;
        while i + 4 < b.len() {
            if b[i..].starts_with(b"id=\"") {
                let s = i + 4;
                let mut e = s;
                while e < b.len() && b[e] != b'"' {
                    e += 1;
                }
                seen.insert(html[s..e].to_string());
                i = e;
            } else {
                i += 1;
            }
        }
    }
    // Walk the body, emitting `<section>` open/close around heading-delimited runs.
    let b = html.as_bytes();
    let mut out = String::with_capacity(html.len() + 512);
    let mut entries: Vec<(u8, String, u32, String)> = Vec::new(); // level, label, 0, id
    let mut open: Vec<u8> = Vec::new(); // stack of open section levels
    let mut i = 0usize;
    let mut copied = 0usize;
    let body_start = html.find("<body>\n").map(|p| p + "<body>\n".len()).unwrap_or(0);
    // Skip the leading front-matter <header> so the title <h1> inside it is neither
    // wrapped in a <section> nor minted a sec- id.
    let header_end = html[body_start..]
        .find("</header>")
        .map(|p| body_start + p + "</header>".len())
        .unwrap_or(body_start);
    while i < b.len() {
        // A heading open tag `<hL>` (L in 1..=6). Only headings within <body> matter.
        if i >= header_end
            && b[i] == b'<'
            && i + 3 < b.len()
            && b[i + 1] == b'h'
            && (b'1'..=b'6').contains(&b[i + 2])
            && b[i + 3] == b'>'
        {
            let level = b[i + 2] - b'0';
            let close = format!("</h{level}>");
            if let Some(rel) = html[i..].find(&close) {
                let inner = &html[i + 4..i + rel];
                let label = strip_inline(inner);
                let label = label.trim();
                if !label.is_empty() {
                    // Flush content up to this heading, then close deeper/sibling sections.
                    out.push_str(&html[copied..i]);
                    while open.last().is_some_and(|&l| l >= level) {
                        out.push_str("</section>");
                        open.pop();
                    }
                    let base = {
                        let s = format!("sec-{}", slug(&label.to_lowercase()));
                        s.trim_matches('-').to_string()
                    };
                    let mut id = base.clone();
                    let mut k = 2;
                    while seen.contains(&id) {
                        id = format!("{base}-{k}");
                        k += 1;
                    }
                    seen.insert(id.clone());
                    entries.push((level, label.to_string(), 0, id.clone()));
                    out.push_str(&format!("<section id=\"{id}\">"));
                    open.push(level);
                    copied = i; // the bare heading itself is copied with the next run
                    i += rel + close.len();
                    continue;
                }
            }
        }
        // Close all open sections right before </body> so nothing leaks outside the body.
        if b[i..].starts_with(b"</body>") {
            out.push_str(&html[copied..i]);
            while open.pop().is_some() {
                out.push_str("</section>");
            }
            copied = i;
            i += "</body>".len();
            continue;
        }
        i += 1;
    }
    out.push_str(&html[copied..]);
    if entries.is_empty() || !include_nav {
        return out;
    }
    insert_nav(out, &build_nav(&entries, false))
}
