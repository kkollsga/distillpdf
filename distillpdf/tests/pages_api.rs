//! The page-at-a-time API against generated PDFs and the repo's own fixtures.
//!
//! Every test names the failure it catches. Corpus-scale parity lives in
//! `scripts/page_parity.py`; these stay self-contained so CI can run them.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use distillpdf::markdown::{self, ImgMode};
use distillpdf::model::BlockKind;
use distillpdf::pages::{CancelToken, DocStats, PageOutput, ScanPhase};
use distillpdf::{Engine, Error, Mode, PdfDocument};
use lopdf::content::{Content, Operation};
use lopdf::{dictionary, Document, Object, Stream};

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../tests/fixtures_pdf")
}

/// A document with one page per entry, each drawing its lines top to bottom in 10 pt Helvetica.
fn pdf_of(pages: &[&[&str]]) -> Vec<u8> {
    let mut doc = Document::with_version("1.7");
    let font = doc.add_object(dictionary! {
        "Type" => "Font", "Subtype" => "Type1", "BaseFont" => "Helvetica",
    });
    let parent = doc.new_object_id();
    let mut kids: Vec<Object> = Vec::new();
    for lines in pages {
        let mut ops = vec![
            Operation::new("BT", vec![]),
            Operation::new("Tf", vec!["F1".into(), 10.into()]),
            Operation::new("Td", vec![72.into(), 720.into()]),
            Operation::new("TL", vec![13.into()]),
        ];
        for line in *lines {
            ops.push(Operation::new("Tj", vec![Object::string_literal(*line)]));
            ops.push(Operation::new("T*", vec![]));
        }
        ops.push(Operation::new("ET", vec![]));
        let content = doc.add_object(Stream::new(dictionary! {}, Content { operations: ops }.encode().unwrap()));
        let page = doc.add_object(dictionary! {
            "Type" => "Page",
            "Parent" => parent,
            "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
            "Contents" => content,
            "Resources" => dictionary! { "Font" => dictionary! { "F1" => font } },
        });
        kids.push(page.into());
    }
    let count = kids.len() as i64;
    doc.objects.insert(parent, Object::Dictionary(dictionary! { "Type" => "Pages", "Kids" => kids, "Count" => count }));
    let catalog = doc.add_object(dictionary! { "Type" => "Catalog", "Pages" => parent });
    doc.trailer.set("Root", catalog);
    let mut bytes = Vec::new();
    doc.save_to(&mut bytes).unwrap();
    bytes
}

fn open(bytes: &[u8]) -> PdfDocument {
    PdfDocument::from_bytes_with_engine(bytes, Engine::Lazy).expect("generated PDF opens")
}

fn all_pages(doc: &PdfDocument) -> Vec<PageOutput> {
    let stats = doc.scan_document(None, &mut |_| {}).expect("scan");
    doc.page_stream(stats).expect("stream").collect::<Result<Vec<_>, _>>().expect("pages")
}

/// A figure's anchor is page-scoped in the page path and document-numbered in the whole-document
/// path, one of the differences `pages.rs` names, so word comparisons leave anchors out.
fn without_anchors(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("(#") {
        out.push_str(&rest[..start]);
        match rest[start..].find(')') {
            Some(end) => rest = &rest[start + end + 1..],
            None => {
                rest = "";
            }
        }
    }
    out.push_str(rest);
    out
}

fn sorted_words(text: &str) -> Vec<String> {
    let text = without_anchors(text);
    let mut words: Vec<String> = text
        .split(|c: char| !c.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .map(str::to_lowercase)
        .collect();
    words.sort();
    words
}

fn strip_markers(markdown: &str) -> String {
    markdown
        .lines()
        .filter(|line| !line.starts_with("[continues on PDF p.") && !line.starts_with("[continued from PDF p."))
        .collect::<Vec<_>>()
        .join("\n")
}

const PROSE_ENDING_MID_SENTENCE: &str = "The contractor shall deliver the quarterly report before the";
const PROSE_OPENING_MID_SENTENCE: &str = "end of each period and shall retain copies for seven years.";

/// Catches page output depending on the order pages were asked for, which is what would make a
/// resumed or randomly accessed page disagree with a sequential one.
#[test]
fn a_page_is_the_same_however_it_is_reached() {
    let bytes = pdf_of(&[
        &["Cover page text that stands alone here."],
        &["First paragraph of this page ends here.", "", PROSE_ENDING_MID_SENTENCE],
        &[PROSE_OPENING_MID_SENTENCE, "", "A new paragraph on the next page ends here."],
        &["Last page stands alone."],
    ]);
    let doc = open(&bytes);
    let in_order = all_pages(&doc);
    let stats = doc.scan_document(None, &mut |_| {}).unwrap();
    let mut stream = doc.page_stream(stats).unwrap();
    for n in [3u32, 1, 4, 2, 2, 3, 1] {
        let page = stream.page(n).unwrap();
        let reference = &in_order[n as usize - 1];
        assert_eq!(page.markdown, reference.markdown, "page {n}");
        assert_eq!(page.blocks.len(), reference.blocks.len(), "page {n}");
    }
}

/// Catches a paragraph split by a page break staying split, so that a phrase across the break
/// cannot be found, and catches its text being lost or doubled when it moves.
#[test]
fn a_paragraph_crossing_a_page_goes_where_it_starts_with_a_marker() {
    let bytes = pdf_of(&[
        &["Cover page text that stands alone here."],
        &["Opening sentence of the page ends here.", "", PROSE_ENDING_MID_SENTENCE],
        &[PROSE_OPENING_MID_SENTENCE, "", "A new paragraph on the third page ends here."],
    ]);
    let doc = open(&bytes);
    let pages = all_pages(&doc);
    assert!(pages[1].markdown.contains("before the end of each period"), "joined: {}", pages[1].markdown);
    assert!(pages[1].markdown.contains("[continues on PDF p. 3]"));
    assert!(pages[2].markdown.starts_with("[continued from PDF p. 2]"));
    assert!(!pages[2].markdown.contains("end of each period"), "the moved text must not appear twice");
    assert_eq!(pages[1].continues_on, Some(3));
    assert_eq!(pages[2].continued_from, Some(2));
    let moved = pages[1].continuations.first().expect("the block says where it goes on");
    assert_eq!(moved.page, 3);
    assert!(moved.bbox.is_some(), "the agent can highlight the tail on its own page");
    let concatenated: String = pages.iter().map(|page| strip_markers(&page.markdown) + "\n").collect();
    let html = doc.render(Mode::Page, false, false);
    let (whole, _) = markdown::html_to_markdown(&html, false, ImgMode::Placeholder);
    assert_eq!(sorted_words(&concatenated), sorted_words(&whole), "no word lost or invented");
}

/// Catches two paragraphs being glued because one lacks a full stop, when the next page opens a
/// new sentence. A false join is worse than a missed one.
#[test]
fn a_page_opening_a_new_sentence_is_not_joined() {
    let bytes = pdf_of(&[
        &["Cover page text that stands alone here."],
        &["Opening sentence of the page ends here.", "", "The closing line of this page has no stop"],
        &["Capitalised sentence on the next page.", "", "Another paragraph follows it here."],
    ]);
    let pages = all_pages(&open(&bytes));
    assert_eq!(pages[1].continues_on, None);
    assert_eq!(pages[2].continued_from, None);
    assert!(pages[2].markdown.contains("Capitalised sentence"));
}

/// Catches a stray line of display math being read as the tail of a sentence, because math opens
/// lowercase too. It would move off the page it is printed on.
#[test]
fn display_math_at_a_page_top_stays_on_its_page() {
    let bytes = pdf_of(&[
        &["Cover page text that stands alone here."],
        &["This paragraph has no closing punctuation before the"],
        &["n n", "", "A paragraph after the equation on this page ends here."],
    ]);
    let pages = all_pages(&open(&bytes));
    assert_eq!(pages[1].continues_on, None, "{}", pages[1].markdown);
    assert_eq!(pages[2].continued_from, None);
}

/// Catches a text-free page being treated as an error or losing its page number.
#[test]
fn an_empty_page_is_a_page() {
    let bytes = pdf_of(&[&["Some text here on the first page."], &[], &["More text here on the last page."]]);
    let pages = all_pages(&open(&bytes));
    assert_eq!(pages.len(), 3);
    assert_eq!(pages[1].page, 2);
    assert!(pages[1].blocks.is_empty());
    assert!(pages[1].markdown.is_empty());
    assert!(pages[1].width_pts > 0.0);
}

/// Catches a page number outside the document panicking or indexing out of bounds.
#[test]
fn a_page_outside_the_document_is_an_error() {
    let doc = open(&pdf_of(&[&["Only page."]]));
    let stats = doc.scan_document(None, &mut |_| {}).unwrap();
    let mut stream = doc.page_stream(stats).unwrap();
    assert!(matches!(stream.page(0), Err(Error::NoPage(Some(0)))));
    assert!(matches!(stream.page(2), Err(Error::NoPage(Some(2)))));
}

/// Catches blocks losing the page and position an agent cites and highlights by, and catches the
/// Markdown map pointing at the wrong bytes.
#[test]
fn blocks_carry_position_and_map_to_their_markdown() {
    let doc = open(&pdf_of(&[&["A paragraph with some words in it.", "", "Another paragraph after that one."]]));
    let page = &all_pages(&doc)[0];
    assert!(!page.blocks.is_empty());
    for block in &page.blocks {
        assert_eq!(block.page, 1);
        assert!(block.id.starts_with("p1-b"), "{}", block.id);
        assert!(block.bbox.is_some(), "{:?}", block.kind);
    }
    assert!(!page.spans.is_empty());
    for span in &page.spans {
        let markdown = &page.markdown[span.markdown.clone()];
        for block in &page.blocks[span.blocks.clone()] {
            if matches!(block.kind, BlockKind::Para) {
                assert!(markdown.contains(block.text.trim()), "{markdown:?} / {:?}", block.text);
            }
        }
    }
}

/// Catches a cancel being ignored by the scan, which is the longest thing the API does.
#[test]
fn cancelling_stops_the_scan_between_pages() {
    let lines: Vec<&[&str]> = vec![&["A line of text on a page."]; 50];
    let doc = open(&pdf_of(&lines));
    let cancel = CancelToken::new();
    let trigger = cancel.clone();
    let seen = AtomicU32::new(0);
    let result = doc.scan_document(Some(&cancel), &mut |progress| {
        if seen.fetch_add(1, Ordering::Relaxed) == 4 {
            trigger.cancel();
        }
        assert_eq!(progress.total, 50);
    });
    assert!(matches!(result, Err(Error::Cancelled)));
    assert_eq!(seen.load(Ordering::Relaxed), 5, "stopped at the next page");
}

/// Catches an iterator that reads as finished after a cancel and a new token: a host looping with
/// `for page in stream` would take the pages left for the end of the document.
#[test]
fn a_new_token_lets_the_iterator_carry_on_after_a_cancel() {
    let lines: Vec<&[&str]> = vec![&["A line of text on a page that ends here."]; 6];
    let doc = open(&pdf_of(&lines));
    let reference = all_pages(&doc);
    let stats = doc.scan_document(None, &mut |_| {}).unwrap();
    let cancel = CancelToken::new();
    let mut stream = doc.page_stream(stats).unwrap().with_cancel(cancel.clone());
    assert_eq!(stream.next().unwrap().unwrap().page, 1);
    cancel.cancel();
    assert!(matches!(stream.next(), Some(Err(Error::Cancelled))));
    assert!(stream.next().is_none(), "with no new token it stays ended");
    let mut stream = stream.with_cancel(CancelToken::new());
    for n in 2..=6 {
        let page = stream.next().expect("pages are left").unwrap();
        assert_eq!(page.page, n);
        assert_eq!(page.markdown, reference[n as usize - 1].markdown);
    }
    assert!(stream.next().is_none());
}

/// Catches a scan that reports no progress, or reports it out of range: a host's progress bar
/// must reach the total in each phase.
#[test]
fn the_scan_reports_every_page_in_each_phase() {
    let lines: Vec<&[&str]> = vec![&["A line of text on a page."]; 6];
    let doc = open(&pdf_of(&lines));
    let mut typography = Vec::new();
    let mut headings = Vec::new();
    doc.scan_document(None, &mut |progress| match progress.phase {
        ScanPhase::Typography => typography.push(progress.done),
        ScanPhase::Headings => headings.push(progress.done),
    })
    .unwrap();
    assert_eq!(typography, vec![1, 2, 3, 4, 5, 6]);
    assert_eq!(headings, vec![1, 2, 3, 4, 5, 6]);
}

/// Catches a killed or cancelled stream poisoning what the caller already holds, and a resumed
/// stream disagreeing with an uninterrupted one. This is the in-process half of "a killed process
/// loses nothing the caller has taken"; the half with a real kill is `pages_memory.rs`.
#[test]
fn pages_taken_before_a_cancel_stay_good_and_a_new_stream_resumes() {
    let lines: Vec<&[&str]> = vec![&["A line of text on a page that ends here."]; 8];
    let doc = open(&pdf_of(&lines));
    let reference = all_pages(&doc);
    let stats = doc.scan_document(None, &mut |_| {}).unwrap();
    let saved = stats.to_json().unwrap();
    let cancel = CancelToken::new();
    let mut stream = doc.page_stream(stats).unwrap().with_cancel(cancel.clone());
    let mut taken = Vec::new();
    for page in stream.by_ref().take(3) {
        taken.push(page.unwrap());
    }
    cancel.cancel();
    assert!(matches!(stream.next(), Some(Err(Error::Cancelled))));
    drop(stream);
    for page in &taken {
        assert_eq!(page.markdown, reference[page.page as usize - 1].markdown);
    }
    let stats = DocStats::from_json(&saved).unwrap();
    let mut resumed = doc.page_stream(stats).unwrap();
    for n in 4..=8 {
        assert_eq!(resumed.page(n).unwrap().markdown, reference[n as usize - 1].markdown);
    }
}

/// A page heavy enough that walking it takes much longer than checking a token.
fn heavy_page(ops: usize) -> Vec<u8> {
    let mut doc = Document::with_version("1.7");
    let parent = doc.new_object_id();
    let content = doc.add_object(Stream::new(dictionary! {}, b"0 0 1 rg\n".repeat(ops)));
    let page = doc.add_object(dictionary! {
        "Type" => "Page",
        "Parent" => parent,
        "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
        "Contents" => content,
    });
    doc.objects.insert(parent, Object::Dictionary(dictionary! { "Type" => "Pages", "Kids" => vec![page.into()], "Count" => 1 }));
    let catalog = doc.add_object(dictionary! { "Type" => "Catalog", "Pages" => parent });
    doc.trailer.set("Root", catalog);
    let mut bytes = Vec::new();
    doc.save_to(&mut bytes).unwrap();
    bytes
}

/// Catches a cancel only taking effect between pages: on a page of millions of operators the
/// host would wait the whole page out. The partial page must also never be handed over.
#[test]
fn cancelling_stops_inside_a_heavy_page() {
    let doc = Arc::new(open(&heavy_page(6_000_000)));
    let stats = doc.scan_document(None, &mut |_| {}).unwrap();
    let saved = stats.to_json().unwrap();

    let started = Instant::now();
    doc.page_stream(stats).unwrap().page(1).unwrap();
    let uncancelled = started.elapsed();

    let cancel = CancelToken::new();
    let trigger = cancel.clone();
    let worker = {
        let doc = Arc::clone(&doc);
        std::thread::spawn(move || {
            let stats = DocStats::from_json(&saved).unwrap();
            let mut stream = doc.page_stream(stats).unwrap().with_cancel(cancel);
            let started = Instant::now();
            let result = stream.page(1);
            (result.map(|_| ()), started.elapsed())
        })
    };
    std::thread::sleep(uncancelled.min(Duration::from_millis(50)) / 5);
    trigger.cancel();
    let (result, took) = worker.join().unwrap();
    assert!(matches!(result, Err(Error::Cancelled)), "a stopped page is not handed over");
    assert!(took < uncancelled, "stopped early: {took:?} of {uncancelled:?}");
}

/// Catches statistics from another file being trusted, which would silently misread its pages.
#[test]
fn statistics_from_another_document_are_refused() {
    let one = open(&pdf_of(&[&["A single page."]]));
    let two = open(&pdf_of(&[&["A first page."], &["A second page."]]));
    let stats = one.scan_document(None, &mut |_| {}).unwrap();
    assert!(matches!(two.page_stream(stats.clone()), Err(Error::StaleStats(_))));
    let json = stats.to_json().unwrap();
    let start = json.find("\"version\":").unwrap();
    let end = start + json[start..].find(',').unwrap();
    let json = format!("{}\"version\":9999{}", &json[..start], &json[end..]);
    assert!(matches!(one.page_stream(DocStats::from_json(&json).unwrap()), Err(Error::StaleStats(_))));
    assert!(matches!(DocStats::from_json("not json"), Err(Error::StaleStats(_))));
}

/// Catches saved statistics rendering differently from fresh ones.
#[test]
fn saved_statistics_round_trip() {
    let lines: Vec<&[&str]> = vec![&["Report running header", "Body text on a page that ends here."]; 8];
    let doc = open(&pdf_of(&lines));
    let stats = doc.scan_document(None, &mut |_| {}).unwrap();
    let back = DocStats::from_json(&stats.to_json().unwrap()).unwrap();
    let fresh: Vec<_> = doc.page_stream(stats).unwrap().map(|p| p.unwrap().markdown).collect();
    let saved: Vec<_> = doc.page_stream(back).unwrap().map(|p| p.unwrap().markdown).collect();
    assert_eq!(fresh, saved);
}

/// Catches hostile fixtures panicking the page path, or a page path that loses a document the
/// whole-document path can read. Includes the form bomb and the deep page trees.
#[test]
fn hostile_fixtures_do_not_panic_and_keep_their_text() {
    let dir = fixtures().join("adversarial");
    let mut checked = 0;
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().and_then(|e| e.to_str()) != Some("pdf") {
            continue;
        }
        let Ok(doc) = PdfDocument::open_with_engine(path.to_str().unwrap(), Engine::Lazy) else { continue };
        let pages = all_pages(&doc);
        assert_eq!(pages.len(), doc.page_count(), "{}", path.display());
        let html = doc.render(Mode::Page, false, false);
        let (whole, _) = markdown::html_to_markdown(&html, false, ImgMode::Placeholder);
        let joined: String = pages.iter().map(|p| strip_markers(&p.markdown) + "\n").collect();
        assert_eq!(sorted_words(&joined), sorted_words(&whole), "{}", path.display());
        checked += 1;
    }
    assert!(checked >= 4, "the adversarial fixtures went missing: {checked}");
}

/// Catches a page path that disagrees with the whole-document path on any committed fixture, by
/// more than the differences the module docs name.
#[test]
fn fixtures_read_the_same_as_the_whole_document() {
    let mut checked = 0;
    for entry in std::fs::read_dir(fixtures()).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().and_then(|e| e.to_str()) != Some("pdf") {
            continue;
        }
        let Ok(doc) = PdfDocument::open_with_engine(path.to_str().unwrap(), Engine::Lazy) else { continue };
        let joined: String = all_pages(&doc).iter().map(|p| strip_markers(&p.markdown) + "\n").collect();
        let html = doc.render(Mode::Page, false, false);
        let (whole, _) = markdown::html_to_markdown(&html, false, ImgMode::Placeholder);
        assert_eq!(sorted_words(&joined), sorted_words(&whole), "{}", path.display());
        checked += 1;
    }
    assert!(checked >= 10, "the fixtures went missing: {checked}");
}

/// Catches `/PageLabels` being dropped: the agent cites by label when the document has them.
#[test]
fn page_labels_come_through() {
    let mut doc = Document::load_mem(&pdf_of(&[&["One."], &["Two."], &["Three."]])).unwrap();
    let catalog = doc.trailer.get(b"Root").unwrap().as_reference().unwrap();
    let labels = dictionary! { "Nums" => vec![
        0.into(), Object::Dictionary(dictionary! { "S" => "r" }),
        1.into(), Object::Dictionary(dictionary! { "S" => "D", "St" => 10 }),
    ]};
    doc.get_object_mut(catalog).unwrap().as_dict_mut().unwrap().set("PageLabels", labels);
    let mut bytes = Vec::new();
    doc.save_to(&mut bytes).unwrap();
    let pages = all_pages(&open(&bytes));
    assert_eq!(pages[0].label.as_deref(), Some("i"));
    assert_eq!(pages[1].label.as_deref(), Some("10"));
    assert_eq!(pages[2].label.as_deref(), Some("11"));
}

/// Catches hostile `/PageLabels` hanging or exhausting memory. A range opening at the bottom of
/// the integers used to loop for centuries, and a huge letter counter made a label per page of
/// hundreds of MiB.
#[test]
fn hostile_page_labels_are_cheap() {
    let mut doc = Document::load_mem(&pdf_of(&[&["One."], &["Two."]])).unwrap();
    let catalog = doc.trailer.get(b"Root").unwrap().as_reference().unwrap();
    let labels = dictionary! { "Nums" => vec![
        Object::Integer(i64::MIN), Object::Dictionary(dictionary! { "S" => "D" }),
        0.into(), Object::Dictionary(dictionary! { "S" => "a", "St" => 4_000_000_000i64 }),
        1.into(), Object::Dictionary(dictionary! { "S" => "D", "St" => i64::MAX }),
    ]};
    doc.get_object_mut(catalog).unwrap().as_dict_mut().unwrap().set("PageLabels", labels);
    let mut bytes = Vec::new();
    doc.save_to(&mut bytes).unwrap();
    let opened = open(&bytes);
    let pages = all_pages(&opened);
    assert_eq!(pages[0].label.as_deref(), Some("4000000000"), "past the cap, digits");
    assert_eq!(pages[1].label.as_deref(), Some("9223372036854775807"));
    // The model path walks the same ranges in a loop of its own.
    let dir = std::env::temp_dir().join(format!("distillpdf-labels-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let out = dir.join("labels.dpdf");
    opened.distill(out.to_str(), &distillpdf::DistillOptions::text_only()).expect("distill");
    let _ = std::fs::remove_dir_all(dir);
}

/// Runs `f` on its own thread and fails the test, rather than hanging the suite, past `seconds`.
fn within<T: Send + 'static>(seconds: u64, f: impl FnOnce() -> T + Send + 'static) -> T {
    let (done, finished) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = done.send(f());
    });
    finished.recv_timeout(Duration::from_secs(seconds)).expect("hung past its limit")
}

fn page_with(content: Vec<u8>) -> Vec<u8> {
    let mut doc = Document::with_version("1.7");
    let font = doc.add_object(dictionary! { "Type" => "Font", "Subtype" => "Type1", "BaseFont" => "Helvetica" });
    let parent = doc.new_object_id();
    let content = doc.add_object(Stream::new(dictionary! {}, content));
    let page = doc.add_object(dictionary! {
        "Type" => "Page",
        "Parent" => parent,
        "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
        "Contents" => content,
        "Resources" => dictionary! { "Font" => dictionary! { "F1" => font } },
    });
    doc.objects.insert(parent, Object::Dictionary(dictionary! { "Type" => "Pages", "Kids" => vec![page.into()], "Count" => 1 }));
    let catalog = doc.add_object(dictionary! { "Type" => "Catalog", "Pages" => parent });
    doc.trailer.set("Root", catalog);
    let mut bytes = Vec::new();
    doc.save_to(&mut bytes).unwrap();
    bytes
}

/// Catches the gutter sweep spinning once `x += step` stops changing `x`, at 2^24 and up with a
/// narrow range, which no cancel could reach. Every path that lays out a page runs it.
#[test]
fn text_far_off_the_page_does_not_hang_any_path() {
    for x in ["20000000", "1000000000", "16777217"] {
        let mut content = b"BT /F1 10 Tf ".to_vec();
        for line in 0..40 {
            content.extend_from_slice(
                format!("1 0 0 1 {x} {} Tm (word wide wide wide wide wide wide wide wide wide wide) Tj\n", 700 - line * 12).as_bytes(),
            );
        }
        content.extend_from_slice(b"ET\n");
        let bytes = page_with(content);
        within(120, move || {
            let doc = open(&bytes);
            doc.extract_page_text(1).unwrap();
            let _ = doc.extract_text();
            let _ = doc.render(Mode::Page, false, false);
            let stats = doc.scan_document(None, &mut |_| {}).unwrap();
            doc.page_stream(stats).unwrap().page(1).unwrap();
        });
    }
}

/// Catches a NaN coordinate reaching a sort. Rust's sort may panic on an ordering that isn't total,
/// and `partial_cmp().unwrap_or(Equal)` is not one, so hostile numbers became a `PanicException`.
/// These two files, the repo's own fixtures with numbers changed by a fuzzer, panicked the render.
#[test]
fn files_that_made_nan_coordinates_do_not_panic() {
    let data = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data");
    for name in ["nan_sort_report.pdf", "nan_sort_chrome.pdf"] {
        let bytes = std::fs::read(data.join(name)).unwrap();
        let doc = open(&bytes);
        let _ = doc.extract_text();
        let _ = doc.render(Mode::Page, false, false);
        let _ = doc.render(Mode::Section, false, true);
        let stats = doc.scan_document(None, &mut |_| {}).unwrap();
        let mut stream = doc.page_stream(stats).unwrap();
        for page in stream.by_ref() {
            page.unwrap();
        }
    }
}

/// Catches an overflowed matrix putting text at a NaN position: it must not reach layout, which
/// measures and sorts by these numbers, and the rest of the page must still read.
#[test]
fn nan_coordinates_do_not_panic_any_path() {
    // Each `cm` overflows the matrix to infinity, and infinity times zero is NaN.
    let huge = "340282350000000000000000000000000000000.0";
    let mut content = Vec::new();
    for line in 0..80 {
        if line % 2 == 0 {
            content.extend_from_slice(format!("q {huge} 0 0 {huge} {huge} {huge} cm {huge} 0 0 {huge} 0 0 cm 0 0 0 0 0 0 cm\n").as_bytes());
        } else {
            content.extend_from_slice(b"q\n");
        }
        content.extend_from_slice(format!("BT /F1 10 Tf 1 0 0 1 {} {} Tm (word {line}) Tj ET Q\n", 40 + line * 3, 700 - line * 7).as_bytes());
        content.extend_from_slice(format!("q 0 0 m {huge} {huge} l S {} {} 50 10 re S Q\n", 40 + line * 3, 700 - line * 7).as_bytes());
    }
    let bytes = page_with(content);
    let doc = open(&bytes);
    let text = doc.extract_text();
    assert!(text.contains("word 1"), "the placed lines still read: {text}");
    let _ = doc.render(Mode::Page, false, false);
    let _ = doc.render(Mode::Section, false, true);
    let stats = doc.scan_document(None, &mut |_| {}).unwrap();
    assert!(stats.to_json().is_ok());
    doc.page_stream(stats).unwrap().page(1).unwrap();
}

/// Catches a raw inline image confusing a chunk boundary: `BI` and ` EI ` in its samples used to
/// lose every line after it and read the samples as page text, an injection path.
#[test]
fn inline_image_samples_never_become_text_or_lose_the_page() {
    let mut samples = Vec::new();
    for _ in 0..8 {
        samples.extend_from_slice(b"Q EI ");
    }
    samples.extend_from_slice(b"BT /F1 10 Tf 72 700 Td (INJECTED) Tj ET BI x");
    let mut content = format!("BI /W {} /H 1 /BPC 8 /CS /G ID ", samples.len()).into_bytes();
    content.extend_from_slice(&samples);
    content.extend_from_slice(b" EI\nBT /F1 10 Tf 72 720 Td 13 TL\n");
    for _ in 0..1800 {
        content.extend_from_slice(b"(the visible body line) Tj T*\n");
    }
    content.extend_from_slice(b"ET\n");
    let decoded = Content::decode(&content).expect("the whole stream parses");
    let bytes = page_with(content);
    let doc = open(&bytes);
    let text = doc.extract_text();
    let expected = decoded.operations.iter().filter(|op| op.operator == "Tj").count();
    assert_eq!(text.matches("visible body").count(), expected, "as many lines as a whole decode finds");
    let stats = doc.scan_typography(None, &mut |_| {}).unwrap();
    let page = doc.page_stream(stats).unwrap().page(1).unwrap();
    assert_eq!(page.markdown.matches("visible body").count(), expected);
    assert_eq!(text.contains("INJECTED"), decoded.operations.iter().any(|op| matches!(op.operands.first(), Some(Object::String(s, _)) if s == b"INJECTED")));
}

/// Catches a walk stopped by the token leaving its partial page in the stream's cache, where a
/// retry returned it as `Ok` without the end of the page. Tried across many stopping points, since
/// where the token lands varies.
#[test]
fn a_retry_after_a_cancel_returns_the_whole_page() {
    let mut content = b"BT /F1 10 Tf 72 720 Td (BEGINWORD first line of the page text) Tj ET\n".to_vec();
    content.extend(b"0 0 1 rg\n".repeat(1_500_000));
    content.extend_from_slice(b"BT /F1 10 Tf 72 300 Td (ENDWORD last line of the page text) Tj ET\n");
    let bytes = page_with(content);
    let doc = Arc::new(open(&bytes));
    let stats = doc.scan_typography(None, &mut |_| {}).unwrap();
    let saved = stats.to_json().unwrap();
    let started = Instant::now();
    let fresh = doc.page_stream(stats).unwrap().page(1).unwrap();
    let whole = started.elapsed();
    assert!(fresh.markdown.contains("ENDWORD"));
    let mut cancelled = 0;
    for step in 0..24u32 {
        let token = CancelToken::new();
        let trigger = token.clone();
        let delay = whole * step / 24;
        let timer = std::thread::spawn(move || {
            std::thread::sleep(delay);
            trigger.cancel();
        });
        let mut stream = doc.page_stream(DocStats::from_json(&saved).unwrap()).unwrap().with_cancel(token);
        let first = stream.page(1);
        timer.join().unwrap();
        match first {
            Ok(page) => assert_eq!(page.markdown, fresh.markdown, "an uncancelled answer is whole"),
            Err(Error::Cancelled) => cancelled += 1,
            Err(other) => panic!("{other}"),
        }
        let mut stream = stream.with_cancel(CancelToken::new());
        assert_eq!(stream.page(1).unwrap().markdown, fresh.markdown, "cancel point {step}");
    }
    assert!(cancelled > 0, "no attempt was actually cancelled");
}

/// Catches `for page in stream` spinning: a loop that doesn't stop on `Err` got the same error
/// forever from a cancelled stream. `.page(n)` is how to retry.
#[test]
fn the_iterator_ends_after_an_error() {
    let doc = open(&pdf_of(&[&["One."], &["Two."]]));
    let stats = doc.scan_typography(None, &mut |_| {}).unwrap();
    let cancel = CancelToken::new();
    cancel.cancel();
    let results: Vec<_> = doc.page_stream(stats).unwrap().with_cancel(cancel).take(5).collect();
    assert_eq!(results.len(), 1, "one error, then the end");
    assert!(matches!(results[0], Err(Error::Cancelled)));
}

/// Catches a same-sized file being read under another file's statistics, which silently changes
/// which rows are dropped as running headers.
#[test]
fn statistics_of_another_file_of_the_same_size_are_refused() {
    let make = |head: &str, body: &str| {
        let pages: Vec<Vec<String>> = (0..8).map(|n| vec![head.to_string(), format!("{body} {n} here.")]).collect();
        let lines: Vec<Vec<&str>> = pages.iter().map(|page| page.iter().map(String::as_str).collect()).collect();
        let slices: Vec<&[&str]> = lines.iter().map(Vec::as_slice).collect();
        pdf_of(&slices)
    };
    let a = make("Annual Report Header", "Body text of document A page");
    let b = make("Secret Findings Line", "Body text of document B page");
    assert_eq!(a.len(), b.len());
    let (doc_a, doc_b) = (open(&a), open(&b));
    let stats = doc_a.scan_typography(None, &mut |_| {}).unwrap();
    assert!(matches!(doc_b.page_stream(stats.clone()), Err(Error::StaleStats(_))));
    assert!(doc_a.page_stream(stats).is_ok(), "its own file still fits");
}

/// Catches a font size overflowing to infinity, which serializes as `null` and can never load, so
/// a host holding saved statistics rescans for ever.
#[test]
fn statistics_of_a_hostile_font_size_can_be_saved_and_loaded() {
    let big = "9".repeat(45);
    let mut content = format!("BT /F1 {big}.0 Tf 72 720 Td (Huge text) Tj ET\n").into_bytes();
    content.extend_from_slice(b"BT /F1 10 Tf 72 700 Td (Body text line one here) Tj ET\n");
    let doc = open(&page_with(content));
    let stats = doc.scan_document(None, &mut |_| {}).unwrap();
    let json = stats.to_json().unwrap();
    let back = DocStats::from_json(&json).expect("what `to_json` writes, `from_json` reads");
    doc.page_stream(back).unwrap().page(1).unwrap();
}

/// Catches a replaced file coming back as blank pages under `Ok`, and a scan over one reporting
/// success: neither may answer from a file that is no longer the one opened.
#[test]
fn a_file_replaced_underneath_is_an_error_not_blank_pages() {
    let dir = std::env::temp_dir().join(format!("distillpdf-replaced-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("doc.pdf");
    let lines: Vec<&[&str]> = vec![&["A line of body text that ends here."]; 6];
    let bytes = pdf_of(&lines);
    std::fs::write(&path, &bytes).unwrap();
    let doc = PdfDocument::open_with_engine(path.to_str().unwrap(), Engine::Lazy).unwrap();
    let stats = doc.scan_document(None, &mut |_| {}).unwrap();
    let mut stream = doc.page_stream(stats).unwrap();
    stream.page(1).unwrap();
    let other: Vec<&[&str]> = vec![&["Entirely different words on this page now."]; 9];
    std::thread::sleep(Duration::from_millis(1100));
    std::fs::write(&path, pdf_of(&other)).unwrap();
    assert!(stream.page(4).is_err(), "a page read after the replace");
    assert!(doc.scan_typography(None, &mut |_| {}).is_err(), "a scan after the replace");
    let _ = std::fs::remove_dir_all(dir);
}

/// A ruled grid per page, in one column layout, the first row of the first page set bold. That is
/// what the proof in `table::finalize_continuation_proofs` accepts as one table.
fn ruled_chain(pages: usize) -> Vec<u8> {
    let mut doc = Document::with_version("1.7");
    let regular = doc.add_object(dictionary! { "Type" => "Font", "Subtype" => "Type1", "BaseFont" => "Helvetica" });
    let bold = doc.add_object(dictionary! { "Type" => "Font", "Subtype" => "Type1", "BaseFont" => "Helvetica-Bold" });
    let parent = doc.new_object_id();
    let mut kids: Vec<Object> = Vec::new();
    for page in 0..pages {
        let rows = 5;
        let columns = [4.0, 94.0, 176.8, 238.0];
        let mut content = String::from("q\n1 0 0 1 147.6 648.4 cm\nq\n");
        if page == 0 {
            content.push_str(".85 .85 .91 rg\nn 0 80 316.8 -16 re f*\n");
        }
        content.push_str("0 0 0 rg\n");
        for row in 0..rows {
            let font = if page == 0 && row == 0 { "F2" } else { "F1" };
            content.push_str(&format!("BT /{font} 8.5 Tf 12 TL ET\n"));
            for (column, x) in columns.iter().enumerate() {
                let y = 69.5 - row as f32 * 16.0;
                let cell = if page == 0 && row == 0 { format!("Head{column}") } else { format!("P{page}R{row}C{column}") };
                content.push_str(&format!("BT 1 0 0 1 {x} {y} Tm ({cell}) Tj T* ET\n"));
            }
        }
        content.push_str("q\n1 J\n1 j\n0 0 0 RG\n.5 w\n");
        for line in ["0 80 m 316.8 80", "0 0 m 316.8 0", "0 0 m 0 80", "316.8 0 m 316.8 80", "0 64 m 316.8 64", "0 48 m 316.8 48", "0 32 m 316.8 32", "0 16 m 316.8 16", "90 0 m 90 80", "172.8 0 m 172.8 80", "234 0 m 234 80"] {
            content.push_str(&format!("n {line} l S\n"));
        }
        content.push_str("Q\nQ\nQ\n");
        let content = doc.add_object(Stream::new(dictionary! {}, content.into_bytes()));
        let page = doc.add_object(dictionary! {
            "Type" => "Page", "Parent" => parent,
            "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
            "Contents" => content,
            "Resources" => dictionary! { "Font" => dictionary! { "F1" => regular, "F2" => bold } },
        });
        kids.push(page.into());
    }
    let count = kids.len() as i64;
    doc.objects.insert(parent, Object::Dictionary(dictionary! { "Type" => "Pages", "Kids" => kids, "Count" => count }));
    let catalog = doc.add_object(dictionary! { "Type" => "Catalog", "Pages" => parent });
    doc.trailer.set("Root", catalog);
    let mut bytes = Vec::new();
    doc.save_to(&mut bytes).unwrap();
    bytes
}

/// Catches a table over three pages misreporting where it starts: page 3's rows are on page 1, so
/// its marker must name page 1, not the empty page 2 between them. The rows must all be there once,
/// and the answer must not depend on the order pages are asked for.
#[test]
fn a_table_over_three_pages_goes_where_it_starts_and_says_so() {
    let bytes = ruled_chain(4);
    let doc = open(&bytes);
    let html = doc.render(Mode::Page, false, false);
    let (whole, _) = markdown::html_to_markdown(&html, false, ImgMode::Placeholder);
    let pages = all_pages(&doc);
    assert!(pages[0].markdown.contains("P3R4C3"), "the chain merged: {}", pages[0].markdown);
    assert_eq!(pages[0].continues_on, Some(2));
    for page in &pages[1..] {
        assert_eq!(page.continued_from, Some(1), "page {}", page.page);
        assert_eq!(page.continues_on, None, "page {}", page.page);
        assert!(!page.markdown.contains('|'), "no rows left on page {}", page.page);
    }
    let spilled: Vec<u32> = pages[0].continuations.iter().map(|c| c.page).collect();
    assert_eq!(spilled, vec![2, 3, 4]);
    let joined: String = pages.iter().map(|page| strip_markers(&page.markdown) + "\n").collect();
    assert_eq!(sorted_words(&joined), sorted_words(&whole));
    let stats = doc.scan_document(None, &mut |_| {}).unwrap();
    let mut stream = doc.page_stream(stats).unwrap();
    for n in [4u32, 2, 3, 1] {
        assert_eq!(stream.page(n).unwrap().markdown, pages[n as usize - 1].markdown, "page {n} out of order");
    }
}

/// Catches the chrome stage walking every span per row: a page of 80,000 rows held a scan for 13.7 s
/// with no way to stop it, six such pages over a minute. The stage itself now checks the token, so
/// this asserts the work, and `chrome.rs` tests the check.
#[test]
fn a_page_of_many_rows_scans_quickly() {
    let mut content = b"BT /F1 10 Tf ".to_vec();
    let height = 10_000_000.0f32;
    for row in 0..80_000 {
        content.extend_from_slice(format!("1 0 0 1 72 {} Tm (x) Tj\n", height - 1.0 - row as f32 * 2.0).as_bytes());
    }
    content.extend_from_slice(b"ET\n");
    let mut doc = Document::with_version("1.7");
    let font = doc.add_object(dictionary! { "Type" => "Font", "Subtype" => "Type1", "BaseFont" => "Helvetica" });
    let parent = doc.new_object_id();
    let mut kids: Vec<Object> = Vec::new();
    for _ in 0..6 {
        let stream = doc.add_object(Stream::new(dictionary! {}, content.clone()));
        let page = doc.add_object(dictionary! {
            "Type" => "Page", "Parent" => parent,
            "MediaBox" => vec![0.into(), 0.into(), 612.into(), Object::Real(height)],
            "Contents" => stream,
            "Resources" => dictionary! { "Font" => dictionary! { "F1" => font } },
        });
        kids.push(page.into());
    }
    doc.objects.insert(parent, Object::Dictionary(dictionary! { "Type" => "Pages", "Kids" => kids, "Count" => 6 }));
    let catalog = doc.add_object(dictionary! { "Type" => "Catalog", "Pages" => parent });
    doc.trailer.set("Root", catalog);
    let mut bytes = Vec::new();
    doc.save_to(&mut bytes).unwrap();
    within(60, move || {
        open(&bytes).scan_typography(None, &mut |_| {}).unwrap();
    });
}

/// Catches a span at the far edge of a huge page overflowing the chrome bucket's arithmetic, a
/// debug-build panic on a number that only needs to compare as far away.
#[test]
fn a_row_at_an_extreme_coordinate_does_not_overflow() {
    let pages: Vec<Vec<u8>> = (0..5)
        .map(|_| b"BT /F1 10 Tf 1 0 0 1 72 -999999999999 Tm (footer far away) Tj 1 0 0 1 72 700 Tm (body text here) Tj ET\n".to_vec())
        .collect();
    let mut doc = Document::with_version("1.7");
    let font = doc.add_object(dictionary! { "Type" => "Font", "Subtype" => "Type1", "BaseFont" => "Helvetica" });
    let parent = doc.new_object_id();
    let mut kids: Vec<Object> = Vec::new();
    for content in pages {
        let stream = doc.add_object(Stream::new(dictionary! {}, content));
        let page = doc.add_object(dictionary! {
            "Type" => "Page", "Parent" => parent,
            "MediaBox" => vec![0.into(), Object::Real(-1.0e13), 612.into(), Object::Real(1.0e13)],
            "Contents" => stream,
            "Resources" => dictionary! { "Font" => dictionary! { "F1" => font } },
        });
        kids.push(page.into());
    }
    doc.objects.insert(parent, Object::Dictionary(dictionary! { "Type" => "Pages", "Kids" => kids, "Count" => 5 }));
    let catalog = doc.add_object(dictionary! { "Type" => "Catalog", "Pages" => parent });
    doc.trailer.set("Root", catalog);
    let mut bytes = Vec::new();
    doc.save_to(&mut bytes).unwrap();
    let doc = open(&bytes);
    let stats = doc.scan_document(None, &mut |_| {}).unwrap();
    doc.page_stream(stats).unwrap().page(1).unwrap();
}
