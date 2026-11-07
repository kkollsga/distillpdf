//! Structured output one page at a time, with memory that does not grow with the document.
//!
//! The whole-document path ([`crate::html::render_doc_elements`]) holds every page's spans and
//! elements at once, cannot be stopped, and peaks at hundreds of MiB. This path renders one page
//! at a time through the same per-page code, so a page here is the page there.
//!
//! ```no_run
//! # fn main() -> Result<(), distillpdf::Error> {
//! use distillpdf::{Engine, PdfDocument};
//! let doc = PdfDocument::open_with_engine("report.pdf", Engine::Lazy)?;
//! let stats = doc.scan_document(None, &mut |_progress| {})?;
//! let mut pages = doc.page_stream(stats)?;
//! for page in &mut pages {
//!     let page = page?;
//!     println!("{} blocks on page {}", page.blocks.len(), page.page);
//! }
//! let twelve = pages.page(12)?; // any page, any order, the same answer
//! # let _ = twelve; Ok(())
//! # }
//! ```
//!
//! # What whole-document knowledge costs, and what it buys
//!
//! Heading levels, body size and running headers need the whole document. [`DocStats`] holds what
//! the page path needs of that, and there are two ways to get it, both holding one page at a time:
//!
//! * [`PdfDocument::scan_document`](crate::PdfDocument::scan_document) reads every page's text to
//!   learn the typography and running chrome, then renders every page once to count which styles
//!   flood the document and which headings recur. A page rendered from these statistics is the
//!   whole-document page, apart from the differences below. It costs what `extract_text` costs
//!   plus a sequential render.
//! * [`PdfDocument::scan_typography`](crate::PdfDocument::scan_typography) does only the first half,
//!   at the cost of `extract_text`. Chrome still goes, but a heading style flooding a filing stays
//!   in and a repeated heading stays a heading: a stated quality trade-off for an answer now.
//!
//! Save the result with [`DocStats::to_json`] to skip the scan next time.
//!
//! **State and its bounds.** [`DocStats`] holds counts and digests, no document text except the
//! outline's titles: typically a few KiB, capped by construction ([`DocStats::approx_bytes`]
//! reports it). [`PageStream`] adds a page index of about 56 bytes a page, the declared tables of
//! a tagged PDF, and at most [`MAX_CACHED_PAGES`] rendered pages, or a table's chain of at most
//! [`SEGMENT_PAGES`].
//!
//! **What the process costs on macOS.** This module's numbers are live heap. The system allocator
//! keeps freed large blocks dirty and in the footprint: the 383-page NASA report peaks at 42 MiB of
//! heap and 283 MiB of `phys_footprint`. Pressure relief does not return them. A helper process
//! started with `MallocLargeCache=0` in its environment peaks at 38 MiB, measured with no visible
//! time cost. It must be set before the process starts, so it is the host's to set.
//!
//! # A page's output is a pure function of the document and its page number
//!
//! The order pages are asked for never shows. That makes output cacheable and lets a killed
//! process resume, and it is why three things differ from concatenating the whole-document
//! Markdown, all named here and scored by the regression gate:
//!
//! 1. **Blocks crossing a page.** A paragraph or table that runs onto the next page goes on the
//!    page where it starts, whole, followed by a `[continues on PDF p. N]` line, and page N opens
//!    with `[continued from PDF p. M]`. The whole-document path leaves paragraphs split at the
//!    break, so a phrase over the break cannot be found there. Paragraphs join only when the next
//!    page opens mid-sentence: lowercase, after text that has no closing punctuation. The moved
//!    text goes straight onto the paragraph it continues, so it lands before that page's
//!    footnotes, where the whole-document order had the footnotes first.
//! 2. **Ruled tables across a segment boundary.** A proven table continuation is merged within a
//!    run of [`SEGMENT_PAGES`] pages, not beyond. That bounds what one page must render. Past the
//!    boundary the rows stay on their own pages, with the markers, and no row is lost.
//! 3. **Ids that need the whole document.** An unnumbered figure's anchor is `#p12-fig-1`, not a
//!    document-wide running number. A second "Figure 3" keeps `#fig-3`, because `fig-3-2` would
//!    depend on the pages before it.
//!
//! # What bounds a hostile file
//!
//! A page's own work is capped: operators by the walk budget, text at
//! 1,000,000 spans and 32 MiB, graphics-state nesting at 4,096, a
//! form's inflated content at 64 MiB, a page's link text at 4 MiB. Per document, [`DocStats`] holds
//! at most 1 MiB of bookmark keys and 16,384 label ranges of 256 bytes, which is the outer limit of
//! what a hostile file can make it hold; [`DocStats::approx_bytes`] says what it holds. Reading the
//! outline holds at most 16 MiB of titles for the moment it takes. Layout on a page at
//! those caps is slow rather than unbounded: it stops only at the next stage, so a host still needs
//! its own deadline, and the process limit is the host's.
//!
//! These caps cut silently: text past one is left out, with no error and no flag on [`PageOutput`].
//! They assume the lazy engine; the eager engine's page content isn't capped.
//!
//! # Stopping, and what a kill loses
//!
//! A [`CancelToken`] stops the scan and the stream between pages, and inside a heavy page at the
//! next operator walked. A cancelled page is discarded and never kept, so asking again answers it
//! whole. Each
//! [`PageOutput`] is complete and owned when returned, so a killed process loses only the page in
//! progress. Nothing here starts a thread: the caller's thread does all the work.

use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::ops::Range;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use lopdf::ObjectId;
use serde::{Deserialize, Serialize};

use crate::access::{DocumentAccess, PageRef};
use crate::error::Error;
use crate::headings::{HeadingPlan, HeadingScan};
use crate::html::{self, Bbox, DocPlan, ElKind, PageElement, PageFacts, PageIR};
use crate::layout::LinkBox;
use crate::model::Block;
use crate::structtree::DeclaredTable;
use crate::{chrome, docscan, elem_passes, links, markdown, model, nav, postprocess, text};

/// Bumped whenever a saved [`DocStats`] would mean something else to the renderer.
const STATS_VERSION: u32 = 2;

/// A ruled table's merge never crosses a multiple of this many pages. It bounds how many pages
/// one page's answer can need, and it is aligned, so where a table is cut does not depend on
/// which page was asked for.
pub const SEGMENT_PAGES: u32 = 16;

/// Rendered pages kept for the neighbours of the page being answered. A sequential read needs
/// the previous page and the next.
pub const MAX_CACHED_PAGES: usize = 3;

/// Headings tracked by the first pass. A document past this is hostile or a heading flood, and an
/// untracked heading simply stays a heading.
const MAX_HEADING_KEYS: usize = 64 * 1024;

const MAX_PROMOTE_KEY_BYTES: usize = 512;
const MAX_PROMOTE_BYTES: usize = 1024 * 1024;

/// The shortest paragraph tail that can be taken to run on: shorter is a label, not a sentence.
const MIN_RUN_ON_CHARS: usize = 20;

/// Stops a scan or a stream. Clone it and hand a copy to whoever decides.
#[derive(Clone, Default, Debug)]
pub struct CancelToken(Arc<AtomicBool>);

impl CancelToken {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.0.store(true, Ordering::Relaxed);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }
}

thread_local! {
    /// The token of the page call running on this thread, for [`crate::WalkBudget`], which sits
    /// too deep to thread a parameter to. Whole-document paths leave it empty.
    static ACTIVE_CANCEL: RefCell<Option<CancelToken>> = const { RefCell::new(None) };
}

/// Whether the page call on this thread has been cancelled. Read every 1,024 operators.
pub(crate) fn cancel_requested() -> bool {
    ACTIVE_CANCEL.with(|active| active.borrow().as_ref().is_some_and(CancelToken::is_cancelled))
}

/// Installs a token for the current thread until dropped, restoring whatever was there.
pub(crate) struct CancelScope(Option<CancelToken>);

impl CancelScope {
    pub(crate) fn new(token: Option<&CancelToken>) -> Self {
        Self(ACTIVE_CANCEL.with(|active| active.replace(token.cloned())))
    }
}

impl Drop for CancelScope {
    fn drop(&mut self) {
        ACTIVE_CANCEL.with(|active| *active.borrow_mut() = self.0.take());
    }
}

fn check(cancel: Option<&CancelToken>) -> Result<(), Error> {
    if cancel.is_some_and(CancelToken::is_cancelled) {
        Err(Error::Cancelled)
    } else {
        Ok(())
    }
}

/// How much of the document a scan reads. See the module docs for the trade-off.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ScanDepth {
    /// Typography and chrome, from text alone.
    Typography,
    /// Also renders every page once, so heading decisions match the whole-document path.
    Full,
}

/// Which stage a [`PageProgress`] reports. A document may repeat [`ScanPhase::Headings`] once, in
/// the rare case where its first reading found a heading style to distrust.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScanPhase {
    Typography,
    Headings,
}

/// `done` of `total` pages finished in `phase`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PageProgress {
    pub phase: ScanPhase,
    pub done: u32,
    pub total: u32,
}

/// A heading text that recurs often enough to be a running head, by digest.
#[derive(Clone, Serialize, Deserialize)]
struct RunningHead {
    key: u64,
    /// The page of its first level-1 occurrence, which stays a heading: the document's real title
    /// may legitimately recur in its running head.
    first_h1_page: Option<u32>,
}

/// What a document's first pass learned. Small and self-contained, see the module docs.
#[derive(Clone, Serialize, Deserialize)]
pub struct DocStats {
    version: u32,
    page_count: u32,
    source_len: u64,
    /// See `SourceRecovery::fingerprint`: length alone let two files of one size share statistics.
    fingerprint: String,
    depth: ScanDepth,
    plan: DocPlan,
    running_heads: Vec<RunningHead>,
    /// Outline titles by target page, as `(page, [(key, nesting depth)])`, sorted by page.
    promote: Vec<(u32, Vec<(String, u8)>)>,
    labels: Vec<model::build::LabelRange>,
}

/// Counts heading texts across a document.
#[derive(Default)]
struct HeadingKeyScan {
    counts: HashMap<u64, (u32, Option<u32>)>,
}

impl HeadingKeyScan {
    fn add(&mut self, page: u32, level: u8, text: &str) {
        let key = postprocess::demote_key(text);
        if key.len() < 4 {
            return;
        }
        let digest = key_digest(&key);
        let room = self.counts.len() < MAX_HEADING_KEYS;
        if room || self.counts.contains_key(&digest) {
            let entry = self.counts.entry(digest).or_default();
            entry.0 += 1;
            if level == 1 && entry.1.is_none() {
                entry.1 = Some(page);
            }
        }
    }

    fn finish(self) -> Vec<RunningHead> {
        let mut heads: Vec<RunningHead> = self
            .counts
            .into_iter()
            .filter(|(_, (count, _))| *count >= 3)
            .map(|(key, (_, first_h1_page))| RunningHead { key, first_h1_page })
            .collect();
        heads.sort_by_key(|head| head.key);
        heads
    }
}

fn key_digest(key: &str) -> u64 {
    key.bytes().fold(0xcbf2_9ce4_8422_2325u64, |hash, byte| (hash ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01b3))
}

impl DocStats {
    /// The bounded first pass. See the module docs for what it does and what it costs.
    pub(crate) fn scan(
        access: &dyn DocumentAccess,
        depth: ScanDepth,
        cancel: Option<&CancelToken>,
        progress: &mut dyn FnMut(PageProgress),
    ) -> Result<Self, Error> {
        let _scope = CancelScope::new(cancel);
        crate::error::catch_panics(|| Self::scan_inner(access, depth, cancel, progress))?
    }

    fn scan_inner(
        access: &dyn DocumentAccess,
        depth: ScanDepth,
        cancel: Option<&CancelToken>,
        progress: &mut dyn FnMut(PageProgress),
    ) -> Result<Self, Error> {
        let pages = access.pages_or_empty();
        let total = pages.len() as u32;
        let outline = links::outline(access);
        let mut promote: BTreeMap<u32, Vec<(String, u8)>> = BTreeMap::new();
        let mut promote_bytes = 0;
        for entry in &outline {
            let key = nav::title_key(&entry.title);
            // What the statistics keep of a bookmark is what they promise to keep small. A title
            // too long to match a heading of at most twelve words earns no place.
            if entry.page > 0 && key.len() <= MAX_PROMOTE_KEY_BYTES && promote_bytes < MAX_PROMOTE_BYTES {
                promote_bytes += key.len();
                promote.entry(entry.page).or_default().push((key, entry.level));
            }
        }

        let mut typography = docscan::TypographyScan::default();
        let mut chrome_scan = chrome::ChromeScan::new(access, pages.iter().map(|page| page.id));
        for (done, page) in pages.iter().enumerate() {
            check(cancel)?;
            let spans = text::extract_spans(access, page.id).unwrap_or_default();
            source_unchanged(access)?;
            typography.add_page(page.number, &spans);
            chrome_scan.add_page(access, page.id, &spans);
            progress(PageProgress { phase: ScanPhase::Typography, done: done as u32 + 1, total });
        }
        check(cancel)?;
        let (body, title_sz, profile) = typography.finish(pages.len(), !outline.is_empty());
        let chrome = chrome_scan.finish(body);
        // Spans with infinite positions never get here, but a size past the f32 range rounds to
        // infinity on its own, and a non-finite number has no JSON form: saved statistics would
        // never load, and a host would rescan for ever.
        let (body, title_sz) = (finite_or(body, 10.0), finite_or(title_sz, finite_or(body, 10.0)));
        let mut plan = DocPlan { body, title_sz, profile, chrome, head_plan: HeadingPlan::default() };
        let mut stats = Self {
            version: STATS_VERSION,
            page_count: total,
            source_len: access.source_len().unwrap_or(0),
            fingerprint: source_fingerprint(access)?,
            depth,
            plan: plan.clone(),
            running_heads: Vec::new(),
            promote: promote.into_iter().collect(),
            labels: model::build::page_label_ranges(access),
        };
        if depth == ScanDepth::Typography {
            return Ok(stats);
        }

        // One rendering gives both the heading styles to distrust and the heading texts that
        // recur. If a style turns out distrusted, the second answer was read with the wrong plan,
        // so it is read again: rare, and only ever on a heading-flooded filing.
        let page_no: HashMap<ObjectId, u32> = pages.iter().map(|page| (page.id, page.number)).collect();
        let declared = crate::structtree::declared_tables(access);
        let read_headings = |stats: &DocStats, progress: &mut dyn FnMut(PageProgress)| -> Result<(HeadingPlan, Vec<RunningHead>), Error> {
            let mut style_scan = HeadingScan::default();
            let mut key_scan = HeadingKeyScan::default();
            for (done, page) in pages.iter().enumerate() {
                check(cancel)?;
                let spans = text::extract_spans(access, page.id).unwrap_or_default();
                source_unchanged(access)?;
                style_scan.add_page(&spans, stats.plan.body, &stats.plan.profile);
                let (els, _) = render_page(access, stats, *page, &page_no, &declared, &spans);
                for element in &els {
                    if let ElKind::Heading { level, text, .. } = &element.kind {
                        key_scan.add(page.number, *level, text);
                    }
                }
                progress(PageProgress { phase: ScanPhase::Headings, done: done as u32 + 1, total });
            }
            check(cancel)?;
            Ok((style_scan.finish(), key_scan.finish()))
        };
        let (head_plan, running_heads) = read_headings(&stats, progress)?;
        stats.running_heads = running_heads;
        if !head_plan.is_empty() {
            plan.head_plan = head_plan;
            stats.plan = plan;
            (_, stats.running_heads) = read_headings(&stats, progress)?;
        }
        Ok(stats)
    }

    /// How much this scan read.
    pub fn depth(&self) -> ScanDepth {
        self.depth
    }

    /// The pages this document has.
    pub fn page_count(&self) -> u32 {
        self.page_count
    }

    /// Bytes these statistics hold, approximately.
    pub fn approx_bytes(&self) -> usize {
        let promote: usize = self
            .promote
            .iter()
            .map(|(_, entries)| 48 + entries.iter().map(|(key, _)| 32 + key.len()).sum::<usize>())
            .sum();
        std::mem::size_of::<Self>()
            + self.plan.chrome.state_bytes()
            + self.running_heads.len() * std::mem::size_of::<RunningHead>()
            + self.plan.profile.tiers.len() * std::mem::size_of::<crate::profile::HeadingTier>()
            + promote
            + self.labels.len() * 96
    }

    /// These statistics as JSON, to skip the scan next time this file is opened.
    pub fn to_json(&self) -> Result<String, Error> {
        serde_json::to_string(self).map_err(|error| Error::Model(error.to_string()))
    }

    /// Statistics saved by [`Self::to_json`]. [`crate::PdfDocument::page_stream`] refuses them
    /// if they do not fit the document opened.
    pub fn from_json(json: &str) -> Result<Self, Error> {
        serde_json::from_str(json).map_err(|error| Error::StaleStats(error.to_string()))
    }

    fn validate(&self, access: &dyn DocumentAccess, pages: usize) -> Result<(), Error> {
        if self.version != STATS_VERSION {
            return Err(Error::StaleStats(format!("saved by version {}, this is {STATS_VERSION}", self.version)));
        }
        if self.page_count as usize != pages {
            return Err(Error::StaleStats(format!("scanned {} pages, the document has {pages}", self.page_count)));
        }
        let source_len = access.source_len().unwrap_or(0);
        if self.source_len != source_len {
            return Err(Error::StaleStats(format!("scanned a {}-byte file, this one is {source_len}", self.source_len)));
        }
        if self.fingerprint != source_fingerprint(access)? {
            return Err(Error::StaleStats(String::from("scanned another file of the same size")));
        }
        Ok(())
    }
}

/// A non-finite number replaced, so it can be saved.
fn finite_or(value: f32, fallback: f32) -> f32 {
    if value.is_finite() {
        value
    } else {
        fallback
    }
}

fn source_fingerprint(access: &dyn DocumentAccess) -> Result<String, Error> {
    access.source_recovery().fingerprint().map_err(|error| Error::StaleStats(format!("the source can't be read: {error}")))
}

/// Whether the file is still the one opened. Read after a page's content, because a read that failed
/// for that reason came back as an empty page, the answer a caller is least able to question.
fn source_unchanged(access: &dyn DocumentAccess) -> Result<(), Error> {
    access
        .source_recovery()
        .validate_unchanged()
        .map_err(|error| Error::StaleStats(format!("the source changed: {error}")))
}

/// One page's element IR, through the same per-page code the whole-document path runs.
fn render_page(
    access: &dyn DocumentAccess,
    stats: &DocStats,
    page: PageRef,
    page_no: &HashMap<ObjectId, u32>,
    declared: &HashMap<ObjectId, Vec<DeclaredTable>>,
    spans: &Vec<text::Span>,
) -> (Vec<PageElement>, Vec<String>) {
    let n = page.number;
    let boxes: Vec<LinkBox> = links::page_links(access, n, page.id, page_no)
        .iter()
        .filter_map(|link| html::link_href(link).map(|href| LinkBox { rect: link.rect, href }))
        .collect();
    let promote = stats
        .promote
        .binary_search_by_key(&n, |(page, _)| *page)
        .map_or(&[][..], |index| stats.promote[index].1.as_slice());
    let facts = PageFacts {
        links: &boxes,
        dests: None,
        promote,
        declared: declared.get(&page.id).map(Vec::as_slice),
        emit_svg: false,
    };
    html::render_page_elements(access, &stats.plan, &facts, n as usize - 1, &n, &page.id, spans, false)
}

/// Part of a page's Markdown and the blocks it came from.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct MarkdownSpan {
    /// Byte range in [`PageOutput::markdown`].
    pub markdown: Range<usize>,
    /// Range in [`PageOutput::blocks`].
    pub blocks: Range<usize>,
}

/// Where a block found on this page carries on.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct BlockContinuation {
    /// Index in [`PageOutput::blocks`].
    pub block: usize,
    pub page: u32,
    /// The region its content occupies on that page, in that page's display space.
    pub bbox: Option<Bbox>,
}

/// One page's structured output. Complete and owned: nothing here refers back to the stream.
#[derive(Clone, Debug, Serialize)]
pub struct PageOutput {
    /// 1-based physical page number.
    pub page: u32,
    /// The page's `/PageLabels` label, when the document declares one.
    pub label: Option<String>,
    pub width_pts: f32,
    pub height_pts: f32,
    /// In reading order. A block's `id` is `p{page}-b{ordinal}`, and its `bbox` is in the page's
    /// display space: PDF points, origin bottom-left, after `/Rotate`.
    pub blocks: Vec<Block>,
    pub markdown: String,
    /// How [`Self::markdown`] maps to [`Self::blocks`].
    pub spans: Vec<MarkdownSpan>,
    /// A block here runs on to this page.
    pub continues_on: Option<u32>,
    /// A block on this earlier page took this page's opening content.
    pub continued_from: Option<u32>,
    pub continuations: Vec<BlockContinuation>,
}

/// One rendered page, and the same page with its cross-page tables settled and its page-local
/// passes run. Either can be recomputed from the document, so eviction is always safe.
struct CachedPage {
    page: u32,
    raw: PageIR,
    resolved: Option<ResolvedPage>,
}

#[derive(Clone)]
struct ResolvedPage {
    els: Vec<PageElement>,
    uris: Vec<String>,
    /// A table on this page runs on; its rows are here, or stay put at a segment boundary.
    table_out: Option<u32>,
    /// This page opens with the tail of a table from an earlier page.
    table_in: Option<u32>,
}

/// One page at a time from a document and its [`DocStats`]. Iterating yields pages in order;
/// [`Self::page`] answers any page, and both give the same bytes.
pub struct PageStream<'d> {
    access: &'d dyn DocumentAccess,
    stats: DocStats,
    running: HashMap<u64, Option<u32>>,
    pages: Vec<PageRef>,
    page_no: HashMap<ObjectId, u32>,
    declared: Option<HashMap<ObjectId, Vec<DeclaredTable>>>,
    cache: Vec<CachedPage>,
    next: u32,
    /// The iterator has handed over an error and ended. [`Self::page`] still answers.
    failed: bool,
    cancel: Option<CancelToken>,
    on_page: Option<Box<dyn FnMut(u32, u32) + 'd>>,
}

impl<'d> PageStream<'d> {
    pub(crate) fn new(access: &'d dyn DocumentAccess, stats: DocStats) -> Result<Self, Error> {
        let pages = access.pages_or_empty();
        stats.validate(access, pages.len())?;
        let page_no = pages.iter().map(|page| (page.id, page.number)).collect();
        let running = stats.running_heads.iter().map(|head| (head.key, head.first_h1_page)).collect();
        Ok(Self { access, stats, running, pages, page_no, declared: None, cache: Vec::new(), next: 1, failed: false, cancel: None, on_page: None })
    }

    /// Stops this stream between pages and inside a heavy page.
    pub fn with_cancel(mut self, cancel: CancelToken) -> Self {
        self.cancel = Some(cancel);
        // A new token is a decision to carry on, so an iterator ended by the last error resumes.
        self.failed = false;
        self
    }

    /// Calls `f(page, total)` as each page is handed over.
    pub fn with_progress(mut self, f: impl FnMut(u32, u32) + 'd) -> Self {
        self.on_page = Some(Box::new(f));
        self
    }

    pub fn page_count(&self) -> u32 {
        self.pages.len() as u32
    }

    /// Bytes of bookkeeping this stream holds beyond the page being rendered: the statistics, the
    /// page index and any declared tables. Rendered pages are not counted.
    pub fn state_bytes(&self) -> usize {
        let declared: usize = self
            .declared
            .iter()
            .flat_map(HashMap::values)
            .flatten()
            .map(|table| 64 + table.rows.iter().map(|row| 24 + row.len() * 96).sum::<usize>())
            .sum();
        self.stats.approx_bytes() + self.pages.len() * 56 + declared
    }

    /// The statistics, to save with [`DocStats::to_json`].
    pub fn stats(&self) -> &DocStats {
        &self.stats
    }

    /// Any page, by 1-based number.
    pub fn page(&mut self, n: u32) -> Result<PageOutput, Error> {
        let cancel = self.cancel.clone();
        let _scope = CancelScope::new(cancel.as_ref());
        check(cancel.as_ref())?;
        if n == 0 || n as usize > self.pages.len() {
            return Err(Error::NoPage(Some(n)));
        }
        let output = crate::error::catch_panics(|| self.build_output(n))?;
        if output.is_err() {
            // Whatever the page left half-built, nothing of it may answer the next call.
            self.cache.clear();
        }
        let output = output?;
        // A walk stopped by the token returns what it has, and that partial page must not leave.
        check(cancel.as_ref())?;
        let total = self.page_count();
        if let Some(on_page) = self.on_page.as_mut() {
            on_page(n, total);
        }
        Ok(output)
    }

    fn cached(&self, n: u32) -> Option<usize> {
        self.cache.iter().position(|entry| entry.page == n)
    }

    /// The index in `cache` of page `n` rendered, at the most recent position.
    fn raw(&mut self, n: u32) -> Result<usize, Error> {
        if let Some(index) = self.cached(n) {
            let entry = self.cache.remove(index);
            self.cache.push(entry);
            return Ok(self.cache.len() - 1);
        }
        check(self.cancel.as_ref())?;
        let page = self.pages[n as usize - 1];
        let spans = text::extract_spans(self.access, page.id).unwrap_or_default();
        let access = self.access;
        let declared = self.declared.get_or_insert_with(|| crate::structtree::declared_tables(access));
        let (els, uris) = render_page(self.access, &self.stats, page, &self.page_no, declared, &spans);
        drop(spans);
        // Both checks come after the read, because each one's failure looks like a short page. A walk
        // the token stopped returned what it had, and a file changed underneath reads as blank. A
        // page like that must not reach the cache, where a retry would find it.
        check(self.cancel.as_ref())?;
        source_unchanged(self.access)?;
        self.cache.push(CachedPage { page: n, raw: (n, els, uris), resolved: None });
        if self.cache.len() > MAX_CACHED_PAGES + SEGMENT_PAGES as usize {
            self.cache.remove(0);
        }
        Ok(self.cache.len() - 1)
    }

    /// Whether the table chain runs from `n` onto `n + 1`, by the proofs alone.
    fn tables_may_join(&mut self, n: u32) -> Result<bool, Error> {
        if n as usize >= self.pages.len() {
            return Ok(false);
        }
        let below = self.raw(n)?;
        if !self.cache[below].raw.1.iter().any(|e| matches!(&e.kind, ElKind::Table(t) if t.may_continue_below())) {
            return Ok(false);
        }
        let above = self.raw(n + 1)?;
        Ok(self.cache[above].raw.1.iter().any(|e| matches!(&e.kind, ElKind::Table(t) if t.may_continue_above())))
    }

    /// Page `n` with its table chain settled and page-local passes run.
    fn resolved(&mut self, n: u32) -> Result<ResolvedPage, Error> {
        let index = self.raw(n)?;
        if let Some(resolved) = &self.cache[index].resolved {
            return Ok(resolved.clone());
        }
        // The chain through `n`, kept inside `n`'s segment.
        let segment_start = (n - 1) / SEGMENT_PAGES * SEGMENT_PAGES + 1;
        let segment_end = (segment_start + SEGMENT_PAGES - 1).min(self.page_count());
        let mut first = n;
        while first > segment_start && self.tables_may_join(first - 1)? {
            first -= 1;
        }
        let mut last = n;
        while last < segment_end && self.tables_may_join(last)? {
            last += 1;
        }
        let mut window: Vec<PageIR> = Vec::with_capacity((last - first + 1) as usize);
        for page in first..=last {
            let index = self.raw(page)?;
            window.push(self.cache[index].raw.clone());
        }
        if window.len() > 1 {
            elem_passes::group_ruled_table_continuations(&mut window);
        }
        // The table chain may cross the segment boundary without merging. The rows stay put, but
        // the reader still hears that the table goes on.
        let boundary_out = n == segment_end && self.tables_may_join(n)? && self.merges_across_boundary(n)?;
        let boundary_in = n == segment_start && n > 1 && self.tables_may_join(n - 1)? && self.merges_across_boundary(n - 1)?;

        // The page holding the rows `n` lost, however far back the chain began.
        let table_in = window
            .iter()
            .find(|(_, els, _)| els.iter().flat_map(|e| e.spill.iter()).any(|(page, _)| *page == n))
            .map(|(page, _, _)| *page);
        let position = (n - first) as usize;
        let (_, mut els, uris) = window.swap_remove(position);
        // Everything `n`'s chain absorbed.
        let table_out = els.iter().flat_map(|e| e.spill.iter()).map(|(page, _)| *page).min();
        self.demote_running(n, &mut els);
        elem_passes::run_page_local_passes(&mut els);
        let resolved = ResolvedPage {
            els,
            uris,
            table_out: table_out.or(boundary_out.then_some(n + 1)),
            table_in: table_in.or(boundary_in.then_some(n - 1)),
        };
        let index = self.raw(n)?;
        self.cache[index].resolved = Some(resolved.clone());
        Ok(resolved)
    }

    /// Would the table chain ending on `n` merge with `n + 1`, if the segment allowed it?
    fn merges_across_boundary(&mut self, n: u32) -> Result<bool, Error> {
        let segment_start = (n - 1) / SEGMENT_PAGES * SEGMENT_PAGES + 1;
        let mut first = n;
        while first > segment_start && self.tables_may_join(first - 1)? {
            first -= 1;
        }
        let mut chain: Vec<PageIR> = Vec::new();
        for page in first..=n + 1 {
            let index = self.raw(page)?;
            chain.push(self.cache[index].raw.clone());
        }
        let tables = |ir: &PageIR| ir.1.iter().filter(|e| matches!(e.kind, ElKind::Table(_))).count();
        let before = chain.last().map_or(0, tables);
        elem_passes::group_ruled_table_continuations(&mut chain);
        Ok(chain.last().map_or(0, tables) < before)
    }

    fn demote_running(&self, page: u32, els: &mut [PageElement]) {
        if self.running.is_empty() {
            return;
        }
        let mut kept_h1: HashSet<u64> = HashSet::new();
        for element in els.iter_mut() {
            let ElKind::Heading { level, text, .. } = &mut element.kind else { continue };
            let key = postprocess::demote_key(text);
            if key.len() < 4 {
                continue;
            }
            let digest = key_digest(&key);
            let Some(first_h1_page) = self.running.get(&digest) else { continue };
            if *level == 1 && *first_h1_page == Some(page) && kept_h1.insert(digest) {
                continue;
            }
            element.kind = ElKind::Para { text: std::mem::take(text) };
        }
    }

    fn build_output(&mut self, n: u32) -> Result<PageOutput, Error> {
        let mut page = self.resolved(n)?;
        let total = self.page_count();

        // Does this page's opening paragraph belong to the page before?
        let mut continued_from = page.table_in;
        if n > 1 && movable_opening(&page.els).is_some() {
            let previous = self.resolved(n - 1)?;
            if closing_runs_on(&previous.els).is_some() {
                if let Some(i) = movable_opening(&page.els) {
                    page.els.remove(i);
                    continued_from = continued_from.or(Some(n - 1));
                }
            }
        }

        // Does this page's closing paragraph run on to the next?
        let mut continues_on = page.table_out;
        let mut spill_para = None;
        if n < total {
            if let Some(closing) = closing_runs_on(&page.els) {
                let next = self.resolved(n + 1)?;
                if let Some(opening) = movable_opening(&next.els) {
                    spill_para = Some((closing, next.els[opening].clone()));
                }
            }
        }
        if let Some((closing, moved)) = spill_para {
            let ElKind::Para { text: tail } = &moved.kind else { unreachable!("opening_para finds a paragraph") };
            if let ElKind::Para { text } = &mut page.els[closing].kind {
                html::append_piece(text, tail.trim());
            }
            page.els[closing].spill.push((n + 1, moved.bbox));
            continues_on = continues_on.or(Some(n + 1));
        }
        if self.cache.len() > MAX_CACHED_PAGES {
            let keep = |entry: &CachedPage| entry.page + 1 >= n && entry.page <= n + 1;
            self.cache.retain(keep);
        }

        let (width_pts, height_pts) = crate::ocr::page_size_pts(self.access, self.pages[n as usize - 1].id);
        let label = model::build::page_label(&self.stats.labels, n, total);
        let mut output = assemble_page(n, width_pts, height_pts, page, continues_on, continued_from);
        output.label = label;
        Ok(output)
    }
}

impl Iterator for PageStream<'_> {
    type Item = Result<PageOutput, Error>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.failed || self.next as usize > self.pages.len() {
            return None;
        }
        let output = self.page(self.next);
        // It ends here, so a loop that doesn't stop on `Err` cannot spin. A cancelled page has not
        // been handed over, and [`PageStream::page`] answers it afresh.
        match &output {
            Ok(_) => self.next += 1,
            Err(_) => self.failed = true,
        }
        Some(output)
    }
}

fn para_text(element: &PageElement) -> &str {
    match &element.kind {
        ElKind::Para { text } => text,
        _ => "",
    }
}

fn is_body(element: &PageElement) -> bool {
    !matches!(element.kind, ElKind::Footnotes { .. } | ElKind::DestAnchors(_))
}

fn body_blocks(els: &[PageElement]) -> usize {
    els.iter().filter(|e| is_body(e)).count()
}

/// The page's first body block, when it is a paragraph.
fn opening_para(els: &[PageElement]) -> Option<usize> {
    let i = els.iter().position(is_body)?;
    matches!(els[i].kind, ElKind::Para { .. }).then_some(i)
}

/// The page's last body block, when it is a paragraph.
fn closing_para(els: &[PageElement]) -> Option<usize> {
    let i = els.iter().rposition(is_body)?;
    matches!(els[i].kind, ElKind::Para { .. }).then_some(i)
}

/// Opens lowercase, on a real word, in text with at least three: display math reads lowercase too
/// (`n n`, `k =0:m`, `a,deg b u=1`), and a stray fragment of it must not join the page before.
fn starts_mid_sentence(html: &str) -> bool {
    let text = nav::strip_inline(html);
    let is_word = |token: &str| token.chars().filter(|c| c.is_alphabetic()).count() >= 2;
    let mut tokens = text.split_whitespace();
    let opens = tokens.next().is_some_and(|first| first.chars().next().is_some_and(char::is_lowercase) && is_word(first));
    opens && tokens.filter(|token| is_word(token)).count() >= 2
}

fn ends_mid_sentence(html: &str) -> bool {
    let text = nav::strip_inline(html);
    let text = text.trim_end();
    text.chars().count() >= MIN_RUN_ON_CHARS
        && text.chars().next_back().is_some_and(|c| c.is_alphanumeric() || matches!(c, ',' | '-' | '\u{00AD}' | '\u{2010}' | '\u{2011}'))
}

/// This page's closing paragraph, when it reads as running on. The page before it may also take
/// the page's opening paragraph, but never this same one: see [`movable_opening`].
fn closing_runs_on(els: &[PageElement]) -> Option<usize> {
    closing_para(els).filter(|&i| {
        let text = para_text(&els[i]);
        ends_mid_sentence(text) && !postprocess::is_math_fragment(text)
    })
}

/// This page's opening paragraph, when it reads as the tail of one on the page before. A page's
/// only block does not qualify when it itself runs on: a paragraph moves one page at most, which
/// keeps a page's answer a function of its two neighbours.
fn movable_opening(els: &[PageElement]) -> Option<usize> {
    opening_para(els).filter(|&i| {
        let text = para_text(&els[i]);
        starts_mid_sentence(text)
            && !postprocess::is_math_fragment(text)
            && !(body_blocks(els) == 1 && closing_runs_on(els).is_some())
    })
}

/// The finished page: blocks, Markdown, and the map between them.
fn assemble_page(
    n: u32,
    width_pts: f32,
    height_pts: f32,
    page: ResolvedPage,
    continues_on: Option<u32>,
    continued_from: Option<u32>,
) -> PageOutput {
    let ResolvedPage { els, uris, .. } = page;
    let mut markdown = String::new();
    let mut figures = 0usize;
    let mut pieces: Vec<Range<usize>> = Vec::with_capacity(els.len());
    if let Some(from) = continued_from {
        markdown.push_str(&format!("[continued from PDF p. {from}]"));
    }
    let mut image = 0usize;
    for element in &els {
        let fragment = element.html();
        let fragment = postprocess::rewrite_sentinels(&fragment, 0, |_index, out| {
            image += 1;
            out.push_str(&image.to_string());
        });
        let piece = markdown::fragment_to_markdown(&fragment, n, &mut figures);
        if piece.is_empty() {
            pieces.push(markdown.len()..markdown.len());
            continue;
        }
        if !markdown.is_empty() {
            markdown.push_str("\n\n");
        }
        let start = markdown.len();
        markdown.push_str(&piece);
        pieces.push(start..markdown.len());
    }
    if let Some(on) = continues_on {
        if !markdown.is_empty() {
            markdown.push_str("\n\n");
        }
        markdown.push_str(&format!("[continues on PDF p. {on}]"));
    }

    let spill: Vec<Vec<(u32, Option<Bbox>)>> = els.iter().map(|element| element.spill.clone()).collect();
    let mut block_ends: Vec<usize> = Vec::with_capacity(els.len());
    let mut blocks = model::build::project_page_blocks(&(n, els, uris), &mut |end| block_ends.push(end));
    for (ordinal, block) in blocks.iter_mut().enumerate() {
        block.id = format!("p{n}-b{:04}", ordinal + 1);
        // The front matter's fidelity fragment is the only place its text lives.
        if block.text.is_empty() {
            if let Some(html) = block.el_html.as_deref().filter(|_| matches!(block.kind, model::BlockKind::Header)) {
                block.text = nav::strip_inline(html).trim().to_string();
            }
        }
    }
    let mut spans = Vec::new();
    let mut continuations = Vec::new();
    let mut start = 0;
    for (index, end) in block_ends.iter().copied().enumerate() {
        if !pieces[index].is_empty() || end > start {
            spans.push(MarkdownSpan { markdown: pieces[index].clone(), blocks: start..end });
        }
        for (page, bbox) in &spill[index] {
            if end > start {
                continuations.push(BlockContinuation { block: end - 1, page: *page, bbox: *bbox });
            }
        }
        start = end;
    }
    PageOutput { page: n, label: None, width_pts, height_pts, blocks, markdown, spans, continues_on, continued_from, continuations }
}
