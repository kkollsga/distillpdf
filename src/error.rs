//! The core crate error type — `distillpdf::Error`.
//!
//! A structured, `std::error::Error + Send + Sync` enum so pure-Rust callers (kglite's
//! `knowledge_tree`, any Rust embedder) get a real error type instead of stringly results.
//! The PyO3 binding (`src/lib.rs`) maps every variant to `PyValueError` via `Display`, so the
//! `Display` strings here are the SAME messages the Python API has always raised — pytest may
//! assert on them, so they are behaviour-locked. Hand-implemented (no `thiserror` dependency)
//! to match the repo's zero-extra-dep style.

use std::fmt;

/// Everything the core layer can fail with. `Display` is the user-facing message (and the
/// exact string the Python binding surfaces as `ValueError`).
#[derive(Debug)]
pub enum Error {
    /// Reading the source file failed (`open(path)`).
    Read(std::io::Error),
    /// Writing an output file failed.
    Write(std::io::Error),
    /// Creating an output directory failed.
    Mkdir(std::io::Error),
    /// Parsing the PDF container from a path failed.
    Open(String),
    /// Parsing the PDF container from bytes failed.
    Parse(String),
    /// An unknown render `mode` string (carries the offending value).
    InvalidMode(String),
    /// An unknown `image_mode` string (carries the offending value).
    InvalidImageMode(String),
    /// A directory output path was given but there is no source filename to derive a name.
    NoSourceDir,
    /// An output path was needed but the document was opened from bytes (no source path).
    NoSourcePath,
    /// The OCR cache mutex was poisoned.
    OcrPoisoned,
    /// A requested page does not exist. `None` renders "no page"; `Some(n)` renders "no page N".
    NoPage(Option<u32>),
    /// A lower-layer message passed through verbatim (container / model / PDF-assembly and the
    /// `assets=` profile parse — these already carry their fully-formed message).
    Model(String),
    /// A loaded `model.json` was not valid UTF-8.
    ModelNotUtf8(String),
    /// Parsing a supplied `model_json` string failed.
    ParseModelJson(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Read(e) => write!(f, "read failed: {e}"),
            Error::Write(e) => write!(f, "write failed: {e}"),
            Error::Mkdir(e) => write!(f, "mkdir failed: {e}"),
            Error::Open(e) => write!(f, "open failed: {e}"),
            Error::Parse(e) => write!(f, "parse failed: {e}"),
            Error::InvalidMode(m) => write!(f, "invalid mode {m:?}: expected \"section\" or \"page\""),
            Error::InvalidImageMode(m) => {
                write!(f, "invalid image_mode {m:?}: expected \"embed\", \"external\", or \"drop\"")
            }
            Error::NoSourceDir => write!(
                f,
                "a directory path needs a source filename to derive the name; pass a full file path"
            ),
            Error::NoSourcePath => write!(f, "no source path (opened from_bytes); pass an explicit path"),
            Error::OcrPoisoned => write!(f, "ocr cache poisoned"),
            Error::NoPage(None) => write!(f, "no page"),
            Error::NoPage(Some(n)) => write!(f, "no page {n}"),
            Error::Model(s) => write!(f, "{s}"),
            Error::ModelNotUtf8(e) => write!(f, "model json not utf-8: {e}"),
            Error::ParseModelJson(e) => write!(f, "parse model_json: {e}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Read(e) | Error::Write(e) | Error::Mkdir(e) => Some(e),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Error;

    #[test]
    fn display_strings_are_locked() {
        assert_eq!(Error::InvalidMode("foo".into()).to_string(), "invalid mode \"foo\": expected \"section\" or \"page\"");
        assert_eq!(
            Error::InvalidImageMode("x".into()).to_string(),
            "invalid image_mode \"x\": expected \"embed\", \"external\", or \"drop\""
        );
        assert_eq!(Error::NoPage(None).to_string(), "no page");
        assert_eq!(Error::NoPage(Some(7)).to_string(), "no page 7");
        assert_eq!(Error::OcrPoisoned.to_string(), "ocr cache poisoned");
        assert_eq!(Error::NoSourcePath.to_string(), "no source path (opened from_bytes); pass an explicit path");
        assert_eq!(Error::Model("verbatim".into()).to_string(), "verbatim");
    }

    #[test]
    fn error_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Error>();
    }
}
