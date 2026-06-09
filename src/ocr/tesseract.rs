//! The bundled **Tesseract** OCR engine (the "fast" tier). Builds on the from-source static
//! Leptonica + Tesseract linked by `build.rs`; feature-gated so the pure-Rust crate doesn't
//! pull the C build. distillPDF decodes the page image in Rust and feeds Tesseract **raw RGB**
//! pixels (`SetImage`), so Leptonica needs no image codecs. Recognized lines become
//! bare-dialect DocTags via [`tess_synth::lines_to_doctags`].
//!
//! `eng` language data is embedded in the binary (`include_bytes!`) and written to a
//! per-version cache dir on first use — so English works fully offline with no download.
//! Additional languages come from a tessdata directory resolved on the Python side (the
//! optional `distillpdf[languages]` companion package) and passed in via `cfg.tessdata_dir`,
//! or from `TESSDATA_PREFIX`.

use std::collections::HashMap;
use std::ffi::{c_char, c_int, c_void, CStr, CString};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};

use super::engine::{NativeCfg, OcrEngine};
use super::tess_synth::{lines_to_doctags, OcrLine};

// -- embedded language data (tessdata_fast) ----------------------------------
// Only English ships in the base wheel (offline out of the box). Other languages arrive via
// the `distillpdf[languages]` companion, whose dir is passed in `cfg.tessdata_dir`.
const TESSDATA: &[(&str, &[u8])] = &[
    ("eng", include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/models/tessdata/eng.traineddata"))),
];

const RIL_TEXTLINE: c_int = 2;
const PSM_AUTO: c_int = 3;

#[allow(non_snake_case)]
extern "C" {
    fn TessBaseAPICreate() -> *mut c_void;
    fn TessBaseAPIDelete(h: *mut c_void);
    fn TessBaseAPIInit3(h: *mut c_void, datapath: *const c_char, lang: *const c_char) -> c_int;
    fn TessBaseAPISetPageSegMode(h: *mut c_void, mode: c_int);
    fn TessBaseAPISetImage(h: *mut c_void, data: *const u8, w: c_int, hgt: c_int, bpp: c_int, bpl: c_int);
    fn TessBaseAPISetSourceResolution(h: *mut c_void, ppi: c_int);
    fn TessBaseAPIRecognize(h: *mut c_void, monitor: *mut c_void) -> c_int;
    fn TessBaseAPIGetIterator(h: *mut c_void) -> *mut c_void;
    fn TessResultIteratorDelete(it: *mut c_void);
    fn TessResultIteratorNext(it: *mut c_void, level: c_int) -> c_int;
    fn TessResultIteratorGetUTF8Text(it: *const c_void, level: c_int) -> *mut c_char;
    fn TessResultIteratorConfidence(it: *const c_void, level: c_int) -> f32;
    fn TessResultIteratorGetPageIteratorConst(it: *const c_void) -> *const c_void;
    fn TessPageIteratorBoundingBox(it: *const c_void, level: c_int,
        l: *mut c_int, t: *mut c_int, r: *mut c_int, b: *mut c_int) -> c_int;
    fn TessDeleteText(t: *mut c_char);
    // leptonica: quiet its stderr chatter (the disabled-TIFF debug-font path warns otherwise)
    fn setMsgSeverity(newsev: c_int) -> c_int;
}

/// An initialized `TessBaseAPI` handle. Not thread-safe for concurrent use, so it always
/// lives behind a `Mutex`; the raw pointer is `Send` only under that lock.
struct TessApi(*mut c_void);
unsafe impl Send for TessApi {}
impl Drop for TessApi {
    fn drop(&mut self) {
        unsafe { TessBaseAPIDelete(self.0) }
    }
}

/// Process-wide cache of initialized handles, keyed by the `+`-joined language string, so a
/// model is loaded at most once per language-set per process (pages reuse it).
fn cache() -> &'static Mutex<HashMap<String, Arc<Mutex<TessApi>>>> {
    static C: OnceLock<Mutex<HashMap<String, Arc<Mutex<TessApi>>>>> = OnceLock::new();
    C.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Delete the cached handles. Call before interpreter shutdown (via a Python `atexit`) so the
/// `TessBaseAPI`s are freed BEFORE Tesseract's C++ static caches tear down — otherwise the
/// dictionary (dawg) objects are reported as leaked on stderr. Harmless either way.
pub(crate) fn clear_cache() {
    if let Ok(mut g) = cache().lock() {
        g.clear();
    }
}

/// The languages embedded in the base wheel (English only).
fn bundled_langs() -> Vec<&'static str> {
    TESSDATA.iter().map(|(l, _)| *l).collect()
}

/// Write the embedded `eng` data to a per-version cache dir and return it. This is the
/// offline-out-of-the-box English path used when no external tessdata dir is provided.
fn embedded_datadir() -> Result<PathBuf, String> {
    let dir = std::env::temp_dir().join(concat!("distillpdf-tessdata-", env!("CARGO_PKG_VERSION")));
    std::fs::create_dir_all(&dir).map_err(|e| format!("tessdata dir: {e}"))?;
    for (lang, bytes) in TESSDATA {
        let f = dir.join(format!("{lang}.traineddata"));
        let fresh = std::fs::metadata(&f).map(|m| m.len() == bytes.len() as u64).unwrap_or(false);
        if !fresh {
            std::fs::write(&f, bytes).map_err(|e| format!("write {lang}.traineddata: {e}"))?;
        }
    }
    Ok(dir)
}

/// Resolve the tessdata directory and whether it is external (user/companion-provided, so any
/// language it contains is allowed). Priority: `cfg.tessdata_dir` (the `distillpdf[languages]`
/// companion) → `TESSDATA_PREFIX` → the embedded English cache.
fn resolve_datadir(cfg: &NativeCfg) -> Result<(PathBuf, bool), String> {
    if let Some(d) = cfg.tessdata_dir.as_deref().filter(|s| !s.is_empty()) {
        return Ok((PathBuf::from(d), true));
    }
    if let Ok(p) = std::env::var("TESSDATA_PREFIX") {
        if !p.is_empty() {
            return Ok((PathBuf::from(p), true));
        }
    }
    Ok((embedded_datadir()?, false))
}

/// Build the `+`-joined Tesseract language string. Defaults to English. When the data dir is
/// the embedded (English-only) one, a request for any other language is a clear error pointing
/// at the `distillpdf[languages]` extra.
fn resolve_langs(cfg: &NativeCfg, external: bool) -> Result<String, String> {
    if cfg.languages.is_empty() {
        return Ok("eng".into());
    }
    if !external {
        for l in &cfg.languages {
            if !bundled_langs().contains(&l.as_str()) {
                return Err(format!(
                    "Tesseract language {l:?} is not bundled — only {} ships in the base wheel. \
                     Install more with `pip install 'distillpdf[languages]'`, or point \
                     TESSDATA_PREFIX at a tessdata folder containing it.",
                    bundled_langs().join(", ")
                ));
            }
        }
    }
    Ok(cfg.languages.join("+"))
}

/// A handle onto the cached, initialized Tesseract API for one language-set.
pub(crate) struct TesseractEngine {
    api: Arc<Mutex<TessApi>>,
}

impl TesseractEngine {
    pub(crate) fn from_cfg(cfg: &NativeCfg) -> Result<Self, String> {
        let (dir, external) = resolve_datadir(cfg)?;
        let langs = resolve_langs(cfg, external)?;

        // Key the cache by (data dir + languages) so different language-sets/dirs don't collide.
        let key = format!("{}\u{0}{langs}", dir.display());
        let mut guard = cache().lock().map_err(|_| "tesseract cache poisoned".to_string())?;
        if let Some(api) = guard.get(&key) {
            return Ok(TesseractEngine { api: api.clone() });
        }
        // First use of this language-set: create + init a handle.
        let api = unsafe {
            setMsgSeverity(6); // L_SEVERITY_NONE — silence leptonica's stderr chatter
            let h = TessBaseAPICreate();
            if h.is_null() {
                return Err("TessBaseAPICreate returned null".into());
            }
            let dp = CString::new(dir.to_string_lossy().as_ref()).map_err(|e| e.to_string())?;
            let lg = CString::new(langs.as_str()).map_err(|e| e.to_string())?;
            if TessBaseAPIInit3(h, dp.as_ptr(), lg.as_ptr()) != 0 {
                TessBaseAPIDelete(h);
                return Err(format!("Tesseract init failed for languages {langs:?} (data dir {})", dir.display()));
            }
            TessBaseAPISetPageSegMode(h, PSM_AUTO);
            TessApi(h)
        };
        let api = Arc::new(Mutex::new(api));
        guard.insert(key, api.clone());
        Ok(TesseractEngine { api })
    }
}

impl OcrEngine for TesseractEngine {
    fn ocr_page(&self, image: &[u8]) -> Result<String, String> {
        let img = image::load_from_memory(image).map_err(|e| format!("decode image: {e}"))?.to_rgb8();
        let (w, h) = (img.width() as i32, img.height() as i32);
        let raw = img.into_raw(); // tightly packed RGB

        let guard = self.api.lock().map_err(|_| "tesseract handle poisoned".to_string())?;
        let hdl = guard.0;
        let mut lines: Vec<OcrLine> = Vec::new();
        unsafe {
            TessBaseAPISetImage(hdl, raw.as_ptr(), w, h.max(1), 3, w.saturating_mul(3));
            TessBaseAPISetSourceResolution(hdl, 150);
            TessBaseAPIRecognize(hdl, std::ptr::null_mut());
            let it = TessBaseAPIGetIterator(hdl);
            if !it.is_null() {
                loop {
                    let txt_ptr = TessResultIteratorGetUTF8Text(it, RIL_TEXTLINE);
                    let text = if txt_ptr.is_null() {
                        String::new()
                    } else {
                        let s = CStr::from_ptr(txt_ptr).to_string_lossy().into_owned();
                        TessDeleteText(txt_ptr);
                        s
                    };
                    let conf = TessResultIteratorConfidence(it, RIL_TEXTLINE);
                    let pit = TessResultIteratorGetPageIteratorConst(it);
                    let (mut l, mut t, mut r, mut b) = (0, 0, 0, 0);
                    TessPageIteratorBoundingBox(pit, RIL_TEXTLINE, &mut l, &mut t, &mut r, &mut b);
                    lines.push(OcrLine { text, x1: l, y1: t, x2: r, y2: b, conf });
                    if TessResultIteratorNext(it, RIL_TEXTLINE) == 0 {
                        break;
                    }
                }
                TessResultIteratorDelete(it);
            }
        }
        drop(guard);
        Ok(lines_to_doctags(lines, w, h))
    }
}
