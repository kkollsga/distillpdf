//! Front-matter of an academic paper: the title, authors (linked to their
//! organisation), abstract, and keywords extracted from page 1. Plain data only —
//! the detection that fills these lives in `html.rs` (next to the line/run model it
//! reads), and `lib.rs` turns this into the `pdf.metadata()` dict.

#[derive(Default, Clone)]
pub struct FrontMatter {
    pub title: String,
    pub authors: Vec<Author>,
    /// Distinct affiliation/organisation lines, in marker order (index ~ the `<sup>`
    /// marker on the authors that reference them).
    pub affiliations: Vec<String>,
    pub abstract_text: Option<String>,
    pub keywords: Vec<String>,
}

#[derive(Default, Clone)]
pub struct Author {
    pub name: String,
    /// The affiliation markers attached to the name (`"1"`, `"2"`, `"*"`, …) — drive
    /// the `<sup>` in the rendered author list.
    pub markers: Vec<String>,
    /// The resolved organisation text (multiple joined with `"; "`), if linkable.
    pub affiliation: Option<String>,
}
