//! Memory bounds of the page path, in their own binary because of the counting allocator.
//!
//! These assert live heap bytes rather than `phys_footprint`, which moves with the allocator and
//! the machine; `examples/pages_probe.rs` measures the whole process on real documents. Each test
//! holds `SERIAL` so one's allocations are not billed to another.

use std::alloc::{GlobalAlloc, Layout, System};
use std::io::{BufRead, Write};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use distillpdf::pages::DocStats;
use distillpdf::{Engine, PdfDocument};
use lopdf::content::{Content, Operation};
use lopdf::{dictionary, Document, Object, Stream};

struct Counting;

static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);
static SERIAL: Mutex<()> = Mutex::new(());

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let pointer = System.alloc(layout);
        if !pointer.is_null() {
            let live = LIVE.fetch_add(layout.size(), Ordering::Relaxed) + layout.size();
            PEAK.fetch_max(live, Ordering::Relaxed);
        }
        pointer
    }
    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        System.dealloc(pointer, layout);
        LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

const MIB: usize = 1024 * 1024;

/// Peak live heap above where `f` started, and `f`'s result.
fn peak_of<T>(f: impl FnOnce() -> T) -> (usize, T) {
    let start = LIVE.load(Ordering::Relaxed);
    PEAK.store(start, Ordering::Relaxed);
    let result = f();
    (PEAK.load(Ordering::Relaxed).saturating_sub(start), result)
}

fn pdf_with_contents(contents: Vec<Vec<u8>>) -> Vec<u8> {
    let mut doc = Document::with_version("1.7");
    let font = doc.add_object(dictionary! { "Type" => "Font", "Subtype" => "Type1", "BaseFont" => "Helvetica" });
    let parent = doc.new_object_id();
    let mut kids: Vec<Object> = Vec::new();
    for bytes in contents {
        let content = doc.add_object(Stream::new(dictionary! {}, bytes));
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

fn prose_page(n: usize) -> Vec<u8> {
    let mut ops = vec![
        Operation::new("BT", vec![]),
        Operation::new("Tf", vec!["F1".into(), 10.into()]),
        Operation::new("Td", vec![72.into(), 720.into()]),
        Operation::new("TL", vec![13.into()]),
    ];
    for line in 0..40 {
        ops.push(Operation::new("Tj", vec![Object::string_literal(format!("Line {line} of page {n} with several plain words on it."))]));
        ops.push(Operation::new("T*", vec![]));
    }
    ops.push(Operation::new("ET", vec![]));
    Content { operations: ops }.encode().unwrap()
}

fn prose_pdf(pages: usize) -> Vec<u8> {
    pdf_with_contents((0..pages).map(prose_page).collect())
}

fn open(bytes: &[u8]) -> PdfDocument {
    PdfDocument::from_bytes_with_engine(bytes, Engine::Lazy).expect("generated PDF opens")
}

/// Catches page-path memory growing with the document: a tenfold document must not cost a tenfold
/// peak, and the scan's statistics must not hold its text. The reader's own page index is the
/// only thing allowed to scale, and it is small per page.
#[test]
fn peak_does_not_grow_with_page_count() {
    let _serial = SERIAL.lock().unwrap();
    let measure = |pages: usize| {
        let bytes = prose_pdf(pages);
        let doc = open(&bytes);
        let (peak, stats) = peak_of(|| doc.scan_document(None, &mut |_| {}).unwrap());
        let stats_bytes = stats.approx_bytes();
        let mut stream = doc.page_stream(stats).unwrap();
        let (peak_stream, ()) = peak_of(|| {
            for page in &mut stream {
                drop(page.unwrap());
            }
        });
        (peak.max(peak_stream), stats_bytes, stream.state_bytes())
    };
    let (small_peak, small_stats, _) = measure(100);
    let (large_peak, large_stats, large_state) = measure(1000);
    assert!(
        large_peak < small_peak * 2 + 4 * MIB,
        "peak grew with the document: {small_peak} bytes at 100 pages, {large_peak} at 1000"
    );
    assert!(large_stats < small_stats * 2 + 64 * 1024, "statistics grew: {small_stats} then {large_stats}");
    assert!(large_state < 256 * 1024, "stated state of a 1000-page document: {large_state} bytes");
}

/// Catches the operator list being materialised again. Four million operators would cost over
/// 200 MiB as a list; a walk holding one chunk costs the decoded stream and a little.
#[cfg_attr(debug_assertions, ignore = "millions of operators through nom: release builds only")]
#[test]
fn a_page_of_millions_of_operators_stays_small() {
    let _serial = SERIAL.lock().unwrap();
    let bytes = pdf_with_contents(vec![b"q Q\n".repeat(2_000_000)]);
    let doc = open(&bytes);
    let (peak, stats) = peak_of(|| doc.scan_document(None, &mut |_| {}).unwrap());
    assert!(peak < 48 * MIB, "scan peaked at {} MiB", peak / MIB);
    let mut stream = doc.page_stream(stats).unwrap();
    let (peak, page) = peak_of(|| stream.page(1).unwrap());
    assert!(peak < 48 * MIB, "page peaked at {} MiB", peak / MIB);
    assert!(page.blocks.is_empty());
}

/// Catches hostile text flooding a page: spans grow per painted glyph run, so a page painting
/// millions of them must hit the page's span cap rather than the machine's memory.
#[cfg_attr(debug_assertions, ignore = "millions of operators through nom: release builds only")]
#[test]
fn a_page_flooding_text_is_capped() {
    let _serial = SERIAL.lock().unwrap();
    let mut content = b"BT /F1 10 Tf 72 720 Td ".to_vec();
    content.extend(b"(x) Tj\n".repeat(3_000_000));
    content.extend_from_slice(b"ET\n");
    let bytes = pdf_with_contents(vec![content]);
    let doc = open(&bytes);
    let (peak, _stats) = peak_of(|| doc.scan_document(None, &mut |_| {}).unwrap());
    assert!(peak < 512 * MIB, "scan peaked at {} MiB", peak / MIB);
}

fn digest(text: &str) -> u64 {
    text.bytes().fold(0xcbf2_9ce4_8422_2325u64, |hash, byte| (hash ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01b3))
}

/// The child half of the kill test: write each page's digest as it is taken, then wait to be
/// killed. Does nothing unless the parent set the variable.
#[test]
fn kill_child_writes_pages() {
    let Ok(out) = std::env::var("DISTILLPDF_KILL_OUT") else { return };
    let doc = open(&prose_pdf(40));
    let stats = doc.scan_document(None, &mut |_| {}).unwrap();
    let mut file = std::fs::File::create(out).unwrap();
    for page in doc.page_stream(stats).unwrap() {
        let page = page.unwrap();
        writeln!(file, "{} {}", page.page, digest(&page.markdown)).unwrap();
        file.flush().unwrap();
        // Slow enough for the parent to catch it mid-document.
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    std::thread::sleep(std::time::Duration::from_secs(60));
}

/// Catches a killed helper leaving the caller with wrong or partial pages: everything written
/// before a real SIGKILL must match an uninterrupted run byte for byte.
#[test]
fn a_kill_loses_nothing_already_taken() {
    let _serial = SERIAL.lock().unwrap();
    let doc = open(&prose_pdf(40));
    let stats = doc.scan_document(None, &mut |_| {}).unwrap();
    let expected: Vec<u64> = doc.page_stream(stats).unwrap().map(|page| digest(&page.unwrap().markdown)).collect();

    let dir = std::env::temp_dir().join(format!("distillpdf-kill-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let out = dir.join("pages.txt");
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "kill_child_writes_pages", "--nocapture"])
        .env("DISTILLPDF_KILL_OUT", &out)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let lines = || -> Vec<String> {
        std::fs::File::open(&out).map(|file| std::io::BufReader::new(file).lines().map_while(Result::ok).collect()).unwrap_or_default()
    };
    let started = std::time::Instant::now();
    while lines().len() < 5 {
        assert!(started.elapsed().as_secs() < 120, "the child never got going");
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    child.kill().unwrap();
    child.wait().unwrap();
    let written = lines();
    assert!(written.len() >= 5 && written.len() < 40, "killed mid-document: {} lines", written.len());
    for line in &written {
        let (page, value) = line.split_once(' ').expect("each line is whole");
        let page: usize = page.parse().unwrap();
        assert_eq!(value.parse::<u64>().unwrap(), expected[page - 1], "page {page}");
    }
    let _ = std::fs::remove_dir_all(dir);
    drop(DocStats::from_json("{}"));
}
