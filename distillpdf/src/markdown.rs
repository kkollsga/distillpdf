//! HTML → Markdown transform. `to_markdown()` is **not** a second renderer: it converts
//! the exact HTML that [`crate::html::to_html`] produces, so every processor improvement
//! (clipping, headings, front-matter, tables, …) flows into Markdown for free, with no
//! parallel logic to keep in sync.
//!
//! The input is our OWN small, well-formed tag set — `h1`–`h6`, `p`, `b/i`, `a`, `sup`,
//! `ul/ol/li`, `table/tr/th/td/caption`, `figure/figcaption`, `img`, `svg`, `nav`,
//! `header`, `section`, `pre/code` — so a targeted parser is reliable (no general-purpose
//! HTML parsing, no extra deps). `<svg>` subtrees are kept verbatim as opaque blocks.
//!
//! Images are handled per [`ImgMode`]: dropped to a caption-only placeholder, embedded as
//! a `data:` URI, or extracted to files (`img/fig_NN_slug.ext`) the caller writes.

use base64::Engine;

/// How figure graphics are rendered into the Markdown.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ImgMode {
    /// No bytes: a figure becomes `![caption](#fig-N)` — the caption is preserved, the
    /// image is not (`image_mode="drop"`).
    Placeholder,
    /// Self-contained: rasters keep their `data:` URI, vector figures are inlined as
    /// `data:image/svg+xml;base64,…` (`image_mode="embed"`).
    Embed,
    /// Extract every figure to a file under `img/` and reference it relatively
    /// (`image_mode="external"`). The files are returned to the caller.
    Files,
}

/// A figure file to write alongside the Markdown (`path` is relative, e.g. `img/fig_01_x.png`).
pub struct ImageFile {
    pub path: String,
    pub bytes: Vec<u8>,
}

/// Convert our HTML to GitHub-flavoured Markdown. Returns the Markdown plus any image
/// files to write (empty unless `img_mode == Files`). `toc` keeps/drops the `<nav>`.
pub fn html_to_markdown(html: &str, toc: bool, img_mode: ImgMode) -> (String, Vec<ImageFile>) {
    let nodes = parse(html);
    let mut ctx = Ctx { img_mode, toc, fig_n: 0, files: Vec::new() };
    let mut out = String::new();
    block(&nodes, &mut out, &mut ctx);
    // Collapse 3+ blank lines to a single blank line; trim trailing whitespace.
    let md = squeeze_blanks(&out);
    (md, ctx.files)
}

// ---------------------------------------------------------------------------- parsing

enum Node {
    Text(String),
    Elem { tag: String, attrs: Vec<(String, String)>, children: Vec<Node>, svg: Option<String> },
}

fn is_void(tag: &str) -> bool {
    matches!(tag, "meta" | "img" | "br" | "hr" | "link" | "input" | "col" | "source" | "image")
}

/// Parse our well-formed HTML into a node forest. Lenient (it only ever sees our own
/// output): unknown/mismatched close tags pop to the nearest match; `<svg>` and
/// `<style>`/`<script>` bodies are captured opaquely.
fn parse(html: &str) -> Vec<Node> {
    let b = html.as_bytes();
    let n = b.len();
    let mut i = 0;
    let mut stack: Vec<(String, Vec<(String, String)>, Vec<Node>)> = Vec::new();
    let mut root: Vec<Node> = Vec::new();

    fn push(stack: &mut [(String, Vec<(String, String)>, Vec<Node>)], root: &mut Vec<Node>, node: Node) {
        match stack.last_mut() {
            Some((_, _, ch)) => ch.push(node),
            None => root.push(node),
        }
    }

    while i < n {
        if b[i] == b'<' {
            // Comment / doctype.
            if html[i..].starts_with("<!--") {
                i = html[i..].find("-->").map(|p| i + p + 3).unwrap_or(n);
                continue;
            }
            if i + 1 < n && b[i + 1] == b'!' {
                i = html[i..].find('>').map(|p| i + p + 1).unwrap_or(n);
                continue;
            }
            // Close tag.
            if i + 1 < n && b[i + 1] == b'/' {
                let end = html[i..].find('>').map(|p| i + p).unwrap_or(n);
                let name = html[i + 2..end].trim().to_ascii_lowercase();
                // Pop to the matching open tag (inclusive).
                if let Some(pos) = stack.iter().rposition(|(t, _, _)| *t == name) {
                    while stack.len() > pos {
                        let (tag, attrs, children) = stack.pop().unwrap();
                        push(&mut stack, &mut root, Node::Elem { tag, attrs, children, svg: None });
                    }
                }
                i = end + 1;
                continue;
            }
            // Open tag.
            let end = match html[i..].find('>') {
                Some(p) => i + p,
                None => break,
            };
            let mut inner = &html[i + 1..end];
            let self_closing = inner.ends_with('/');
            if self_closing {
                inner = inner[..inner.len() - 1].trim_end();
            }
            let (name, attrs) = parse_tag(inner);
            // Opaque blocks: capture verbatim (svg) or discard (style/script).
            if name == "svg" {
                let raw = capture_balanced(html, i, "svg");
                let consumed = raw.len();
                push(&mut stack, &mut root, Node::Elem { tag: name, attrs, children: Vec::new(), svg: Some(raw) });
                i += consumed;
                continue;
            }
            if name == "style" || name == "script" {
                let close = format!("</{name}>");
                i = html[end..].find(&close).map(|p| end + p + close.len()).unwrap_or(n);
                continue;
            }
            if self_closing || is_void(&name) {
                push(&mut stack, &mut root, Node::Elem { tag: name, attrs, children: Vec::new(), svg: None });
            } else {
                stack.push((name, attrs, Vec::new()));
            }
            i = end + 1;
        } else {
            let end = html[i..].find('<').map(|p| i + p).unwrap_or(n);
            let txt = unescape(&html[i..end]);
            if !txt.is_empty() {
                push(&mut stack, &mut root, Node::Text(txt));
            }
            i = end;
        }
    }
    // Close any still-open tags.
    while let Some((tag, attrs, children)) = stack.pop() {
        push(&mut stack, &mut root, Node::Elem { tag, attrs, children, svg: None });
    }
    root
}

/// Capture `<svg …> … </svg>` verbatim from `start`, honouring nested `<svg>`.
fn capture_balanced(html: &str, start: usize, tag: &str) -> String {
    let open = format!("<{tag}");
    let close = format!("</{tag}>");
    let mut depth = 0i32;
    let mut i = start;
    let n = html.len();
    while i < n {
        if html[i..].starts_with(&open) {
            depth += 1;
            i += open.len();
        } else if html[i..].starts_with(&close) {
            depth -= 1;
            i += close.len();
            if depth == 0 {
                return html[start..i].to_string();
            }
        } else {
            // Advance one full UTF-8 char, not one byte — SVG content may hold multi-byte
            // chars (e.g. accented text labels), and a byte step would land `i` mid-char and
            // panic the next `html[i..]`.
            i += html[i..].chars().next().map_or(1, |c| c.len_utf8());
        }
    }
    html[start..].to_string()
}

/// Split a tag's inner text into (lowercased name, attrs). Attribute values may be quoted
/// (`"`/`'`) or bare; a value-less attr (e.g. the `1` in the `<image 1>` placeholder) maps
/// to an empty value.
fn parse_tag(inner: &str) -> (String, Vec<(String, String)>) {
    let inner = inner.trim();
    // name
    let mut name_end = inner.len();
    for (idx, c) in inner.char_indices() {
        if c.is_whitespace() {
            name_end = idx;
            break;
        }
    }
    let name = inner[..name_end].to_ascii_lowercase();
    let mut attrs = Vec::new();
    let rest = inner[name_end..].trim_start();
    let rb = rest.as_bytes();
    let mut i = 0;
    while i < rb.len() {
        // skip whitespace
        while i < rb.len() && (rb[i] as char).is_whitespace() {
            i += 1;
        }
        if i >= rb.len() {
            break;
        }
        // attr name
        let ks = i;
        while i < rb.len() && rb[i] != b'=' && !(rb[i] as char).is_whitespace() {
            i += 1;
        }
        let key = rest[ks..i].to_ascii_lowercase();
        // optional = value
        while i < rb.len() && (rb[i] as char).is_whitespace() {
            i += 1;
        }
        let mut val = String::new();
        if i < rb.len() && rb[i] == b'=' {
            i += 1;
            while i < rb.len() && (rb[i] as char).is_whitespace() {
                i += 1;
            }
            if i < rb.len() && (rb[i] == b'"' || rb[i] == b'\'') {
                let q = rb[i];
                i += 1;
                let vs = i;
                while i < rb.len() && rb[i] != q {
                    i += 1;
                }
                val = unescape(&rest[vs..i.min(rest.len())]);
                i += 1; // closing quote
            } else {
                let vs = i;
                while i < rb.len() && !(rb[i] as char).is_whitespace() {
                    i += 1;
                }
                val = unescape(&rest[vs..i]);
            }
        }
        if !key.is_empty() {
            attrs.push((key, val));
        }
    }
    (name, attrs)
}

fn unescape(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    s.replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&#39;", "'").replace("&amp;", "&")
}

// --------------------------------------------------------------------------- rendering

struct Ctx {
    img_mode: ImgMode,
    toc: bool,
    fig_n: usize,
    files: Vec<ImageFile>,
}

fn attr<'a>(attrs: &'a [(String, String)], key: &str) -> Option<&'a str> {
    attrs.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
}

/// Render block-level nodes, separating blocks with a blank line.
fn block(nodes: &[Node], out: &mut String, ctx: &mut Ctx) {
    for node in nodes {
        match node {
            Node::Text(t) => {
                if !t.trim().is_empty() {
                    push_block(out, &collapse_ws(t));
                }
            }
            Node::Elem { tag, attrs, children, svg } => render_elem(tag, attrs, children, svg.as_deref(), out, ctx),
        }
    }
}

fn render_elem(tag: &str, attrs: &[(String, String)], children: &[Node], svg: Option<&str>, out: &mut String, ctx: &mut Ctx) {
    match tag {
        // Transparent containers: render their children at block level.
        "html" | "body" | "head" | "header" | "section" | "article" | "main" | "div" => block(children, out, ctx),
        "nav" => {
            if ctx.toc {
                render_nav(children, out);
            }
        }
        "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
            let level = tag.as_bytes()[1] - b'0';
            let hashes = "#".repeat(level as usize);
            // A heading is already strong, so drop a bold span that wraps the whole title
            // (our HTML emits `<h2><b>1.4 Database</b></h2>` for bold-detected headings).
            let text = strip_full_bold(inline(children, ctx).trim());
            push_block(out, &format!("{hashes} {text}"));
        }
        "p" => {
            let s = inline(children, ctx);
            if !s.trim().is_empty() {
                push_block(out, s.trim());
            }
        }
        "ul" => render_list(children, out, ctx, false),
        "ol" => render_list(children, out, ctx, true),
        "pre" => {
            // <pre><code>…</code></pre> → fenced block; keep text verbatim.
            let code = plain_text(children);
            push_block(out, &format!("```\n{}\n```", code.trim_end_matches('\n')));
        }
        "blockquote" => {
            let mut inner = String::new();
            block(children, &mut inner, ctx);
            let quoted: String = inner.trim().lines().map(|l| format!("> {l}")).collect::<Vec<_>>().join("\n");
            push_block(out, &quoted);
        }
        "table" => render_table(children, attrs, out, ctx),
        "figure" => render_figure(children, attrs, out, ctx),
        "img" => {
            // A standalone image (not wrapped in <figure>).
            let g = Graphic::from_img(attrs);
            let md = emit_image(ctx, "", g, None);
            push_block(out, &md);
        }
        "image" => {
            // Placeholder graphic outside a figure.
            ctx.fig_n += 1;
            push_block(out, &format!("![](#fig-{})", ctx.fig_n));
        }
        "svg" => {
            let g = Graphic::Svg(svg.unwrap_or("").to_string());
            let md = emit_image(ctx, "", g, None);
            push_block(out, &md);
        }
        "hr" => push_block(out, "---"),
        // Anything else: fall back to inline.
        _ => {
            let s = inline(std::slice::from_ref(&Node::Elem { tag: tag.to_string(), attrs: attrs.to_vec(), children: clone_nodes(children), svg: svg.map(|s| s.to_string()) }), ctx);
            if !s.trim().is_empty() {
                push_block(out, s.trim());
            }
        }
    }
}

/// Inline rendering: bold/italic/links/sup/code/text, returns a single-line-ish string.
fn inline(nodes: &[Node], ctx: &mut Ctx) -> String {
    let mut s = String::new();
    for node in nodes {
        match node {
            Node::Text(t) => s.push_str(&collapse_ws(t)),
            Node::Elem { tag, attrs, children, svg } => match tag.as_str() {
                "b" | "strong" => {
                    let inner = inline(children, ctx);
                    if !inner.trim().is_empty() {
                        s.push_str(&format!("**{}**", inner.trim()));
                    }
                }
                "i" | "em" => {
                    let inner = inline(children, ctx);
                    if !inner.trim().is_empty() {
                        s.push_str(&format!("*{}*", inner.trim()));
                    }
                }
                "code" => s.push_str(&format!("`{}`", plain_text(children))),
                "a" => {
                    let txt = inline(children, ctx);
                    match attr(attrs, "href") {
                        Some(h) if !txt.trim().is_empty() => s.push_str(&format!("[{}]({})", txt.trim(), md_anchor(h))),
                        _ => s.push_str(txt.trim()),
                    }
                }
                "sup" => s.push_str(&format!("^{}", inline(children, ctx).trim())),
                "sub" => s.push_str(&inline(children, ctx)),
                "br" => s.push('\n'),
                "img" => {
                    let g = Graphic::from_img(attrs);
                    s.push_str(&emit_image(ctx, "", g, None));
                }
                "image" => {
                    ctx.fig_n += 1;
                    s.push_str(&format!("![](#fig-{})", ctx.fig_n));
                }
                "svg" => {
                    let g = Graphic::Svg(svg.clone().unwrap_or_default());
                    s.push_str(&emit_image(ctx, "", g, None));
                }
                _ => s.push_str(&inline(children, ctx)),
            },
        }
    }
    s
}

fn render_nav(children: &[Node], out: &mut String) {
    // <nav><ul><li data-level=L><a href=#sec-…>title</a></li>…</ul></nav>
    fn collect(nodes: &[Node], items: &mut Vec<(u8, String, String)>) {
        for n in nodes {
            if let Node::Elem { tag, attrs, children, .. } = n {
                if tag == "li" {
                    let lvl = attr(attrs, "data-level").and_then(|v| v.parse().ok()).unwrap_or(2u8);
                    // first <a> descendant
                    if let Some((href, title)) = first_link(children) {
                        items.push((lvl, title, href));
                    }
                }
                collect(children, items);
            }
        }
    }
    let mut items = Vec::new();
    collect(children, &mut items);
    if items.is_empty() {
        return;
    }
    let mut s = String::new();
    for (lvl, title, href) in items {
        let indent = "  ".repeat((lvl.saturating_sub(2)) as usize);
        s.push_str(&format!("{indent}- [{title}]({})\n", md_anchor(&href)));
    }
    push_block(out, s.trim_end());
}

fn first_link(nodes: &[Node]) -> Option<(String, String)> {
    for n in nodes {
        if let Node::Elem { tag, attrs, children, .. } = n {
            if tag == "a" {
                let href = attr(attrs, "href").unwrap_or("").to_string();
                return Some((href, plain_text(children).trim().to_string()));
            }
            if let Some(r) = first_link(children) {
                return Some(r);
            }
        }
    }
    None
}

fn render_list(children: &[Node], out: &mut String, ctx: &mut Ctx, ordered: bool) {
    let mut s = String::new();
    let mut idx = 1;
    for node in children {
        if let Node::Elem { tag, children: li_children, .. } = node {
            if tag == "li" {
                let marker = if ordered { format!("{idx}. ") } else { "- ".to_string() };
                // Inline content of the li, plus any nested lists indented.
                let mut nested = String::new();
                let mut inline_nodes: Vec<&Node> = Vec::new();
                for c in li_children {
                    match c {
                        Node::Elem { tag: ct, .. } if ct == "ul" || ct == "ol" => {
                            let mut sub = String::new();
                            if let Node::Elem { children: sc, tag: st, .. } = c {
                                render_list(sc, &mut sub, ctx, st == "ol");
                            }
                            for l in sub.trim_end().lines() {
                                nested.push_str(&format!("\n  {l}"));
                            }
                        }
                        other => inline_nodes.push(other),
                    }
                }
                let owned: Vec<Node> = inline_nodes.into_iter().map(clone_node).collect();
                let text = inline(&owned, ctx);
                s.push_str(&format!("{marker}{}{}\n", text.trim(), nested));
                idx += 1;
            }
        }
    }
    push_block(out, s.trim_end());
}

fn render_table(children: &[Node], attrs: &[(String, String)], out: &mut String, ctx: &mut Ctx) {
    // Optional <caption> before the grid.
    let mut caption = String::new();
    let mut rows: Vec<Vec<String>> = Vec::new();
    fn walk_rows(nodes: &[Node], rows: &mut Vec<Vec<String>>, caption: &mut String, ctx: &mut Ctx) {
        for n in nodes {
            if let Node::Elem { tag, children, .. } = n {
                match tag.as_str() {
                    "caption" => *caption = inline(children, ctx).trim().to_string(),
                    "tr" => {
                        let mut cells = Vec::new();
                        for c in children {
                            if let Node::Elem { tag: ct, children: cc, .. } = c {
                                if ct == "td" || ct == "th" {
                                    let v = inline(cc, ctx).replace('|', "\\|").replace('\n', " ");
                                    cells.push(v.trim().to_string());
                                }
                            }
                        }
                        rows.push(cells);
                    }
                    "thead" | "tbody" | "tfoot" => walk_rows(children, rows, caption, ctx),
                    _ => {}
                }
            }
        }
    }
    let _ = attrs;
    walk_rows(children, &mut rows, &mut caption, ctx);
    rows.retain(|r| !r.is_empty());
    if rows.is_empty() {
        return;
    }
    let cols = rows.iter().map(|r| r.len()).max().unwrap_or(0);
    let mut s = String::new();
    if !caption.is_empty() {
        s.push_str(&format!("**{caption}**\n\n"));
    }
    let pad = |r: &Vec<String>| {
        let mut cells = r.clone();
        cells.resize(cols, String::new());
        format!("| {} |", cells.join(" | "))
    };
    s.push_str(&pad(&rows[0]));
    s.push('\n');
    s.push_str(&format!("| {} |\n", vec!["---"; cols].join(" | ")));
    for r in &rows[1..] {
        s.push_str(&pad(r));
        s.push('\n');
    }
    push_block(out, s.trim_end());
}

fn render_figure(children: &[Node], attrs: &[(String, String)], out: &mut String, ctx: &mut Ctx) {
    let fig_id = attr(attrs, "id").map(|s| s.to_string());
    // Find the caption and the graphic.
    let mut caption = String::new();
    let mut graphic = Graphic::None;
    fn find(nodes: &[Node], caption: &mut String, graphic: &mut Graphic, ctx: &mut Ctx) {
        for n in nodes {
            if let Node::Elem { tag, attrs, children, svg } = n {
                match tag.as_str() {
                    "figcaption" => *caption = inline(children, ctx).trim().to_string(),
                    "img" if matches!(graphic, Graphic::None) => *graphic = Graphic::from_img(attrs),
                    "svg" if matches!(graphic, Graphic::None) => *graphic = Graphic::Svg(svg.clone().unwrap_or_default()),
                    "image" if matches!(graphic, Graphic::None) => *graphic = Graphic::Placeholder,
                    _ => find(children, caption, graphic, ctx),
                }
            }
        }
    }
    find(children, &mut caption, &mut graphic, ctx);
    let md = emit_image(ctx, &caption, graphic, fig_id.as_deref());
    push_block(out, &md);
}

// ------------------------------------------------------------------------------- images

enum Graphic {
    None,
    Raster { uri: String }, // data: URI (or any src)
    Svg(String),            // raw <svg>…</svg>
    Placeholder,
}
impl Graphic {
    fn from_img(attrs: &[(String, String)]) -> Graphic {
        match attr(attrs, "src") {
            Some(s) if !s.is_empty() => Graphic::Raster { uri: s.to_string() },
            _ => Graphic::Placeholder,
        }
    }
}

/// Emit a Markdown image for a figure/graphic, honouring [`ImgMode`]. `fig_id` is the
/// HTML `id` (e.g. `fig-3`), used for the placeholder anchor.
fn emit_image(ctx: &mut Ctx, caption: &str, graphic: Graphic, fig_id: Option<&str>) -> String {
    ctx.fig_n += 1;
    let n = ctx.fig_n;
    let alt = caption.trim();
    let anchor = fig_id.map(|s| s.to_string()).unwrap_or_else(|| format!("fig-{n}"));

    // Placeholder mode (or a graphic we can't materialise): caption only.
    if ctx.img_mode == ImgMode::Placeholder || matches!(graphic, Graphic::None | Graphic::Placeholder) {
        return format!("![{alt}](#{anchor})");
    }

    match (ctx.img_mode, graphic) {
        // Embed: keep the bytes inline.
        (ImgMode::Embed, Graphic::Raster { uri }) => format!("![{alt}]({uri})"),
        (ImgMode::Embed, Graphic::Svg(raw)) => {
            let b64 = base64::engine::general_purpose::STANDARD.encode(raw.as_bytes());
            format!("![{alt}](data:image/svg+xml;base64,{b64})")
        }
        // Files: extract the graphic to img/ and reference it.
        (ImgMode::Files, Graphic::Raster { uri }) => match decode_data_uri(&uri) {
            Some((bytes, ext)) => {
                let path = fig_path(n, alt, &ext);
                ctx.files.push(ImageFile { path: path.clone(), bytes });
                format!("![{alt}]({path})")
            }
            None => format!("![{alt}]({uri})"), // not a data URI — reference verbatim
        },
        (ImgMode::Files, Graphic::Svg(raw)) => {
            let path = fig_path(n, alt, "svg");
            ctx.files.push(ImageFile { path: path.clone(), bytes: raw.into_bytes() });
            format!("![{alt}]({path})")
        }
        // None/Placeholder handled above.
        _ => format!("![{alt}](#{anchor})"),
    }
}

/// `img/fig_NN_slug.ext` — NN is the running figure index, slug a ≤20-char slug of the
/// caption (caption's leading "Figure N:" label stripped first).
fn fig_path(n: usize, caption: &str, ext: &str) -> String {
    let slug = caption_slug(caption);
    if slug.is_empty() {
        format!("img/fig_{n:02}.{ext}")
    } else {
        format!("img/fig_{n:02}_{slug}.{ext}")
    }
}

fn caption_slug(caption: &str) -> String {
    // Drop a leading "Figure 5.7:" / "Fig. 3 -" style label.
    let lower = caption.trim();
    let body = {
        let l = lower.to_ascii_lowercase();
        if l.starts_with("figure") || l.starts_with("fig.") || l.starts_with("fig ") {
            // skip the label up to the first ':' or '.' or '-' separator, then the number.
            lower.split_once([':', '–', '-']).map(|x| x.1).unwrap_or(lower)
        } else {
            lower
        }
    };
    let mut slug = String::new();
    let mut prev_dash = false;
    for c in body.trim().chars() {
        if c.is_ascii_alphanumeric() {
            slug.push(c.to_ascii_lowercase());
            prev_dash = false;
        } else if !prev_dash && !slug.is_empty() {
            slug.push('-');
            prev_dash = true;
        }
        if slug.len() >= 20 {
            break;
        }
    }
    slug.trim_matches('-').to_string()
}

/// Decode a `data:image/<fmt>;base64,…` URI into raw bytes + a file extension.
fn decode_data_uri(uri: &str) -> Option<(Vec<u8>, String)> {
    let rest = uri.strip_prefix("data:")?;
    let (meta, data) = rest.split_once(',')?;
    if !meta.contains("base64") {
        return None;
    }
    let mime = meta.split(';').next().unwrap_or("");
    let ext = match mime {
        "image/png" => "png",
        "image/jpeg" | "image/jpg" => "jpg",
        "image/gif" => "gif",
        "image/webp" => "webp",
        "image/svg+xml" => "svg",
        _ => "bin",
    };
    let bytes = base64::engine::general_purpose::STANDARD.decode(data.trim()).ok()?;
    Some((bytes, ext.to_string()))
}

// ------------------------------------------------------------- HTML image externalisation

/// Rewrite a finished HTML document so figure images live in an `img/` folder instead of
/// inline `data:` URIs (the HTML analogue of Markdown's file mode). Returns the rewritten
/// HTML plus the files to write. A raster `<img src="data:…">` keeps its tag with `src`
/// repointed to `img/fig_NN_slug.ext`; an inline figure `<svg>…</svg>` is written out as
/// `img/fig_NN_slug.svg` and replaced by an `<img>` referencing it. Naming matches
/// [`html_to_markdown`] so the two formats produce the same `img/` filenames.
pub fn externalize_images(html: &str) -> (String, Vec<ImageFile>) {
    let nodes = parse(html);
    let mut ctx = ExtCtx { fig_n: 0, files: Vec::new(), repls: Vec::new() };
    ext_walk(&nodes, &mut ctx);
    // Apply replacements in document order (each `from` — a unique data URI or the exact
    // raw `<svg>` — is replaced once; the `to` never reintroduces a `from`).
    let mut out = html.to_string();
    for (from, to) in &ctx.repls {
        out = out.replacen(from, to, 1);
    }
    (out, ctx.files)
}

struct ExtCtx {
    fig_n: usize,
    files: Vec<ImageFile>,
    repls: Vec<(String, String)>,
}

fn ext_walk(nodes: &[Node], ctx: &mut ExtCtx) {
    for n in nodes {
        if let Node::Elem { tag, attrs, children, svg } = n {
            match tag.as_str() {
                "figure" => {
                    ctx.fig_n += 1;
                    let num = ctx.fig_n;
                    let caption = figcaption_text(children);
                    ext_graphic(children, &caption, num, ctx);
                }
                "img" => {
                    ctx.fig_n += 1;
                    let num = ctx.fig_n;
                    if let Some(src) = attr(attrs, "src") {
                        ext_raster(src, "", num, ctx);
                    }
                }
                "svg" => {
                    ctx.fig_n += 1;
                    let num = ctx.fig_n;
                    ext_svg(svg.as_deref().unwrap_or(""), "", num, ctx);
                }
                _ => ext_walk(children, ctx),
            }
        }
    }
}

/// Externalise the first raster/svg graphic in a figure's children (its `<figcaption>` is
/// skipped). Returns once a graphic is handled.
fn ext_graphic(nodes: &[Node], caption: &str, num: usize, ctx: &mut ExtCtx) -> bool {
    for n in nodes {
        if let Node::Elem { tag, attrs, children, svg } = n {
            match tag.as_str() {
                "figcaption" => {}
                "img" => {
                    if let Some(src) = attr(attrs, "src") {
                        ext_raster(src, caption, num, ctx);
                        return true;
                    }
                }
                "svg" => {
                    ext_svg(svg.as_deref().unwrap_or(""), caption, num, ctx);
                    return true;
                }
                _ => {
                    if ext_graphic(children, caption, num, ctx) {
                        return true;
                    }
                }
            }
        }
    }
    false
}

fn ext_raster(src: &str, caption: &str, num: usize, ctx: &mut ExtCtx) {
    if let Some((bytes, ext)) = decode_data_uri(src) {
        let path = fig_path(num, caption, &ext);
        ctx.files.push(ImageFile { path: path.clone(), bytes });
        // Repoint just the src value; the rest of the <img> tag is untouched.
        ctx.repls.push((src.to_string(), path));
    }
}

fn ext_svg(raw: &str, caption: &str, num: usize, ctx: &mut ExtCtx) {
    if raw.is_empty() {
        return;
    }
    let path = fig_path(num, caption, "svg");
    ctx.files.push(ImageFile { path: path.clone(), bytes: raw.as_bytes().to_vec() });
    ctx.repls.push((raw.to_string(), format!("<img src=\"{}\" alt=\"{}\" />", path, esc_attr(caption))));
}

fn figcaption_text(nodes: &[Node]) -> String {
    for n in nodes {
        if let Node::Elem { tag, children, .. } = n {
            if tag == "figcaption" {
                return plain_text(children).trim().to_string();
            }
            let t = figcaption_text(children);
            if !t.is_empty() {
                return t;
            }
        }
    }
    String::new()
}

fn esc_attr(s: &str) -> String {
    s.replace('&', "&amp;").replace('"', "&quot;").replace('<', "&lt;")
}

// -------------------------------------------------------------------------------- utils

/// Turn an HTML href into a Markdown-friendly anchor: external URIs pass through; an
/// internal `#sec-foo` becomes `#foo` (closer to GitHub's heading-slug algorithm).
fn md_anchor(href: &str) -> String {
    match href.strip_prefix("#sec-") {
        Some(rest) => format!("#{rest}"),
        None => href.to_string(),
    }
}

/// Strip a `**…**` that wraps the entire string (no other `**` inside) — redundant when
/// the surrounding context (a heading) already implies emphasis.
fn strip_full_bold(s: &str) -> String {
    let t = s.trim();
    if let Some(inner) = t.strip_prefix("**").and_then(|x| x.strip_suffix("**")) {
        if !inner.contains("**") && !inner.is_empty() {
            return inner.to_string();
        }
    }
    t.to_string()
}

fn plain_text(nodes: &[Node]) -> String {
    let mut s = String::new();
    for n in nodes {
        match n {
            Node::Text(t) => s.push_str(t),
            Node::Elem { children, .. } => s.push_str(&plain_text(children)),
        }
    }
    s
}

fn collapse_ws(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_space = false;
    for c in s.chars() {
        if c.is_whitespace() {
            if !prev_space {
                out.push(' ');
            }
            prev_space = true;
        } else {
            out.push(c);
            prev_space = false;
        }
    }
    out
}

/// Append a block, ensuring exactly one blank line before it.
fn push_block(out: &mut String, block: &str) {
    let block = block.trim_matches('\n');
    if block.is_empty() {
        return;
    }
    if !out.is_empty() {
        if !out.ends_with('\n') {
            out.push('\n');
        }
        out.push('\n');
    }
    out.push_str(block);
}

fn squeeze_blanks(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut blanks = 0;
    for line in s.lines() {
        if line.trim().is_empty() {
            blanks += 1;
            if blanks <= 1 {
                out.push('\n');
            }
        } else {
            blanks = 0;
            out.push_str(line.trim_end());
            out.push('\n');
        }
    }
    while out.ends_with('\n') {
        out.pop();
    }
    out.push('\n');
    out
}

// Cheap deep clone for the few places we re-wrap borrowed nodes.
fn clone_node(n: &Node) -> Node {
    match n {
        Node::Text(t) => Node::Text(t.clone()),
        Node::Elem { tag, attrs, children, svg } => Node::Elem { tag: tag.clone(), attrs: attrs.clone(), children: clone_nodes(children), svg: svg.clone() },
    }
}
fn clone_nodes(nodes: &[Node]) -> Vec<Node> {
    nodes.iter().map(clone_node).collect()
}

#[cfg(test)]
mod md_tests {
    use super::*;

    #[test]
    fn svg_with_multibyte_chars_does_not_panic() {
        // Regression: capture_balanced stepped one BYTE at a time, landing mid-char on
        // multi-byte UTF-8 (e.g. the accented label below) and panicking on the next slice.
        let html = "<body><p>Antes</p><svg><text>Inválido Ré Ção</text></svg><p>Depois</p></body>";
        let (md, _) = html_to_markdown(html, false, ImgMode::Placeholder);
        assert!(md.contains("Antes") && md.contains("Depois"));
    }
}
