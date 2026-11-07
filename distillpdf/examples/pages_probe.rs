//! Whole-process memory and timing probe for the text and render paths. One mode per process,
//! release build, so `phys_footprint` measures that mode alone:
//!
//! ```text
//! cargo run --release -p distillpdf --example pages_probe -- MODE PDF [PAGE]
//! ```
//!
//! Prints one JSON line: wall ms, `lifetime_max_phys_footprint`, and the peak of live heap bytes
//! from the counting allocator below. Comparing the two separates what the program really holds
//! from what the allocator keeps after a free.
//!
//! Modes: `open`, `onepage`, `alltext`, `render`, `markdown`, `dump` (the byte hashes the
//! regression comparison reads).

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use distillpdf::{markdown, Engine, Mode, PdfDocument};

struct Counting;

static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

fn charge(bytes: usize) {
    let live = LIVE.fetch_add(bytes, Ordering::Relaxed) + bytes;
    PEAK.fetch_max(live, Ordering::Relaxed);
}

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let pointer = System.alloc(layout);
        if !pointer.is_null() {
            charge(layout.size());
        }
        pointer
    }
    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        System.dealloc(pointer, layout);
        LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
    }
    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let moved = System.realloc(pointer, layout, new_size);
        if !moved.is_null() {
            LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
            charge(new_size);
        }
        moved
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

#[cfg(target_os = "macos")]
fn footprint() -> (u64, u64) {
    // `struct rusage_info_v4` from <sys/resource.h>: 296 bytes of u64 and a 16-byte uuid.
    // Field offsets are counted in u64 slots after the uuid.
    #[repr(C)]
    struct RusageInfoV4 {
        uuid: [u8; 16],
        fields: [u64; 36],
    }
    extern "C" {
        fn proc_pid_rusage(pid: i32, flavor: i32, buffer: *mut RusageInfoV4) -> i32;
        fn getpid() -> i32;
    }
    const RUSAGE_INFO_V4: i32 = 4;
    const PHYS_FOOTPRINT: usize = 7;
    const LIFETIME_MAX_PHYS_FOOTPRINT: usize = 28;
    let mut info = RusageInfoV4 { uuid: [0; 16], fields: [0; 36] };
    // SAFETY: the buffer is at least as large as rusage_info_v4 and outlives the call.
    let status = unsafe { proc_pid_rusage(getpid(), RUSAGE_INFO_V4, &mut info) };
    if status != 0 {
        return (0, 0);
    }
    (info.fields[PHYS_FOOTPRINT], info.fields[LIFETIME_MAX_PHYS_FOOTPRINT])
}

#[cfg(not(target_os = "macos"))]
fn footprint() -> (u64, u64) {
    (0, 0)
}

const MIB: f64 = 1024.0 * 1024.0;

/// With `PROBE_RELIEF` set, hands freed memory back to the system after each page. The gap between
/// `peak_footprint_mib` and `peak_heap_mib` is what the system allocator keeps after a free, and
/// this measures how much of it a host can recover by asking.
fn relieve() {
    #[cfg(target_os = "macos")]
    {
        extern "C" {
            fn malloc_zone_pressure_relief(zone: *mut std::ffi::c_void, goal: usize) -> usize;
        }
        // SAFETY: a null zone means every zone, and a goal of 0 means as much as possible.
        unsafe {
            malloc_zone_pressure_relief(std::ptr::null_mut(), 0);
        }
    }
}

fn report(mode: &str, path: &str, ms: f64, extra: &str) {
    let (now, peak) = footprint();
    let name = path.rsplit('/').next().unwrap_or(path);
    println!(
        "{{\"mode\":\"{mode}\",\"doc\":\"{name}\",\"ms\":{ms:.1},\"footprint_mib\":{:.1},\"peak_footprint_mib\":{:.1},\"peak_heap_mib\":{:.1}{extra}}}",
        now as f64 / MIB,
        peak as f64 / MIB,
        PEAK.load(Ordering::Relaxed) as f64 / MIB,
    );
}

fn hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(bytes).iter().take(10).map(|b| format!("{b:02x}")).collect()
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let [_, mode, path, rest @ ..] = args.as_slice() else {
        eprintln!("usage: pages_probe MODE PDF [PAGE]");
        std::process::exit(2);
    };
    let start = Instant::now();
    let doc = match PdfDocument::open_with_engine(path, Engine::Lazy) {
        Ok(doc) => doc,
        Err(error) => {
            println!("{{\"mode\":\"{mode}\",\"doc\":\"{path}\",\"error\":{:?}}}", error.to_string());
            return;
        }
    };
    match mode.as_str() {
        "open" => report(mode, path, start.elapsed().as_secs_f64() * 1e3, ""),
        "onepage" => {
            let page: u32 = rest.first().and_then(|p| p.parse().ok()).unwrap_or(1);
            let text = doc.extract_page_text(page).unwrap_or_default();
            report(mode, path, start.elapsed().as_secs_f64() * 1e3, &format!(",\"chars\":{}", text.len()));
        }
        "alltext" => {
            let mut chars = 0;
            let mut worst = (0.0f64, 0u32);
            for page in 1..=doc.page_count() as u32 {
                let t = Instant::now();
                chars += doc.extract_page_text(page).map(|text| text.len()).unwrap_or(0);
                let ms = t.elapsed().as_secs_f64() * 1e3;
                if ms > worst.0 {
                    worst = (ms, page);
                }
            }
            report(
                mode,
                path,
                start.elapsed().as_secs_f64() * 1e3,
                &format!(",\"chars\":{chars},\"worst_page\":{},\"worst_ms\":{:.1}", worst.1, worst.0),
            );
        }
        "render" => {
            let html = doc.render(Mode::Page, false, false);
            report(mode, path, start.elapsed().as_secs_f64() * 1e3, &format!(",\"html\":{}", html.len()));
        }
        "markdown" => {
            let html = doc.render(Mode::Page, false, false);
            let (md, _) = markdown::html_to_markdown(&html, false, markdown::ImgMode::Placeholder);
            report(mode, path, start.elapsed().as_secs_f64() * 1e3, &format!(",\"md\":{}", md.len()));
        }
        "dump" => {
            let page_html = doc.render(Mode::Page, false, true);
            let section_html = doc.render(Mode::Section, false, true);
            let (page_md, _) = markdown::html_to_markdown(&page_html, false, markdown::ImgMode::Placeholder);
            let (section_md, _) = markdown::html_to_markdown(&section_html, true, markdown::ImgMode::Placeholder);
            let text = doc.extract_text();
            println!(
                "{{\"doc\":\"{}\",\"page_html\":\"{}\",\"section_html\":\"{}\",\"page_md\":\"{}\",\"section_md\":\"{}\",\"text\":\"{}\"}}",
                path.rsplit('/').next().unwrap_or(path),
                hex(page_html.as_bytes()),
                hex(section_html.as_bytes()),
                hex(page_md.as_bytes()),
                hex(section_md.as_bytes()),
                hex(text.as_bytes()),
            );
        }
        // Live-heap peak of each page's text extraction, heaviest first: which pages drive the
        // peak, and how much of it is still held after the page (caches, not the page itself).
        "textpeaks" => {
            let mut peaks = Vec::new();
            for page in 1..=doc.page_count() as u32 {
                let before = LIVE.load(Ordering::Relaxed);
                PEAK.store(before, Ordering::Relaxed);
                let _ = doc.extract_page_text(page);
                peaks.push((PEAK.load(Ordering::Relaxed) - before, page, LIVE.load(Ordering::Relaxed)));
            }
            peaks.sort_by_key(|(peak, ..)| std::cmp::Reverse(*peak));
            for (peak, page, live) in peaks.iter().take(8) {
                println!("{{\"page\":{page},\"page_peak_mib\":{:.1},\"live_after_mib\":{:.1}}}", *peak as f64 / MIB, *live as f64 / MIB);
            }
            report(mode, path, start.elapsed().as_secs_f64() * 1e3, "");
        }
        "scan" => {
            let mut last = Instant::now();
            let mut spent = [0.0f64; 2];
            let mut worst = [(0.0f64, 0u32); 2];
            let stats = doc
                .scan_document(None, &mut |progress| {
                    let phase = matches!(progress.phase, distillpdf::pages::ScanPhase::Headings) as usize;
                    let ms = last.elapsed().as_secs_f64() * 1e3;
                    last = Instant::now();
                    spent[phase] += ms;
                    if ms > worst[phase].0 {
                        worst[phase] = (ms, progress.done);
                    }
                })
                .expect("scan");
            report(
                mode,
                path,
                start.elapsed().as_secs_f64() * 1e3,
                &format!(
                    ",\"stats_bytes\":{},\"typography_ms\":{:.1},\"headings_ms\":{:.1},\"worst_typography\":[{:.1},{}],\"worst_headings\":[{:.1},{}]",
                    stats.approx_bytes(), spent[0], spent[1], worst[0].0, worst[0].1, worst[1].0, worst[1].1
                ),
            );
        }
        "stream" | "pagemd" => {
            let relief = std::env::var_os("PROBE_RELIEF").is_some();
            let mut on_progress = |_| {
                if relief {
                    relieve();
                }
            };
            // `PROBE_QUICK` measures the typography-only scan's trade-off.
            let stats = if std::env::var_os("PROBE_QUICK").is_some() {
                doc.scan_typography(None, &mut on_progress)
            } else {
                doc.scan_document(None, &mut on_progress)
            }
            .expect("scan");
            let scan_ms = start.elapsed().as_secs_f64() * 1e3;
            let mut stream = doc.page_stream(stats).expect("stream");
            let mut bytes = 0;
            let mut worst = (0.0f64, 0u32);
            let mut most_live = 0usize;
            let mut times = Vec::new();
            let mut out = String::new();
            let mut last_peak = footprint().1;
            if std::env::var_os("PROBE_JUMPS").is_some() {
                eprintln!("after scan: footprint {:.1}, peak {:.1}, live heap {:.1}", footprint().0 as f64 / MIB, last_peak as f64 / MIB, LIVE.load(Ordering::Relaxed) as f64 / MIB);
            }
            loop {
                let t = Instant::now();
                let Some(page) = stream.next() else { break };
                let page = page.expect("page");
                if relief {
                    relieve();
                }
                let ms = t.elapsed().as_secs_f64() * 1e3;
                let peak_now = footprint().1;
                if std::env::var_os("PROBE_JUMPS").is_some() && peak_now > last_peak + (4 << 20) {
                    eprintln!("page {} footprint peak {:.1} -> {:.1} MiB, now {:.1}, live heap {:.1}", page.page, last_peak as f64 / MIB, peak_now as f64 / MIB, footprint().0 as f64 / MIB, LIVE.load(Ordering::Relaxed) as f64 / MIB);
                }
                last_peak = peak_now;
                times.push(ms);
                if ms > worst.0 {
                    worst = (ms, page.page);
                }
                bytes += page.markdown.len();
                most_live = most_live.max(LIVE.load(Ordering::Relaxed));
                if mode == "pagemd" {
                    out.push_str(&page.markdown);
                    out.push_str("\n\n");
                }
            }
            times.sort_by(|a, b| a.total_cmp(b));
            let pct = |q: f64| times.get(((times.len() as f64 - 1.0) * q) as usize).copied().unwrap_or(0.0);
            if mode == "pagemd" {
                print!("{out}");
                return;
            }
            if std::env::var_os("PROBE_HOLD").is_some() {
                eprintln!("holding {}", std::process::id());
                std::thread::sleep(std::time::Duration::from_secs(20));
            }
            report(
                mode,
                path,
                start.elapsed().as_secs_f64() * 1e3,
                &format!(
                    ",\"scan_ms\":{scan_ms:.1},\"md\":{bytes},\"p50_ms\":{:.2},\"p95_ms\":{:.2},\"worst_ms\":{:.1},\"worst_page\":{},\"state_bytes\":{},\"most_live_between_pages_mib\":{:.1}",
                    pct(0.5), pct(0.95), worst.0, worst.1, stream.state_bytes(), most_live as f64 / MIB
                ),
            );
        }
        // Each page's Markdown from both paths as JSON, for `scripts/page_score.py`.
        "bothpages" => {
            let html = doc.render(Mode::Page, false, false);
            let whole: Vec<String> = html
                .split("<section data-page=")
                .skip(1)
                .map(|section| markdown::html_to_markdown(&format!("<section data-page={section}"), false, markdown::ImgMode::Placeholder).0)
                .collect();
            let stats = doc.scan_document(None, &mut |_| {}).expect("scan");
            let pages: Vec<String> = doc.page_stream(stats).expect("stream").map(|page| page.expect("page").markdown).collect();
            println!("{}", serde_json::json!({ "whole": whole, "pages": pages }));
        }
        "wholemd" => {
            let html = doc.render(Mode::Page, false, false);
            let (md, _) = markdown::html_to_markdown(&html, false, markdown::ImgMode::Placeholder);
            print!("{md}");
        }
        other => {
            eprintln!("unknown mode {other}");
            std::process::exit(2);
        }
    }
}
