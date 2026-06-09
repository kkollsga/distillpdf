//! Pure DocTags synthesis from Tesseract line geometry. Kept out of the feature gate so it
//! is always compiled and unit-tested (no C libraries needed). The `tesseract` engine
//! (`tesseract.rs`) feeds it the recognized lines; everything downstream is the normal
//! bare-dialect DocTags pipeline.

/// One recognized text line: its text, pixel bounding box (top-left origin), and the
/// engine's mean confidence (0–100).
pub(crate) struct OcrLine {
    pub text: String,
    pub x1: i32,
    pub y1: i32,
    pub x2: i32,
    pub y2: i32,
    pub conf: f32,
}

/// Minimum line confidence to keep — drops Tesseract's low-confidence noise.
const MIN_CONF: f32 = 30.0;

/// Map line boxes (image-pixel space, `w`×`h`) into **bare-dialect DocTags** — one
/// `<loc_x1><loc_y1><loc_x2><loc_y2>TEXT` line per recognized line, loc ints normalized to
/// 0–500 (the parser's `LOC_DIV`). Lines are emitted top-to-bottom, left-to-right so the
/// renderer (and its column logic) sees a sensible reading order. The bare dialect lets the
/// parser's ALL-CAPS/numbered heading heuristic recover headings for free.
pub(crate) fn lines_to_doctags(mut lines: Vec<OcrLine>, w: i32, h: i32) -> String {
    let norm = |v: i32, dim: i32| {
        ((v as f32 / dim.max(1) as f32) * 500.0).round().clamp(0.0, 500.0) as i32
    };
    lines.sort_by(|a, b| (a.y1, a.x1).cmp(&(b.y1, b.x1)));
    let mut out = String::new();
    for ln in &lines {
        let t = ln.text.trim();
        if t.is_empty() || ln.conf < MIN_CONF {
            continue;
        }
        out.push_str(&format!(
            "<loc_{}><loc_{}><loc_{}><loc_{}>{}\n",
            norm(ln.x1, w),
            norm(ln.y1, h),
            norm(ln.x2, w),
            norm(ln.y2, h),
            t
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(text: &str, x1: i32, y1: i32, x2: i32, y2: i32, conf: f32) -> OcrLine {
        OcrLine { text: text.into(), x1, y1, x2, y2, conf }
    }

    #[test]
    fn normalizes_to_loc_500_and_keeps_text() {
        // a 1000x2000 page; a line at (100,200)-(900,260)
        let dt = lines_to_doctags(vec![line("Hello world", 100, 200, 900, 260, 90.0)], 1000, 2000);
        // x: 100/1000*500=50, 900/1000*500=450 ; y: 200/2000*500=50, 260/2000*500=65
        assert_eq!(dt, "<loc_50><loc_50><loc_450><loc_65>Hello world\n");
    }

    #[test]
    fn sorts_top_to_bottom_then_left() {
        let dt = lines_to_doctags(
            vec![
                line("second", 0, 100, 50, 110, 90.0),
                line("first", 0, 10, 50, 20, 90.0),
                line("left", 0, 100, 50, 110, 90.0), // same y as "second", earlier x tie kept stable
            ],
            500,
            500,
        );
        let order: Vec<&str> = dt.lines().map(|l| l.rsplit('>').next().unwrap()).collect();
        assert_eq!(order[0], "first");
    }

    #[test]
    fn drops_empty_and_low_confidence() {
        let dt = lines_to_doctags(
            vec![
                line("good", 0, 0, 10, 10, 80.0),
                line("   ", 0, 20, 10, 30, 95.0),     // whitespace-only
                line("noise", 0, 40, 10, 50, 12.0),   // below MIN_CONF
            ],
            100,
            100,
        );
        assert!(dt.contains("good"));
        assert!(!dt.contains("noise"));
        assert_eq!(dt.lines().count(), 1);
    }

    #[test]
    fn clamps_out_of_range_coords() {
        let dt = lines_to_doctags(vec![line("x", -10, -10, 2000, 2000, 90.0)], 1000, 1000);
        assert!(dt.starts_with("<loc_0><loc_0><loc_500><loc_500>"));
    }
}
