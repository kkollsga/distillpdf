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
    let json = stats.to_json().unwrap().replace("\"version\":1", "\"version\":9999");
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
