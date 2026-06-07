//! Render an [`OcrPage`] block model into distillPDF-style HTML, reusing the same
//! escaping and id conventions as the born-digital path (`html::esc`, `html::slug`,
//! `sec-…`/`tab-…`/`fig-…` anchors) so OCR'd pages flow through the same
//! markdown/section/nav machinery.

use crate::html::{esc, slug};
use crate::ocr::doctags::{Block, Cell, OcrPage, Table};

/// Optional per-figure image markup (e.g. an `<img …>` data URI) keyed by the
/// picture's 0-based index within the page, supplied by the integration layer.
pub(crate) struct RenderOpts<'a> {
    pub figure_img: Option<&'a dyn Fn(usize) -> Option<String>>,
    /// Drop running page header/footer lines (they're usually stamps/noise).
    pub drop_running: bool,
}

impl Default for RenderOpts<'_> {
    fn default() -> Self {
        RenderOpts { figure_img: None, drop_running: false }
    }
}

/// Render one page's blocks to an HTML fragment.
pub(crate) fn render_page(page: &OcrPage, opts: &RenderOpts) -> String {
    let mut out = String::new();
    let mut pic_idx = 0usize;
    let mut i = 0;
    let blocks = &page.blocks;
    while i < blocks.len() {
        match &blocks[i] {
            Block::Title(t) => {
                let s = esc(t.text.trim());
                if !s.is_empty() {
                    out.push_str(&format!("<h1 id=\"sec-{}\">{}</h1>\n", slug(&t.text), s));
                }
            }
            Block::Heading { level, block } => {
                let s = esc(block.text.trim());
                if !s.is_empty() {
                    let h = (level + 1).clamp(2, 6); // DocTags level 1 → <h2> (title is <h1>)
                    out.push_str(&format!("<h{h} id=\"sec-{}\">{}</h{h}>\n", slug(&block.text), s));
                }
            }
            Block::ListItem(_) => {
                // group a run of consecutive list items into one <ul>
                out.push_str("<ul>\n");
                while let Some(Block::ListItem(li)) = blocks.get(i) {
                    let s = esc(li.text.trim());
                    if !s.is_empty() {
                        out.push_str(&format!("<li>{s}</li>\n"));
                    }
                    i += 1;
                }
                out.push_str("</ul>\n");
                continue; // i already advanced
            }
            Block::Code(c) => {
                out.push_str(&format!("<pre><code>{}</code></pre>\n", esc(&c.text)));
            }
            Block::Formula(f) => {
                let s = esc(f.text.trim());
                if !s.is_empty() {
                    out.push_str(&format!("<p class=\"formula\">{s}</p>\n"));
                }
            }
            Block::Caption(c) => {
                let s = esc(c.text.trim());
                if !s.is_empty() {
                    out.push_str(&format!("<p class=\"caption\">{s}</p>\n"));
                }
            }
            Block::Footnote(f) => {
                let s = esc(f.text.trim());
                if !s.is_empty() {
                    out.push_str(&format!("<p class=\"footnote\">{s}</p>\n"));
                }
            }
            Block::PageHeader(t) | Block::PageFooter(t) => {
                if !opts.drop_running {
                    let s = esc(t.text.trim());
                    if !s.is_empty() {
                        out.push_str(&format!("<p>{s}</p>\n"));
                    }
                }
            }
            Block::Para(t) => {
                let s = esc(t.text.trim());
                if !s.is_empty() {
                    out.push_str(&format!("<p>{s}</p>\n"));
                }
            }
            Block::Picture { class, caption, .. } => {
                out.push_str(&render_figure(pic_idx, class.as_deref(), caption.as_deref(), opts));
                pic_idx += 1;
            }
            Block::Table(t) => out.push_str(&render_table(t)),
        }
        i += 1;
    }
    out
}

fn render_figure(idx: usize, class: Option<&str>, caption: Option<&str>, opts: &RenderOpts) -> String {
    let img = opts.figure_img.and_then(|f| f(idx)).unwrap_or_default();
    let cls = class.unwrap_or("figure");
    let cap = match caption {
        Some(c) if !c.trim().is_empty() => esc(c.trim()),
        _ => esc(&pretty_class(cls)),
    };
    format!(
        "<figure id=\"fig-{idx}\" data-class=\"{}\">{img}<figcaption>{cap}</figcaption></figure>\n",
        esc(cls)
    )
}

fn pretty_class(c: &str) -> String {
    let mut s = c.replace('_', " ");
    if let Some(first) = s.get_mut(0..1) {
        first.make_ascii_uppercase();
    }
    s
}

fn render_table(t: &Table) -> String {
    let mut out = String::from("<table>\n");
    if let Some(cap) = &t.caption {
        if !cap.trim().is_empty() {
            out.push_str(&format!("<caption>{}</caption>\n", esc(cap.trim())));
        }
    }
    for row in &t.rows {
        out.push_str("<tr>");
        for c in row {
            out.push_str(&render_cell(c));
        }
        out.push_str("</tr>\n");
    }
    out.push_str("</table>\n");
    out
}

fn render_cell(c: &Cell) -> String {
    let tag = if c.header { "th" } else { "td" };
    let mut attrs = String::new();
    if c.colspan > 1 {
        attrs.push_str(&format!(" colspan=\"{}\"", c.colspan));
    }
    if c.rowspan > 1 {
        attrs.push_str(&format!(" rowspan=\"{}\"", c.rowspan));
    }
    format!("<{tag}{attrs}>{}</{tag}>", esc(c.text.trim()))
}

/// Convenience: parse DocTags and render to an HTML fragment in one call.
pub(crate) fn doctags_to_html(doctags: &str) -> String {
    render_page(&crate::ocr::doctags::parse(doctags), &RenderOpts::default())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ocr::doctags::parse;

    #[test]
    fn headings_paragraphs_lists() {
        let dt = "<loc_1><loc_1><loc_9><loc_9>TERMO DE DECLARAÇÕES\n\
                  <loc_1><loc_1><loc_9><loc_9>Body paragraph one.\n\
                  <list_item><loc_1><loc_1><loc_9><loc_9>first</list_item>\
                  <list_item><loc_1><loc_1><loc_9><loc_9>second</list_item>";
        let html = render_page(&parse(dt), &RenderOpts::default());
        assert!(html.contains("<h2 id=\"sec-"));
        assert!(html.contains("TERMO DE DECLARAÇÕES"));
        assert!(html.contains("<p>Body paragraph one.</p>"));
        assert!(html.contains("<ul>"));
        assert_eq!(html.matches("<li>").count(), 2);
    }

    #[test]
    fn table_render_headers_and_span() {
        let dt = "<otsl><ched>Year<ched>Value<nl><fcel>2019<fcel>42<nl></otsl>";
        let html = render_page(&parse(dt), &RenderOpts::default());
        assert!(html.contains("<table>"));
        assert_eq!(html.matches("<th>").count(), 2);
        assert!(html.contains("<td>2019</td>"));
    }

    #[test]
    fn picture_figure_with_class() {
        let dt = "<loc_1><loc_1><loc_9><loc_9><logo>";
        let html = render_page(&parse(dt), &RenderOpts::default());
        assert!(html.contains("<figure id=\"fig-0\" data-class=\"logo\">"));
        assert!(html.contains("<figcaption>Logo</figcaption>"));
    }

    #[test]
    fn figure_image_injection() {
        let dt = "<loc_1><loc_1><loc_9><loc_9><logo>";
        let img = |_i: usize| Some("<img src=\"data:x\">".to_string());
        let opts = RenderOpts { figure_img: Some(&img), drop_running: false };
        let html = render_page(&parse(dt), &opts);
        assert!(html.contains("<img src=\"data:x\">"));
    }

    #[test]
    fn escapes_html() {
        let dt = "<loc_1><loc_1><loc_9><loc_9>a < b & c > d";
        let html = doctags_to_html(dt);
        assert!(html.contains("&lt;") && html.contains("&amp;"));
    }
}
