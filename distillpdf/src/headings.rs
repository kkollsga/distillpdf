//! Heading classification: decide whether a line is a section heading and at what level
//! (size/number/roman/run-in cues), validate via the document style profile, plan the
//! trusted heading styles globally, and merge wrapped heading lines. Extracted from html.rs.

use crate::html::{clone_span, has_year, initials_count, looks_like_reference, numbered_level, roman_section, THEOREM_ENV, TOP_HEADS};
use crate::layout::{lines_of, Line, Run};
use crate::profile::DocProfile;
use crate::text::Span;
use lopdf::ObjectId;
use std::collections::HashMap;

pub(crate) fn size_level(ratio: f32) -> u8 {
    if ratio >= 1.5 {
        1
    } else if ratio >= 1.2 {
        2
    } else {
        3
    }
}
/// True when a text run reads as a figure's axis label: numeric-DOMINATED (most
/// whitespace tokens are short numeric ticks) — covering a lone tick (`"1000"`), a full
/// tick row (`"1000 2000 … 7000"`), and an axis title fused to its ticks
/// (`"Vp (m/s) 1000 … 7000"`) — or a lone unit-bearing axis title (`"Vp(m/s)"`, `"BSF(m)"`).
/// Rejects the multi-word `"Figure N:"` caption and ordinary prose (mostly word tokens),
/// so body-size axis labels can be pulled into a figure without swallowing prose.
pub(crate) fn is_axis_label_text(text: &str) -> bool {
    let t = text.trim();
    if t.is_empty() {
        return false;
    }
    let toks: Vec<&str> = t.split_whitespace().collect();
    let is_tick = |tok: &str| !tok.is_empty() && tok.len() <= 6 && tok.chars().all(|c| c.is_ascii_digit() || ".,-–%".contains(c));
    let nnum = toks.iter().filter(|tok| is_tick(tok)).count();
    let numeric_row = nnum >= 1 && nnum * 2 >= toks.len();
    // A short unit-bearing axis title — "Vp(m/s)", "BSF(m)", or the spaced "Vp (m/s)".
    let axis_title = toks.len() <= 2 && t.len() <= 14 && t.contains('(') && t.contains(')');
    numeric_row || axis_title
}
/// Paragraph-aware header detection. A header is a numbered section line, a
/// canonical top-level name, a standalone bold/larger line, or a **bold run-in
/// lead** at the top of a paragraph. Returns `(level, n_leading_runs)`: when
/// `n_leading_runs < line.runs.len()` the header is a run-in lead and the
/// remaining runs continue as that paragraph's body. Level comes from the
/// section number when present, else from font size.
/// SEC filing structure anchor: "PART I"–"PART VIII" (→ level 1) or "Item 1"/"Item 1A"/
/// "Item 10" (→ level 2), with or without a trailing title. Returns the heading level.
/// These are the authoritative section markers in 10-K/S-1 filings (which ship no PDF
/// outline), so they are recognised even though their styling matches non-heading lines.
pub(crate) fn sec_anchor(trimmed: &str) -> Option<u8> {
    let low = trimmed.to_lowercase();
    // What follows the anchor token must be empty, a separator, or a capitalised title —
    // never a lowercase sentence continuation ("Part I of Form W-9. If you are …"), which
    // is prose that merely mentions the part, not the heading.
    let title_ok = |rest: &str| {
        let r = rest.trim_start();
        r.is_empty() || r.starts_with([':', '.', '—', '-']) || r.chars().next().is_some_and(|c| !c.is_ascii_lowercase())
    };
    if low.starts_with("part ") {
        let after = trimmed["part ".len()..].trim_start();
        let tl = after.find(char::is_whitespace).unwrap_or(after.len());
        let tok = after[..tl].trim_end_matches([':', '.', '—', '-', ',']);
        if (1..=5).contains(&tok.len()) && tok.chars().all(|c| "IVXivx".contains(c)) && title_ok(&after[tl..]) {
            return Some(1);
        }
    }
    if low.starts_with("item ") {
        let after = trimmed["item ".len()..].trim_start();
        let tl = after.find(|c: char| c.is_whitespace() || c == '.' || c == ':').unwrap_or(after.len());
        let tok = &after[..tl];
        let digits = tok.chars().take_while(|c| c.is_ascii_digit()).count();
        let suffix: String = tok.chars().skip_while(|c| c.is_ascii_digit()).collect();
        if (1..=2).contains(&digits)
            && (suffix.is_empty() || (suffix.len() <= 2 && suffix.chars().all(|c| c.is_ascii_alphabetic())))
            && title_ok(&after[tl..])
        {
            return Some(2);
        }
    }
    None
}
/// A line that reads as a CLAUSE or value phrase, not a heading — used to reject body
/// fragments that happen to match a heading STYLE ("158 GeV at 95% confidence level have
/// been set", "600 GeV), largely due to …"). Signals: a trailing comma, an internal
/// sentence boundary (". " + capital/'('), a mid-line lowercase finite verb, or a leading
/// number+unit followed by a lowercase continuation. Only applied to multi-word lines, so
/// noun-phrase headings ("System Architecture", "Data Set") are untouched.
pub(crate) fn looks_like_clause(trimmed: &str) -> bool {
    if trimmed.ends_with(',') {
        return true;
    }
    // A line ENDING in a coordinating conjunction / preposition ("…, and", "… of",
    // "… with", "… that") is a wrapped mid-sentence fragment, never a section title.
    // A real heading ends on its last content word. (This catches a math/prose fragment
    // — "… {Φij(xj)⌞M}, and" — that gets isolated onto its own line and would otherwise
    // be promoted to a heading by the style path.)
    {
        const TRAILING_FN: &[&str] = &[
            "and", "or", "but", "with", "of", "that", "which", "the", "a", "an", "to",
            "for", "in", "on", "at", "by", "as", "from", "into", "than", "where", "while",
        ];
        let last = trimmed
            .rsplit(|c: char| c.is_whitespace())
            .find(|t| !t.is_empty())
            .unwrap_or("")
            .trim_matches(|c: char| !c.is_alphabetic())
            .to_lowercase();
        if TRAILING_FN.contains(&last.as_str()) {
            return true;
        }
    }
    // Strip a leading section-number token ("1. ", "1.2 ", "II. ", "A. ") so its
    // separator dot is not mistaken for an internal sentence boundary ("II. METHODS").
    let core = {
        let t = trimmed.trim_start();
        match t.find(char::is_whitespace) {
            Some(sp) if t[..sp].ends_with('.') => {
                let head = t[..sp].trim_end_matches('.');
                let numbery = !head.is_empty()
                    && (head.chars().all(|c| c.is_ascii_digit() || c == '.')
                        || head.chars().all(|c| "IVXLCDM".contains(c))
                        || (head.len() == 1 && head.chars().all(|c| c.is_ascii_uppercase())));
                if numbery { t[sp..].trim_start() } else { t }
            }
            _ => t,
        }
    };
    let chars: Vec<char> = core.chars().collect();
    for w in chars.windows(3) {
        if w[0] == '.' && w[1] == ' ' && (w[2].is_uppercase() || w[2] == '(') {
            return true; // internal sentence boundary
        }
    }
    let toks: Vec<&str> = trimmed.split_whitespace().collect();
    const VERBS: &[&str] = &["is", "are", "was", "were", "have", "has", "had", "been", "be", "set", "shows", "show", "due", "using", "based"];
    if toks.iter().skip(1).any(|t| {
        let w = t.trim_matches(|c: char| !c.is_alphabetic());
        !w.is_empty() && w.chars().all(|c| c.is_lowercase()) && VERBS.contains(&w)
    }) {
        return true; // mid-line lowercase finite verb
    }
    const UNITS: &[&str] = &["gev", "tev", "mev", "kev", "ev", "kg", "mhz", "ghz", "khz", "hz", "nm", "mm", "cm", "km", "ms"];
    if toks.first().is_some_and(|t| t.chars().next().is_some_and(|c| c.is_ascii_digit()))
        && toks.get(1).is_some_and(|t| UNITS.contains(&t.trim_matches(|c: char| !c.is_alphabetic()).to_lowercase().as_str()))
        && toks.iter().skip(2).any(|t| t.chars().next().is_some_and(|c| c.is_lowercase()))
    {
        return true; // leading value+unit with a lowercase continuation
    }
    false
}

/// How confidently a line was judged a heading (see `header_at` for how `Style` verdicts
/// are gated on positional isolation).
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum HeadingKind {
    Reliable,
    Style,
    RunIn,
}
pub(crate) fn detect_header(line: &Line, body: f32, profile: Option<&DocProfile>) -> Option<(u8, usize, HeadingKind)> {
    let txt = line.text();
    let trimmed = txt.trim();
    if trimmed.is_empty() {
        return None;
    }
    let words = trimmed.split_whitespace().count();
    let ratio = line.size / body.max(1.0);
    let nonspace: Vec<&Run> = line.runs.iter().filter(|r| !r.text.trim().is_empty()).collect();
    if nonspace.is_empty() {
        return None;
    }
    let all_bold = nonspace.iter().all(|r| r.bold);
    let first_bold = nonspace[0].bold;
    let low = trimmed.to_lowercase();

    // Not headers: pure-number/symbol lines (bold table cells like "86.6 86.3"),
    // the arXiv sidebar id, running page heads / reference-list entries (author
    // initials "J. Cui, H. Yan", a publication year, or an "et al." author list).
    // An author/collaboration byline ("The ATLAS Collaboration", "LIGO Scientific
    // Consortium") is set like a heading but is not one: a ≥2-word line ending in a
    // collaboration marker.
    let collab_byline = (2..=6).contains(&words)
        && matches!(
            trimmed.split_whitespace().last().unwrap_or("").trim_end_matches(['.', ',', ':']).to_lowercase().as_str(),
            "collaboration" | "collaborations" | "consortium"
        );
    if trimmed.chars().filter(|c| c.is_alphabetic()).count() < 2
        || low.contains("et al")
        || low.starts_with("arxiv")
        || has_year(trimmed)
        || initials_count(trimmed) >= 2
        || collab_byline
    {
        return None;
    }

    // Mis-extracted display MATH promoted to a heading. A heading is real words; an
    // equation fragment is symbols, single letters and digits. `real_words` = tokens
    // with ≥2 ASCII letters.
    //   • ONE real word: reject only on a relation ("T ∆² ≤ Õ(…)") — the original guard.
    //   • NO real words: reject when dominated by math symbols, single-char tokens or
    //     digits ("K ρ c", "∂ t ∂ z ∂ z", "⎢ ⎝350⎠ d ⎥ s"). Gated on real_words==0 so a
    //     numbered heading ("5 Conclusions", real_words=1) is never caught here.
    let real_words = trimmed.split_whitespace().filter(|w| w.chars().filter(|c| c.is_ascii_alphabetic()).count() >= 2).count();
    let relational = trimmed.chars().any(|c| matches!(c, '=' | '≤' | '≥' | '≡' | '≈' | '←' | '→' | '∝' | '≪' | '≫' | '∑' | '∈'));
    // Math operators / fences / Greek (NOT the ascii hyphen, which is a word joiner).
    let math = trimmed.chars().filter(|&c| {
        "=≤≥≡≈≠<>+←→↔⇒↦∝≪≫∑∏∫∂∇√∈∉±×÷·∞⎢⎝⎠⎥⎜⎟|∆−".contains(c)
            || "αβγδεζηθικλμνξπρςστυϕφχψωΓΔΘΛΞΠΣΦΨΩ".contains(c)
    }).count();
    if real_words == 0 {
        // No real word: an equation fragment is symbols, single letters or digits
        // ("K ρ c", "∂ t ∂ z ∂ z", "⎢ ⎝350⎠ d ⎥ s"). (A numbered heading "5 Conclusions"
        // is real_words=1, never reaching here.)
        let toks: Vec<&str> = trimmed.split_whitespace().collect();
        let ntok = toks.len().max(1);
        let single = toks.iter().filter(|t| t.trim_matches(|c: char| !c.is_alphanumeric()).chars().count() <= 1).count();
        let nonspace = trimmed.chars().filter(|c| !c.is_whitespace()).count().max(1);
        let digits = trimmed.chars().filter(|c| c.is_ascii_digit()).count();
        if relational || math >= 1 || single * 2 >= ntok || digits as f32 > nonspace as f32 * 0.4 {
            return None;
        }
    } else if real_words == 1 && relational {
        return None;
    }
    // Symbol-soup equation even WITH a few (often mis-extracted) word-like tokens
    // ("K ρ zT ,ρ= ρ= K − ρρ −+ρ BTe"): math operators dominate over real words. The
    // `>= 3` floor and `>= real_words + 2` margin spare genuine headings that carry a
    // symbol or two ("H → ZZ → 4ℓ channel", "β-decay Measurements").
    if math >= 3 && math >= real_words + 2 {
        return None;
    }

    // Theorem-environment labels ("Theorem 1.1.", "Lemma 2.3", "Definition",
    // "Proof.", "Remark 2.4") are bold leads, not section headers — exclude them.
    let first_word: String = trimmed.split(|c: char| !c.is_alphabetic()).next().unwrap_or("").to_lowercase();
    if THEOREM_ENV.contains(&first_word.as_str()) {
        let rest = trimmed[first_word.len()..].trim_start();
        if rest.is_empty()
            || rest.starts_with(|c: char| c.is_ascii_digit())
            || trimmed.ends_with(['.', ':'])
        {
            return None;
        }
    }

    // 0. SEC filing structure anchors ("PART I", "Item 1A. Risk Factors"). These carry
    //    the real section structure of a 10-K/S-1 that has no PDF outline and whose tier
    //    styling is shared with hundreds of non-heading lines (so the global distrust pass
    //    would otherwise drop them). Recognising them as Reliable keeps them at the right
    //    level and exempts them from the style-frequency distrust.
    if words <= 16 {
        if let Some(lvl) = sec_anchor(trimmed) {
            return Some((lvl, line.runs.len(), HeadingKind::Reliable));
        }
    }
    // 1. Numbered section header — level from numbering depth. The "N.N Title"
    //    shape (number, space, capitalised title with a real word) is the signal;
    //    no bold needed (many heading faces aren't flagged bold). This also rejects
    //    inline math like "1 N".
    if words <= 12 && !looks_like_reference(trimmed) {
        if let Some(lvl) = numbered_level(trimmed).or_else(|| roman_section(trimmed)) {
            let title = trimmed.split_once(char::is_whitespace).map(|x| x.1).unwrap_or("");
            let title_upper = title.chars().find(|c| c.is_alphabetic()).is_some_and(|c| c.is_uppercase());
            let alpha = title.chars().filter(|c| c.is_alphabetic()).count();
            // Footnotes ("2 We do not show halo … because …") carry a leading number
            // like a section, but are set SMALLER than body text; a real numbered
            // heading is body-size or larger. Reject sub-body-size numbered lines.
            // Also reject a measured VALUE mis-read as a section number ("158 GeV at 95%
            // confidence level …", "7 TeV at the LHC …"): the leading "number" is a
            // measurement whose next token is a unit, not a section title. (Such a line,
            // if it is heading-styled at all, is then caught downstream by the style
            // path's clause guard; otherwise it stays body text.)
            let value_unit = {
                let toks: Vec<&str> = trimmed.split_whitespace().collect();
                const UNITS: &[&str] = &["gev", "tev", "mev", "kev", "ev", "kg", "mhz", "ghz", "khz", "hz", "nm", "mm", "cm", "km", "ms"];
                toks.get(1).is_some_and(|t| UNITS.contains(&t.trim_matches(|c: char| !c.is_alphabetic()).to_lowercase().as_str()))
            };
            if title_upper && alpha >= 3 && ratio >= 0.92 && !value_unit {
                // "N. **Bold lead.** body" (a labelled limitation/claim list item):
                // keep only the bold run-in lead as the heading and let the body
                // split off — the whole sentence is not the heading.
                if let Some(fb) = line.runs.iter().position(|r| !r.text.trim().is_empty() && r.bold) {
                    let mut e = fb;
                    while e < line.runs.len() && (line.runs[e].text.trim().is_empty() || line.runs[e].bold) {
                        e += 1;
                    }
                    let lead: String = line.runs[..e].iter().map(|r| r.text.as_str()).collect();
                    if e < line.runs.len() && matches!(lead.trim_end().chars().next_back(), Some('.') | Some(':')) {
                        return Some((lvl, e, HeadingKind::Reliable));
                    }
                }
                // A wrapped prose line that a DECIMAL VALUE merely opened is not a
                // numbered section: "0.3 F1 behind fine-tuning the entire model. This …"
                // (wrapping after "… which is only"). Two signals together, both
                // required so real section titles stay headings: the leading number is
                // "0.x" (a value — section numbers start at 1, never 0), AND the title
                // reads as TWO sentences (an internal ". " + capital). Real numbered
                // sections ("2.1 …", "3.3 …") start ≥1 so are untouched.
                let internal_sentence = title.split(". ").nth(1).and_then(|r| r.chars().find(|c| c.is_alphabetic())).is_some_and(|c| c.is_uppercase());
                if trimmed.starts_with('0') && internal_sentence {
                    return None;
                }
                return Some((lvl, line.runs.len(), HeadingKind::Reliable));
            }
        }
    }
    // 2. Canonical unnumbered top-level header. A line that is *exactly* a
    //    canonical name (≤2 words, trailing '.'/':'/'—' allowed) is unambiguously
    //    a heading even at body size & non-bold (common in math/physics styles).
    let canon = low.trim_end_matches(['.', ':', '—', '-', ' ']);
    if trimmed.split_whitespace().count() <= 2 && TOP_HEADS.contains(&canon) {
        return Some((1, line.runs.len(), HeadingKind::Reliable));
    }
    // "Appendix A", "Appendix A. Title", "Appendix B: ..." → section level. But not
    // a sentence that merely opens with the word ("Appendix B shows that …") nor an
    // inline cross-reference ("…see Appendix A), β = …"): require a clean appendix id
    // followed by end / separator / a capitalised title, never a lowercase
    // continuation or a stray bracket.
    if low.starts_with("appendix ") && words <= 16 {
        let rest = trimmed["appendix ".len()..].trim_start();
        let id_end = rest.find(char::is_whitespace).unwrap_or(rest.len());
        let id = &rest[..id_end];
        let after = rest[id_end..].trim_start();
        let clean_id = !id.is_empty() && id.chars().all(|c| c.is_alphanumeric() || matches!(c, '.' | ':' | '-'));
        let title_after = after.is_empty()
            || after.starts_with([':', '.', '—'])
            || after.chars().next().is_some_and(|c| c.is_uppercase());
        if clean_id && title_after {
            // A long sentence after the id ("Appendix E. The resulting … are arranged
            // …") is body text merged onto the appendix label — keep "Appendix <id>"
            // as the heading and split the sentence off as a paragraph.
            let after_sentence = after.split_whitespace().count() > 6
                && (after.contains(',')
                    || after.split_whitespace().any(|w| matches!(w, "is" | "are" | "was" | "were")));
            if after_sentence {
                let prefix_len = "appendix ".len() + (trimmed["appendix ".len()..].len() - rest.len()) + id_end;
                let target = (txt.len() - txt.trim_start().len()) + prefix_len;
                let mut acc = 0usize;
                for (ri, r) in line.runs.iter().enumerate() {
                    acc += r.text.len();
                    if acc >= target {
                        return Some((1, ri + 1, HeadingKind::Reliable));
                    }
                }
            }
            return Some((1, line.runs.len(), HeadingKind::Reliable));
        }
    }
    // 2.5 Profiled heading tier: a short, capitalised line whose size·weight·font matches
    //     a heading style the document analysis identified — catches faces only slightly
    //     larger than body (or body-size in a distinct font) that the fixed 1.18× ratio
    //     below would miss. A trailing ':' is allowed (a labelled section head); '.'/','/';'
    //     are not. Numbered/canonical paths above still win first.
    if let Some(prof) = profile {
        if words <= 16
            && !trimmed.ends_with(['.', ',', ';'])
            && !looks_like_reference(trimmed)
            && !(real_words >= 2 && looks_like_clause(trimmed))
            && trimmed.chars().next().is_some_and(|c| !c.is_lowercase())
        {
            if let Some(level) = prof.heading_level(line.size, line.font, all_bold) {
                return Some((level, line.runs.len(), HeadingKind::Style));
            }
        }
    }
    // 3. Standalone bold (or visibly larger) short line.
    if words <= 12
        && (all_bold || ratio >= 1.18)
        && !trimmed.ends_with(['.', ',', ';', ':'])
        && !looks_like_reference(trimmed)
        && !(real_words >= 2 && looks_like_clause(trimmed))
        && trimmed.chars().next().is_some_and(|c| !c.is_lowercase())
    {
        return Some((size_level(ratio), line.runs.len(), HeadingKind::Style));
    }
    // 4. Bold run-in lead: short bold prefix, then non-bold body on the same line
    //    ("Encoder: The encoder …", "Task #1: Masked LM Before feeding …").
    if first_bold && !all_bold {
        let mut k = 0usize;
        while k < line.runs.len() {
            if line.runs[k].text.trim().is_empty() {
                k += 1;
                continue;
            }
            if line.runs[k].bold {
                k += 1;
            } else {
                break;
            }
        }
        let lead: String = line.runs[..k].iter().map(|r| r.text.as_str()).collect();
        let lead = lead.trim();
        let lead_words = lead.split_whitespace().count();
        // A real run-in head is a phrase (>=2 words) or a labelled lead ending in
        // ':'. This rejects drop-cap initials ("B", "T") and inline bold tokens
        // ("BERTBASE") that aren't section heads.
        // A real run-in head is a phrase (>=2 words) or a single labelled word
        // ending ':' or '.' ("Benchmarks.", "Discussions."). Rejects drop-cap
        // initials ("B.") and inline bold tokens ("BERTBASE").
        let labelled = lead.ends_with(':') || lead.ends_with('.');
        // A run-in head starts capitalised; a lowercase lead is a wrapped continuation
        // of an inline bold phrase ("…de Evidență / a Procurilor și a Revocărilor…"),
        // not a heading.
        let cap_lead = lead.chars().next().is_some_and(|c| !c.is_lowercase());
        // A lead ending in a comma is a continued enumeration — a bold author name at
        // the head of an author list ("**Chris Dart,** Anne-Lise Lysholm, …"), not a
        // section head. Headings never end in a comma.
        if cap_lead
            && !lead.ends_with(',')
            && ((2..=8).contains(&lead_words)
                || (labelled && lead.chars().filter(|c| c.is_alphabetic()).count() >= 2))
        {
            return Some((3, k, HeadingKind::RunIn));
        }
    }
    None
}
/// The gated heading decision used at the prose/list emission sites. A `Style` verdict
/// from `detect_header` is dropped when its style signature was DISTRUSTED by the global
/// pre-detection pass (an over-used emphasis/label style — see plan_headings).
/// `Reliable` / `RunIn` verdicts (numbered, canonical, SEC `Item`/`PART`, run-ins) bypass
/// the distrust, so real structure survives even in a filing whose headings share styling
/// with hundreds of line items.
pub(crate) fn header_at(lines: &[&Line], i: usize, body: f32, profile: &DocProfile, plan: &HeadingPlan) -> Option<(u8, usize)> {
    let (lvl, k, kind) = detect_header(lines[i], body, Some(profile))?;
    match kind {
        HeadingKind::Reliable | HeadingKind::RunIn => Some((lvl, k)),
        HeadingKind::Style => {
            if plan.distrusted.contains(&style_sig(lines[i])) {
                None
            } else {
                Some((lvl, k))
            }
        }
    }
}
/// True when a whole line is set in a heading style the document profile identified.
pub(crate) fn line_in_heading_tier(l: &Line, profile: &DocProfile) -> bool {
    let all_bold = !l.runs.is_empty() && l.runs.iter().all(|r| r.text.trim().is_empty() || r.bold);
    profile.heading_level(l.size, l.font, all_bold).is_some()
}
/// Fold a heading that wraps across lines into one `Line`. A two-line section title (same
/// heading tier, vertically adjacent, same indent — the second line is the continuation,
/// often lowercase or following a first line that ends ':') becomes a single heading so it
/// is emitted as one `<hN>` rather than a heading plus an orphaned body paragraph. Only
/// heading-tier lines are merged, so body text and separated headings are never joined.
pub(crate) fn merge_heading_continuations(lines: &mut Vec<Line>, profile: &DocProfile) {
    if profile.tiers.is_empty() {
        return;
    }
    let words = |l: &Line| l.text().split_whitespace().count();
    let mut i = 0;
    while i + 1 < lines.len() {
        if line_in_heading_tier(&lines[i], profile) && words(&lines[i]) <= 16 {
            while i + 1 < lines.len() {
                let a = &lines[i];
                let b = &lines[i + 1];
                let same_tier = a.size.round() == b.size.round() && a.font == b.font;
                let adjacent = (a.y - b.y).abs() < a.size * 1.8;
                let same_indent = (b.x0 - a.x0).abs() < a.size * 1.5;
                let next_pagenum = b.text().trim().chars().last().is_some_and(|c| c.is_ascii_digit());
                let combined = words(a) + words(b) <= 16;
                if same_tier && adjacent && same_indent && combined && !next_pagenum && line_in_heading_tier(b, profile) {
                    let nxt = lines.remove(i + 1);
                    if let Some(last) = lines[i].runs.last_mut() {
                        if !last.text.ends_with(' ') {
                            last.text.push(' ');
                        }
                    }
                    lines[i].x1 = lines[i].x1.max(nxt.x1);
                    lines[i].tot_w += nxt.tot_w;
                    lines[i].runs.extend(nxt.runs);
                } else {
                    break;
                }
            }
        }
        i += 1;
    }
}
/// The style signature of a line: rounded size, all-bold, font id — the key the heading
/// tier and the distrust pass share.
pub(crate) fn style_sig(line: &Line) -> (i32, bool, u32) {
    let nonspace: Vec<&Run> = line.runs.iter().filter(|r| !r.text.trim().is_empty()).collect();
    let all_bold = !nonspace.is_empty() && nonspace.iter().all(|r| r.bold);
    (line.size.round() as i32, all_bold, line.font)
}

/// Document-global heading model (distrusted over-firing style signatures); built once
/// before the parallel render, read-only and integer-keyed → deterministic.
#[derive(Default, Clone)]
pub(crate) struct HeadingPlan {
    distrusted: std::collections::HashSet<(i32, bool, u32)>,
}
/// PASS over the whole document's lines: count, per style signature, how many lines yield
/// a surviving `Style` heading verdict, then distrust a signature whose count is far above
/// what real section headings plausibly reach. A genuine document spreads headings across
/// a few tiers with at most a few dozen of any one; an emphasis/label style fires on
/// hundreds. The threshold is an ABSOLUTE floor (not a fraction of doc length — a long
/// filing's flood is still only a few percent of its lines) raised mildly for very long
/// docs. arXiv papers (≤~50 headings, mostly numbered=Reliable) never trip it → no-op.
pub(crate) fn plan_headings(page_spans: &[(u32, ObjectId, Vec<Span>)], body: f32, profile: &DocProfile) -> HeadingPlan {
    let mut counts: HashMap<(i32, bool, u32), usize> = HashMap::new();
    let mut samples: HashMap<(i32, bool, u32), Vec<String>> = HashMap::new();
    let debug = std::env::var("DPDF_HEAD_PROFILE").is_ok();
    let mut doc_lines = 0usize;
    let mut pages = 0usize;
    for (_pno, _id, spans) in page_spans {
        pages += 1;
        let lines = lines_of(spans.iter().map(clone_span).collect(), &[]);
        for ln in &lines {
            doc_lines += 1;
            if let Some((_, _, HeadingKind::Style)) = detect_header(ln, body, Some(profile)) {
                let sig = style_sig(ln);
                *counts.entry(sig).or_insert(0) += 1;
                if debug {
                    let s = samples.entry(sig).or_default();
                    if s.len() < 4 {
                        s.push(ln.text().trim().chars().take(40).collect());
                    }
                }
            }
        }
    }
    let thresh = 90usize.max(pages); // absolute floor 90; ~1 per page for very long docs
    let distrusted: std::collections::HashSet<(i32, bool, u32)> =
        counts.iter().filter(|(_, &c)| c > thresh).map(|(&k, _)| k).collect();
    if debug {
        let mut v: Vec<_> = counts.iter().collect();
        v.sort_by(|a, b| b.1.cmp(a.1));
        eprintln!("[head-profile] doc_lines={doc_lines} pages={pages} thresh={thresh} style-sigs:");
        for (sig, c) in v.iter().take(10) {
            let dis = if **c > thresh { " DISTRUST" } else { "" };
            eprintln!("  {:?} x{}{}  e.g. {:?}", sig, c, dis, samples.get(*sig).map(|s| s.as_slice()).unwrap_or(&[]));
        }
    }
    HeadingPlan { distrusted }
}
