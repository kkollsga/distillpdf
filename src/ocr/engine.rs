//! The pluggable OCR backend. `OcrEngine` turns a page image into DocTags; everything
//! downstream (parsing, HTML, PDF) is pure Rust and backend-agnostic.
//!
//! The default [`ServerOcrEngine`] talks to a llama.cpp / granite OpenAI-compatible
//! server over plain HTTP (no extra deps: a tiny `TcpStream` client, JSON built and read
//! by hand). [`ReplayEngine`] returns pre-captured DocTags in call order — for tests and
//! for re-running a captured corpus without live inference. An in-process candle backend
//! can be added later behind a cargo feature, implementing the same trait.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use base64::Engine as _;

/// A backend that turns a single page image (PNG/JPEG bytes) into a DocTags string.
pub(crate) trait OcrEngine: Sync {
    fn ocr_page(&self, image: &[u8]) -> Result<String, String>;
}

/// Engine-agnostic options parsed from the Python side (a plain dict crosses the PyO3
/// boundary). Each engine picks the fields it cares about and ignores the rest — mirroring
/// how the Python `OcrConfig` dataclass is shared across backends.
#[derive(Default, Clone)]
pub(crate) struct NativeCfg {
    pub languages: Vec<String>,
    pub dpi: Option<u32>,
    pub prompt: Option<String>,
    pub max_tokens: Option<u32>,
    pub host: Option<String>,
    pub port: Option<u16>,
}

/// Build a Rust-native OCR engine by name. Unknown / compiled-out engines return `Err`.
/// This is the single registry that surfaces native engines (the bundled Tesseract, the
/// bring-your-own server) into the Python backend registry via `ocr_page_native`.
pub(crate) fn native_engine(name: &str, cfg: &NativeCfg) -> Result<Box<dyn OcrEngine>, String> {
    match name {
        #[cfg(feature = "tesseract")]
        "tesseract" => Ok(Box::new(crate::ocr::tesseract::TesseractEngine::from_cfg(cfg)?)),
        "server" => Ok(Box::new(ServerOcrEngine::from_cfg(cfg))),
        _ => Err(format!("unknown native OCR engine {name:?}")),
    }
}

/// Whether `name` is a native engine compiled into this build — cheap, constructs nothing,
/// so capability/`--list-ocr-engines` queries never touch a model or the C FFI.
pub(crate) fn native_engine_available(name: &str) -> bool {
    match name {
        "tesseract" => cfg!(feature = "tesseract"),
        "server" => true,
        _ => false,
    }
}

/// The native engines compiled into this build, in preference order.
pub(crate) fn native_engine_names() -> Vec<&'static str> {
    ["tesseract", "server"]
        .into_iter()
        .filter(|n| native_engine_available(n))
        .collect()
}

/// Returns pre-captured DocTags in call order. For tests / replaying a captured corpus.
pub(crate) struct ReplayEngine {
    pages: Vec<String>,
    next: AtomicUsize,
}

impl ReplayEngine {
    pub(crate) fn new(pages: Vec<String>) -> Self {
        ReplayEngine { pages, next: AtomicUsize::new(0) }
    }
}

impl OcrEngine for ReplayEngine {
    fn ocr_page(&self, _image: &[u8]) -> Result<String, String> {
        let i = self.next.fetch_add(1, Ordering::SeqCst);
        self.pages.get(i).cloned().ok_or_else(|| format!("ReplayEngine: no canned page {i}"))
    }
}

/// HTTP client for a llama.cpp / granite `llama-server` (OpenAI `/v1/chat/completions`).
pub(crate) struct ServerOcrEngine {
    pub host: String,
    pub port: u16,
    pub prompt: String,
    pub max_tokens: u32,
    pub timeout: Duration,
}

impl Default for ServerOcrEngine {
    fn default() -> Self {
        ServerOcrEngine {
            host: "localhost".into(),
            port: 8080,
            prompt: "Convert this page to docling.".into(),
            max_tokens: 4096,
            timeout: Duration::from_secs(300),
        }
    }
}

impl ServerOcrEngine {
    /// Build from the engine-agnostic `NativeCfg`, falling back to defaults for unset fields.
    pub(crate) fn from_cfg(cfg: &NativeCfg) -> Self {
        let d = ServerOcrEngine::default();
        ServerOcrEngine {
            host: cfg.host.clone().unwrap_or(d.host),
            port: cfg.port.unwrap_or(d.port),
            prompt: cfg.prompt.clone().unwrap_or(d.prompt),
            max_tokens: cfg.max_tokens.unwrap_or(d.max_tokens),
            timeout: d.timeout,
        }
    }
}

impl OcrEngine for ServerOcrEngine {
    fn ocr_page(&self, image: &[u8]) -> Result<String, String> {
        let b64 = base64::engine::general_purpose::STANDARD.encode(image);
        let body = build_request_body(&self.prompt, self.max_tokens, &b64);
        let resp = http_post(&self.host, self.port, "/v1/chat/completions", &body, self.timeout)?;
        extract_message_content(&resp).ok_or_else(|| {
            format!("no message content in response: {}", resp.chars().take(200).collect::<String>())
        })
    }
}

/// Build the OpenAI chat-completions JSON. The image is a base64 PNG data URL.
fn build_request_body(prompt: &str, max_tokens: u32, b64_png: &str) -> String {
    format!(
        concat!(
            "{{\"model\":\"granite\",\"max_tokens\":{},\"temperature\":0.0,",
            "\"messages\":[{{\"role\":\"user\",\"content\":[",
            "{{\"type\":\"image_url\",\"image_url\":{{\"url\":\"data:image/png;base64,{}\"}}}},",
            "{{\"type\":\"text\",\"text\":\"{}\"}}",
            "]}}]}}"
        ),
        max_tokens,
        b64_png,
        json_escape(prompt),
    )
}

/// Minimal HTTP/1.1 POST over TcpStream (localhost, no TLS). `Connection: close` so we
/// can read the body to EOF without chunked-transfer parsing.
fn http_post(host: &str, port: u16, path: &str, body: &str, timeout: Duration) -> Result<String, String> {
    let mut stream = TcpStream::connect((host, port)).map_err(|e| format!("connect {host}:{port}: {e}"))?;
    stream.set_read_timeout(Some(timeout)).ok();
    stream.set_write_timeout(Some(timeout)).ok();
    let req = format!(
        "POST {path} HTTP/1.1\r\nHost: {host}\r\nContent-Type: application/json\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(req.as_bytes()).map_err(|e| format!("write: {e}"))?;
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).map_err(|e| format!("read: {e}"))?;
    let text = String::from_utf8_lossy(&raw);
    // split headers / body
    let body = text.split_once("\r\n\r\n").map(|(_, b)| b).unwrap_or(&text);
    Ok(body.to_string())
}

/// JSON-escape a string for embedding in a request body.
fn json_escape(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            '\r' => o.push_str("\\r"),
            '\t' => o.push_str("\\t"),
            c if (c as u32) < 0x20 => o.push_str(&format!("\\u{:04x}", c as u32)),
            c => o.push(c),
        }
    }
    o
}

/// Pull `choices[0].message.content` out of a chat-completions JSON response by locating
/// the `"content"` key after `"message"` and decoding the JSON string that follows.
fn extract_message_content(json: &str) -> Option<String> {
    // anchor at the message object so we don't grab a "content" inside the request echo
    let from = json.find("\"message\"").unwrap_or(0);
    let rest = &json[from..];
    let key = rest.find("\"content\"")?;
    let after = &rest[key + "\"content\"".len()..];
    let colon = after.find(':')?;
    let after = &after[colon + 1..];
    let q = after.find('"')?;
    decode_json_string(&after[q + 1..])
}

/// Decode a JSON string body (starting just after the opening quote) up to the closing
/// unescaped quote, resolving `\n \t \" \\ \/ \uXXXX` escapes.
fn decode_json_string(s: &str) -> Option<String> {
    let mut out = String::new();
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' => return Some(out),
            '\\' => match chars.next()? {
                'n' => out.push('\n'),
                't' => out.push('\t'),
                'r' => out.push('\r'),
                '"' => out.push('"'),
                '\\' => out.push('\\'),
                '/' => out.push('/'),
                'b' => out.push('\u{0008}'),
                'f' => out.push('\u{000C}'),
                'u' => {
                    let hex: String = (0..4).filter_map(|_| chars.next()).collect();
                    let cp = u32::from_str_radix(&hex, 16).ok()?;
                    // handle a UTF-16 surrogate pair
                    if (0xD800..=0xDBFF).contains(&cp) {
                        // expect \uXXXX low surrogate
                        if chars.next() == Some('\\') && chars.next() == Some('u') {
                            let hex2: String = (0..4).filter_map(|_| chars.next()).collect();
                            let lo = u32::from_str_radix(&hex2, 16).ok()?;
                            let c = 0x10000 + ((cp - 0xD800) << 10) + (lo - 0xDC00);
                            out.push(char::from_u32(c)?);
                        }
                    } else {
                        out.push(char::from_u32(cp)?);
                    }
                }
                other => out.push(other),
            },
            c => out.push(c),
        }
    }
    Some(out) // unterminated — return what we have
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replay_returns_in_order() {
        let e = ReplayEngine::new(vec!["<a>".into(), "<b>".into()]);
        assert_eq!(e.ocr_page(b"x").unwrap(), "<a>");
        assert_eq!(e.ocr_page(b"x").unwrap(), "<b>");
        assert!(e.ocr_page(b"x").is_err());
    }

    #[test]
    fn json_escape_basic() {
        assert_eq!(json_escape("a\"b\\c\nd"), "a\\\"b\\\\c\\nd");
    }

    #[test]
    fn extract_content_with_escapes() {
        let resp = r#"{"choices":[{"message":{"role":"assistant","content":"<loc_1>line one\nline two \"q\""}}]}"#;
        let c = extract_message_content(resp).unwrap();
        assert_eq!(c, "<loc_1>line one\nline two \"q\"");
    }

    #[test]
    fn extract_content_unicode_escape() {
        let resp = r#"{"message":{"content":"COMISSÃO"}}"#;
        assert_eq!(extract_message_content(resp).unwrap(), "COMISSÃO");
    }

    #[test]
    fn request_body_shape() {
        let b = build_request_body("Convert this page to docling.", 4096, "QUJD");
        assert!(b.contains("data:image/png;base64,QUJD"));
        assert!(b.contains("\"max_tokens\":4096"));
        assert!(b.contains("Convert this page to docling."));
    }
}
