//! Document style profile: a one-pass analysis of the whole document's typography —
//! the body text size/weight/font, the heading size/weight/font tiers (ranked to levels),
//! whether sections are numbered, whether a PDF outline exists, and the column layout.
//!
//! Computed once up front (see `build_doc_profile` in `html.rs`) and fed into the heading
//! decision gates so classification adapts to each document instead of relying on fixed
//! global thresholds (e.g. a heading face only 1.1× body, set in a distinct font, that the
//! old `ratio >= 1.18` rule missed). Plain data only — the clustering logic lives in
//! `html.rs` next to the span model it reads.

use std::collections::HashSet;

/// One detected heading style: a size/weight/font signature and the heading level it maps
/// to. `level` is the `detect_header` level (1 → `<h2>`, 2 → `<h3>`, …); `<h1>` is reserved
/// for the document title and handled separately.
#[derive(Clone, Copy, Debug)]
pub struct HeadingTier {
    pub size: f32,
    pub bold: bool,
    pub font: u32,
    pub level: u8,
}

// Several fields are recorded by the document analysis but not consumed yet (this pass
// wires only heading classification); they are the foundation for migrating the other
// decision gates later, so keep them even while unread.
#[allow(dead_code)]
#[derive(Default, Clone)]
pub struct DocProfile {
    pub body_size: f32,
    pub body_font: u32,
    pub body_bold: bool,
    pub title_sz: f32,
    /// Heading tiers, largest size first.
    pub tiers: Vec<HeadingTier>,
    /// The document uses numbered section headings ("3 Title", "3.2 Title", …).
    pub numbered: bool,
    /// The PDF carries an `/Outlines` bookmark tree (recorded; not consumed yet).
    pub has_outline: bool,
    /// Dominant column count (1 or 2; recorded; not consumed yet).
    pub columns: u8,
    /// Repeated running header/footer lines (recorded; populated by a later pass).
    pub running_heads: HashSet<String>,
}

impl DocProfile {
    /// The heading level for a line with this `(size, font, all_bold)` signature, if it
    /// matches a detected heading tier. `None` for body text. A tier with `font == 0`
    /// matches any font (size/weight-only tier); a bold tier requires the line to be bold.
    pub fn heading_level(&self, size: f32, font: u32, all_bold: bool) -> Option<u8> {
        for t in &self.tiers {
            let size_ok = (size - t.size).abs() <= 0.6;
            let font_ok = t.font == 0 || self.body_font == 0 || font == t.font;
            let bold_ok = !t.bold || all_bold;
            if size_ok && font_ok && bold_ok {
                return Some(t.level);
            }
        }
        None
    }
}
