//! HTML string post-processing passes (behaviour-preserving): run on the assembled
//! HTML to merge split figures, rejoin math/list fragments, dedup ids, merge adjacent
//! links, and demote running-header lines. Pure string→string; extracted from html.rs.

/// Merge a graphic-only `<figure>` immediately adjacent to a caption-only `<figure>`
/// (in either order) into one — the literal "graphic and caption split into two
/// figures" defect, resolved regardless of why caption-anchoring missed the pair.
pub(crate) fn merge_adjacent_figures(html: &str) -> String {
    fn take_figure(s: &str) -> Option<(&str, usize)> {
        if !s.starts_with("<figure") {
            return None;
        }
        s.find("</figure>").map(|e| (&s[..e + 9], e + 9))
    }
    // The opening tag attributes (after "<figure", before '>') and inner content.
    fn parts(fig: &str) -> (&str, &str) {
        let open_end = fig.find('>').map(|i| i + 1).unwrap_or(0);
        let attrs = fig["<figure".len()..open_end.saturating_sub(1)].trim();
        let inner = &fig[open_end..fig.len() - 9];
        (attrs, inner)
    }
    let is_graphic = |f: &str| f.contains("<img") || f.contains("<svg");
    let mut out = String::with_capacity(html.len());
    let mut rest = html;
    while !rest.is_empty() {
        if let Some((f1, l1)) = take_figure(rest) {
            let tail = rest[l1..].trim_start();
            if let Some((f2, l2)) = take_figure(tail) {
                let (g1, c1) = (is_graphic(f1), f1.contains("<figcaption"));
                let (g2, c2) = (is_graphic(f2), f2.contains("<figcaption"));
                // one is graphic-only, the other caption-only → merge graphic + caption
                let pair = (g1 && !c1 && c2 && !g2) || (c1 && !g1 && g2 && !c2);
                if pair {
                    let (graphic, caption) = if g1 { (f1, f2) } else { (f2, f1) };
                    let (gattr, ginner) = parts(graphic);
                    let (cattr, cinner) = parts(caption);
                    let attr = if !cattr.is_empty() { cattr } else { gattr }; // keep the id (on the caption figure)
                    out.push_str("<figure");
                    if !attr.is_empty() {
                        out.push(' ');
                        out.push_str(attr);
                    }
                    out.push('>');
                    out.push_str(ginner);
                    out.push_str(cinner);
                    out.push_str("</figure>");
                    let consumed = (rest.len() - tail.len()) + l2;
                    rest = &rest[consumed..];
                    continue;
                }
            }
            out.push_str(f1);
            rest = &rest[l1..];
            continue;
        }
        let c = rest.chars().next().unwrap();
        out.push(c);
        rest = &rest[c.len_utf8()..];
    }
    out
}

pub(crate) fn strip_tags_inline(s: &str) -> String {
    let mut o = String::new();
    let mut in_tag = false;
    for c in s.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => o.push(c),
            _ => {}
        }
    }
    o
}

/// A `<p>` whose content is a fragment of a DISPLAY EQUATION rather than prose: a
/// lone operator/number/punctuation, an equation number "(N)", a single symbol, or
/// a short run with math operators/Greek and ≤1 real word.
pub(crate) fn is_math_fragment(inner: &str) -> bool {
    let t = strip_tags_inline(inner);
    let t = t.trim();
    let n = t.chars().count();
    if t.is_empty() || n > 60 {
        return false;
    }
    let alpha_words = t.split(|c: char| !c.is_alphabetic()).filter(|w| w.chars().count() >= 2).count();
    if alpha_words > 1 {
        return false; // real prose
    }
    const MATH: &str = "=+-−–×÷·≤≥≠≈∝∫∑∏√∈∉⊂⊆∂∇∞()[]{}|/^_<>";
    const GREEK: &str = "αβγδεζηθικλμνξπρστυϕφχψωΓΔΘΛΞΠΣΦΨΩ";
    t.chars().any(|c| MATH.contains(c) || GREEK.contains(c)) || n <= 2 || t.chars().all(|c| !c.is_alphabetic())
}

/// Rejoin a display equation shattered into per-token `<p>`s: merge a RUN of ≥2
/// consecutive math-fragment `<p>`s (only whitespace between) into one `<p>` so the
/// equation is a single block and stray operators/numbers/commas stop being orphan
/// paragraphs. A lone fragment is left alone.
pub(crate) fn merge_math_fragments(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut rest = html;
    while !rest.is_empty() {
        if rest.starts_with("<p>") {
            let mut frags: Vec<&str> = Vec::new();
            let mut cursor = rest;
            loop {
                let c2 = cursor.trim_start();
                if let Some(body) = c2.strip_prefix("<p>") {
                    if let Some(rel) = body.find("</p>") {
                        let inner = &body[..rel];
                        if is_math_fragment(inner) {
                            frags.push(inner);
                            let adv = (cursor.len() - c2.len()) + 3 + rel + 4;
                            cursor = &cursor[adv..];
                            continue;
                        }
                    }
                }
                break;
            }
            if frags.len() >= 2 {
                out.push_str("<p>");
                out.push_str(&frags.join(" "));
                out.push_str("</p>");
                rest = cursor;
                continue;
            }
        }
        let ch = rest.chars().next().unwrap();
        out.push(ch);
        rest = &rest[ch.len_utf8()..];
    }
    out
}

/// Whether a `<p>` between two same-type lists INTRODUCES the following list (its text
/// ends with ':', e.g. "The second procedure has these steps:"). Such a line is a real
/// separator — the next list is its own list, not a fragment of the previous one — so
/// the two must not be fused. A genuine wrapped continuation of the last item never ends
/// with a colon.
pub(crate) fn introduces_list(inner: &str) -> bool {
    let mut t = String::new();
    let mut depth = 0i32;
    for ch in inner.chars() {
        match ch {
            '<' => depth += 1,
            '>' => depth = (depth - 1).max(0),
            _ if depth == 0 => t.push(ch),
            _ => {}
        }
    }
    t.trim_end().ends_with(':')
}

/// Rejoin a list fragmented into single-item lists: `…A</li></ul> <p>cont</p>… <ul><li>B…`
/// becomes `…A cont…</li><li>B…`. The intervening `<p>`s are the wrapped continuation
/// of item A that the line loop couldn't attach (flush-left wrap, column break). Only
/// fires for same-type adjacent lists with a few short continuation paragraphs between,
/// and never across a `<p>` that introduces the next list (ends with ':') — so two real
/// lists, the second introduced by a lead-in line, are left separate.
pub(crate) fn merge_fragmented_lists(html: &str) -> String {
    let mut s = html.to_string();
    for tag in ["ul", "ol"] {
        let close = format!("</li></{tag}>");
        let open_li = format!("<{tag}><li>");
        let mut out = String::with_capacity(s.len());
        let mut i = 0;
        while i < s.len() {
            if s[i..].starts_with(&close) {
                // After the close: optional whitespace + up to 3 short <p>…</p> blocks
                // (the wrapped continuation), then the SAME-type list reopening.
                let mut k = i + close.len();
                let mut conts: Vec<&str> = Vec::new();
                let mut ok = true;
                loop {
                    while k < s.len() && s.as_bytes()[k].is_ascii_whitespace() {
                        k += 1;
                    }
                    if s[k..].starts_with(&open_li) {
                        break;
                    }
                    if conts.len() < 3 {
                        if let Some(body) = s[k..].strip_prefix("<p>") {
                            if let Some(rel) = body.find("</p>") {
                                // A lead-in line ("… steps:") separates two real lists;
                                // never fold across it.
                                if rel < 400 && !introduces_list(&body[..rel]) {
                                    conts.push(&body[..rel]);
                                    k += 3 + rel + 4;
                                    continue;
                                }
                            }
                        }
                    }
                    ok = false;
                    break;
                }
                if ok && s[k..].starts_with(&open_li) {
                    out.push(' ');
                    out.push_str(&conts.join(" "));
                    out.push_str("</li><li>");
                    i = k + open_li.len();
                    continue;
                }
            }
            let ch = s[i..].chars().next().unwrap();
            out.push(ch);
            i += ch.len_utf8();
        }
        s = out;
    }
    s
}

/// Guarantee unique `id=` attributes: the first use of an id keeps it, later uses
/// are suffixed ("tab-3" → "tab-3-2"). The inline-cross-reference guard removes the
/// common cause (phantom figures), but genuine same-number elements (sub-tables on
/// different pages) can still collide — an HTML document must not repeat an id.
pub(crate) fn dedup_ids(html: &str) -> String {
    let mut seen: std::collections::HashMap<String, u32> = std::collections::HashMap::new();
    let mut out = String::with_capacity(html.len());
    let mut rest = html;
    while let Some(pos) = rest.find("id=\"") {
        out.push_str(&rest[..pos + 4]);
        rest = &rest[pos + 4..];
        let end = match rest.find('"') {
            Some(e) => e,
            None => break,
        };
        let id = &rest[..end];
        let n = seen.entry(id.to_string()).or_insert(0);
        *n += 1;
        if *n == 1 {
            out.push_str(id);
        } else {
            out.push_str(&format!("{id}-{n}"));
        }
        out.push('"');
        rest = &rest[end + 1..];
    }
    out.push_str(rest);
    out
}

/// Collapse adjacent same-href anchors into one: a citation/URL split across styled
/// runs or line breaks ("Rad"+"ford", a wrapped DOI) emits `…</a><a href="H">…`.
/// When the just-closed `<a>` and the next opening `<a>` share the same href, drop
/// the boundary (keeping any whitespace) so the link is a single atomic anchor.
pub(crate) fn merge_adjacent_links(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut open_href: Option<String> = None;
    let mut rest = s;
    let read_anchor = |t: &str| -> Option<(String, usize)> {
        // (href, total tag length) for a leading `<a href="...">`
        let body = t.strip_prefix("<a href=\"")?;
        let q = body.find('"')?;
        let gt = body[q..].find('>')?;
        Some((body[..q].to_string(), 9 + q + gt + 1))
    };
    while !rest.is_empty() {
        if let Some((href, len)) = read_anchor(rest) {
            out.push_str(&rest[..len]);
            open_href = Some(href);
            rest = &rest[len..];
            continue;
        }
        if let Some(after) = rest.strip_prefix("</a>") {
            let ws = after.len() - after.trim_start().len();
            if let (Some(cur), Some((href2, len2))) = (&open_href, read_anchor(after[ws..].as_ref())) {
                if &href2 == cur {
                    // merge: keep whitespace, drop the </a> and the reopening <a>
                    out.push_str(&after[..ws]);
                    rest = &after[ws + len2..];
                    continue;
                }
            }
            out.push_str("</a>");
            open_href = None;
            rest = after;
            continue;
        }
        let c = rest.chars().next().unwrap();
        out.push(c);
        rest = &rest[c.len_utf8()..];
    }
    out
}

/// Doc-level pass: a real section title appears once, but a running page header
/// (the paper title or author list repeated atop every page) gets emitted as a
/// heading on each page. Any heading whose text (minus a leading page/section
/// number) recurs 3+ times across the document is a running head — demote those
/// occurrences from `<hN>` to `<p>` so they don't pollute the heading outline.
pub(crate) fn demote_running_headings(html: String) -> String {
    // Collect (range, inner) for every <h1-6>…</h1-6>.
    let bytes = html.as_bytes();
    let mut spans: Vec<(usize, usize, usize, usize, String)> = Vec::new(); // open,close_end,lvl, inner_start,inner
    let mut i = 0;
    while i + 3 < bytes.len() {
        if bytes[i] == b'<' && bytes[i + 1] == b'h' && matches!(bytes[i + 2], b'1'..=b'6') && bytes[i + 3] == b'>' {
            let lvl = (bytes[i + 2] - b'0') as usize;
            let close = format!("</h{lvl}>");
            if let Some(rel) = html[i..].find(&close) {
                let inner_start = i + 4;
                let inner = html[inner_start..i + rel].to_string();
                spans.push((i, i + rel + close.len(), lvl, inner_start, inner));
                i += rel + close.len();
                continue;
            }
        }
        i += 1;
    }
    // Count normalized keys (strip tags + a leading number/roman/letter token).
    let key = |inner: &str| -> String {
        let text: String = {
            let mut s = String::new();
            let mut intag = false;
            for c in inner.chars() {
                match c {
                    '<' => intag = true,
                    '>' => intag = false,
                    _ if !intag => s.push(c),
                    _ => {}
                }
            }
            s
        };
        let t = text.trim_start();
        // drop a leading "12 ", "3.2.1", "IV.", "A." token
        let t = t.trim_start_matches(|c: char| c.is_alphanumeric() || c == '.' );
        t.to_lowercase().split_whitespace().collect::<Vec<_>>().join(" ")
    };
    let mut counts: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for s in &spans {
        let k = key(&s.4);
        if k.len() >= 4 {
            *counts.entry(k).or_insert(0) += 1;
        }
    }
    // Rebuild, demoting repeated ones to <p>.
    let mut outp = String::with_capacity(html.len());
    let mut pos = 0;
    let mut kept_h1: std::collections::HashSet<String> = std::collections::HashSet::new();
    for (open, close_end, lvl, _is, inner) in &spans {
        let k = key(inner);
        if counts.get(&k).copied().unwrap_or(0) >= 3 {
            // A heading repeated ≥3× is a running page-header — demote to <p>. But the
            // document title legitimately recurs in the running head: keep its first
            // <h1> occurrence (the real title) and demote every other repeat.
            if *lvl == 1 && kept_h1.insert(k) {
                continue;
            }
            outp.push_str(&html[pos..*open]);
            outp.push_str("<p>");
            outp.push_str(inner);
            outp.push_str("</p>");
            pos = *close_end;
        }
    }
    outp.push_str(&html[pos..]);
    outp
}

/// Generic single pass over `\0<idx>\0` sentinels: each is replaced by `repl(idx)`'s
/// output (the closure pushes directly into the buffer). Non-sentinel text is copied
/// verbatim. NUL never occurs in real text/base64, so the markers are unambiguous.
pub(crate) fn rewrite_sentinels(html: &str, extra: usize, mut repl: impl FnMut(usize, &mut String)) -> String {
    let b = html.as_bytes();
    let mut out = String::with_capacity(html.len() + extra);
    let mut i = 0;
    let mut last = 0;
    while i < b.len() {
        if b[i] == 0 {
            let start = i;
            let mut j = i + 1;
            let mut idx = 0usize;
            let mut any = false;
            while j < b.len() && b[j].is_ascii_digit() {
                idx = idx * 10 + (b[j] - b'0') as usize;
                j += 1;
                any = true;
            }
            if any && j < b.len() && b[j] == 0 {
                out.push_str(&html[last..start]);
                repl(idx, &mut out);
                i = j + 1;
                last = i;
                continue;
            }
        }
        i += 1;
    }
    out.push_str(&html[last..]);
    out
}

/// Append one page's fragment to `out`, shifting its page-local `\0<idx>\0` image
/// sentinels by `offset` so they index into the document-wide URI list built at merge.
pub(crate) fn append_with_img_offset(out: &mut String, frag: &str, offset: usize) {
    if offset == 0 || !frag.as_bytes().contains(&0) {
        out.push_str(frag); // first page (local==global), or no image sentinels to shift
        return;
    }
    out.push_str(&rewrite_sentinels(frag, frag.len() / 8, |idx, o| {
        o.push('\u{0}');
        o.push_str(&(idx + offset).to_string());
        o.push('\u{0}');
    }));
}

/// Resolve the deferred image sentinels: inline mode splices the base64 data URI back
/// in; placeholder mode replaces the sentinel with the 1-based `<image N>` number.
pub(crate) fn substitute_images(html: String, uris: &[String], inline: bool) -> String {
    if uris.is_empty() {
        return html;
    }
    let extra: usize = uris.iter().map(|u| u.len()).sum::<usize>().max(uris.len() * 4);
    rewrite_sentinels(&html, extra, |idx, o| {
        if inline {
            if let Some(u) = uris.get(idx) {
                o.push_str(u);
            }
        } else {
            o.push_str(&(idx + 1).to_string());
        }
    })
}
