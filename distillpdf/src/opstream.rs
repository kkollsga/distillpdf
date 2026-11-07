//! Operator-at-a-time content stream decoding.
//!
//! `lopdf::content::Content::decode` builds the whole `Vec<Operation>` before a walker sees the
//! first operator, at about 600 bytes an operator: the USGS map page's 382,527 operators cost
//! 230 MiB of AST to walk 4.7 MiB of content, and a hostile stream of `q\n` costs far more per
//! input byte. No walker in this crate looks back at an earlier operator, so none needs the list.
//!
//! [`OpReader`] hands the same operators over one at a time, holding a bounded chunk. It still
//! parses with lopdf's parser, so there is one grammar. It only finds where to cut: a position
//! just after a top-level operator, where the parser starts fresh. Most producers end an operator
//! with a line break, so it tries the first such line end past the chunk size, and scans the
//! operands properly only when that does not hold.
//!
//! **Why a cut cannot change the result.** The parser is context-free at an operator boundary.
//! Each chunk is parsed strictly, so a chunk that does not parse to its last byte is never
//! trusted: the reader then parses the whole remainder leniently, exactly as a whole-stream
//! decode would from that point, which stops at the same first bad operator. A boundary scanner
//! that disagrees with the parser therefore costs memory, never a different operator list.
//!
//! **The one difference from `Content::decode`.** An inline image with no `EI` is a hard parser
//! failure, and the whole-stream decode answers `Err`, losing every operator before it too. Here
//! the operators before the bad image have already been handed over and are kept. That is the
//! "degrade, never lose the page" rule of [`crate::walker`], applied to a malformed stream.

use lopdf::content::{Content, Operation};

/// Bytes of content decoded at a time. A chunk's AST is at most about 40× this on the densest
/// input (`q\n`), so the walk holds single-digit MiB however long the stream is.
const CHUNK_BYTES: usize = 32 * 1024;

/// How many `EI` candidates to try for an unfiltered inline image whose samples happen to hold
/// the bytes ` EI `, and the largest image region worth parsing to check one. Past either, no
/// cut is made and the remainder is decoded in one piece, which is correct and merely dearer.
const MAX_EI_CANDIDATES: usize = 8;
const MAX_INLINE_IMAGE_CHECK_BYTES: usize = 4 * 1024 * 1024;

/// Yields a content stream's operators in order. See the module docs.
pub(crate) struct OpReader<'a> {
    data: &'a [u8],
    pending: std::vec::IntoIter<Operation>,
    /// Operators handed over so far.
    yielded: usize,
    /// The parser refused the rest of the stream outright, the case where a whole-stream decode
    /// answers `Err`. Callers whose contract distinguishes that read it after the walk.
    failed: bool,
}

impl<'a> OpReader<'a> {
    pub(crate) fn new(data: &'a [u8]) -> Self {
        Self { data, pending: Vec::new().into_iter(), yielded: 0, failed: false }
    }

    /// Operators in the whole stream, consuming the rest of it without keeping any.
    ///
    /// Annotation appearances are addressed past the page's last operator, so a walk that stops
    /// early under `MAX_OPS` still needs the true total.
    pub(crate) fn finish_count(&mut self) -> usize {
        for _ in self.by_ref() {}
        self.yielded
    }

    /// True once the parser has refused the stream, see [`Self::failed`] on the struct.
    pub(crate) fn failed(&self) -> bool {
        self.failed
    }

    fn refill(&mut self) {
        while self.pending.len() == 0 && !self.data.is_empty() {
            let mut parsed = None;
            if self.data.len() > CHUNK_BYTES {
                // Line ends first: scanning every operand costs 5-10% of the walk on dense pages.
                // The strict parse is what makes the guess safe, and it refuses a line end that
                // fell inside a string, an array or a dictionary.
                let data = self.data;
                let strict = |cut: usize| Content::decode_strict(&data[..cut]).ok().map(|content| (cut, content));
                let found = line_end_boundary(data, CHUNK_BYTES)
                    .and_then(strict)
                    .or_else(|| op_boundary(data, CHUNK_BYTES).and_then(strict));
                if let Some((cut, content)) = found {
                    self.data = &data[cut..];
                    parsed = Some(content);
                }
            }
            let content = parsed.or_else(|| {
                let rest = std::mem::take(&mut self.data);
                Content::decode(rest).ok().or_else(|| {
                    // Refused outright. What precedes the bad image still stands.
                    self.failed = true;
                    Content::decode_strict(&rest[..op_boundary(rest, usize::MAX)?]).ok()
                })
            });
            self.pending = content.map(|content| content.operations).unwrap_or_default().into_iter();
        }
    }
}

impl Iterator for OpReader<'_> {
    type Item = Operation;

    fn next(&mut self) -> Option<Operation> {
        if self.pending.len() == 0 {
            self.refill();
        }
        let op = self.pending.next()?;
        self.yielded += 1;
        Some(op)
    }
}

fn is_ws(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\r' | b'\n')
}

fn is_op_char(byte: u8) -> bool {
    byte.is_ascii_alphabetic() || matches!(byte, b'*' | b'\'' | b'"')
}

/// A name's body runs until PDF whitespace or a delimiter, as lopdf's `name` parser reads it.
fn is_name_end(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\n' | b'\r' | 0 | 0x0C | b'(' | b')' | b'<' | b'>' | b'[' | b']' | b'{' | b'}' | b'/' | b'%')
}

/// Past the end of the literal string opening at `i`, `None` when it never closes.
fn skip_literal(data: &[u8], mut i: usize) -> Option<usize> {
    let mut depth = 0usize;
    while i < data.len() {
        match data[i] {
            b'\\' => i += 1,
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i + 1);
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// One step of the operand lexer shared by the two scans below: at `i` in `data`, either consume
/// one non-operator token and return where it ends, or return `None` when `data[i]` starts an
/// operator-shaped word. `depth` tracks arrays and dictionaries. A string that never closes ends
/// the scan by jumping to the end of the data, so no cut is proposed after it.
fn skip_operand(data: &[u8], i: usize, depth: &mut usize) -> Option<usize> {
    let len = data.len();
    Some(match data[i] {
        byte if is_ws(byte) => i + 1,
        b'%' => data[i..].iter().position(|&b| b == b'\r' || b == b'\n').map_or(len, |p| i + p + 1),
        b'(' => skip_literal(data, i).unwrap_or(len),
        b'<' if data.get(i + 1) == Some(&b'<') => {
            *depth += 1;
            i + 2
        }
        b'<' => data[i..].iter().position(|&b| b == b'>').map_or(len, |p| i + p + 1),
        b'>' if data.get(i + 1) == Some(&b'>') => {
            *depth = depth.saturating_sub(1);
            i + 2
        }
        b'[' => {
            *depth += 1;
            i + 1
        }
        b']' => {
            *depth = depth.saturating_sub(1);
            i + 1
        }
        b'/' => {
            let mut end = i + 1;
            while end < len && !is_name_end(data[end]) {
                end += 1;
            }
            end
        }
        byte if is_op_char(byte) => return None,
        _ => i + 1,
    })
}

/// How far past the chunk size to look for an operator at a line end.
const LINE_END_WINDOW: usize = 4096;

/// A guess at a boundary, for the strict parse to confirm: the first line break at least `min_len`
/// into `data` that follows an operator-shaped character. Only inline image samples could make a
/// wrong guess parse, as data the parser would read another way, so a window holding `BI` makes
/// no guess at all and leaves it to [`op_boundary`].
fn line_end_boundary(data: &[u8], min_len: usize) -> Option<usize> {
    let limit = data.len().min(min_len + LINE_END_WINDOW);
    if data[..limit].windows(2).any(|pair| pair == b"BI") {
        return None;
    }
    (min_len..limit).find(|&i| matches!(data[i], b'\n' | b'\r') && is_op_char(data[i - 1])).map(|i| i + 1)
}

/// The first position at least `min_len` into `data` that sits just after a top-level operator
/// and the whitespace following it. `None` when no such position exists before the end.
fn op_boundary(data: &[u8], min_len: usize) -> Option<usize> {
    let mut i = 0;
    let mut depth = 0usize;
    while i < data.len() {
        if let Some(next) = skip_operand(data, i, &mut depth) {
            i = next;
            continue;
        }
        let start = i;
        while i < data.len() && is_op_char(data[i]) {
            i += 1;
        }
        let word = &data[start..i];
        // The parser reads these three as operands, by prefix.
        if matches!(word, b"true" | b"false" | b"null") || depth > 0 {
            continue;
        }
        if word == b"BI" {
            // An image with no end fails the whole parse, so what comes before it must be cut
            // off from it, however short.
            match after_inline_image(data, i) {
                Some(end) => i = end,
                None => return (start > 0).then_some(start),
            }
        }
        while i < data.len() && is_ws(data[i]) {
            i += 1;
        }
        if i >= min_len {
            return (i < data.len()).then_some(i);
        }
    }
    None
}

/// Where the inline image whose `BI` ends at `after_bi` stops, past its `EI`. Mirrors the
/// parser's two readings: a filtered or otherwise unreadable image ends at the first
/// whitespace-delimited `EI`, and a raw one at its computed length, which can run past `EI`-like
/// bytes in its samples. Each raw candidate is therefore confirmed by parsing the region.
fn after_inline_image(data: &[u8], after_bi: usize) -> Option<usize> {
    let bi = after_bi - 2;
    let mut i = after_bi;
    let mut depth = 0usize;
    let samples = loop {
        if i >= data.len() {
            return None;
        }
        if let Some(next) = skip_operand(data, i, &mut depth) {
            i = next;
            continue;
        }
        let word = i;
        while i < data.len() && is_op_char(data[i]) {
            i += 1;
        }
        if &data[word..i] == b"ID" && depth == 0 {
            while i < data.len() && is_ws(data[i]) {
                i += 1;
            }
            break i;
        }
    };
    let dict = &data[after_bi..samples];
    let filtered = dict.windows(2).enumerate().any(|(p, pair)| {
        pair == b"/F" && dict.get(p + 2).is_none_or(|&b| is_name_end(b) || dict[p + 2..].starts_with(b"ilter"))
    });
    let mut candidates = data[samples..]
        .windows(4)
        .enumerate()
        .filter(|(_, w)| is_ei_window(w))
        .map(|(p, _)| samples + p + 3);
    let first = candidates.next()?;
    if filtered {
        return Some(first);
    }
    for end in std::iter::once(first).chain(candidates).take(MAX_EI_CANDIDATES) {
        if end - bi > MAX_INLINE_IMAGE_CHECK_BYTES {
            return None;
        }
        // The parser's own reading of the region decides: one `BI` carrying its samples means
        // the computed length ended exactly here.
        if let Ok(content) = Content::decode_strict(&data[bi..end]) {
            if let [op] = content.operations.as_slice() {
                if matches!(op.operands.first(), Some(lopdf::Object::Stream(_))) {
                    return Some(end);
                }
            }
        }
    }
    Some(first)
}

fn is_ei_window(window: &[u8]) -> bool {
    matches!(window[0], b' ' | b'\n' | b'\r') && &window[1..3] == b"EI" && matches!(window[3], b' ' | b'\n' | b'\r')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn whole(data: &[u8]) -> Vec<Operation> {
        Content::decode(data).map(|content| content.operations).unwrap_or_default()
    }

    fn streamed(data: &[u8]) -> Vec<Operation> {
        OpReader::new(data).collect()
    }

    fn assert_same(data: &[u8]) {
        let (a, b) = (whole(data), streamed(data));
        assert_eq!(a.len(), b.len(), "operator count");
        for (i, (x, y)) in a.iter().zip(&b).enumerate() {
            assert_eq!(x.operator, y.operator, "operator {i}");
            assert_eq!(format!("{:?}", x.operands), format!("{:?}", y.operands), "operands of {i}");
        }
    }

    fn repeated(unit: &[u8], bytes: usize) -> Vec<u8> {
        unit.iter().copied().cycle().take(bytes - bytes % unit.len()).collect()
    }

    /// Catches a cut inside a string, array or dictionary: every shape a boundary must not split,
    /// spread across several chunks, must decode to the whole-stream operator list.
    #[test]
    fn a_long_stream_decodes_to_the_same_operators_in_chunks() {
        let unit = b"q 1 0 0 1 72.5 -3 cm BT /F1 12 Tf (a \\) string (nested) op Tj) Tj [(x) -5 (y) <48 65>] TJ ET\n<< /K [1 2] /N /v#20 >> /T BDC EMC true false null Q % note Tj\n";
        assert_same(&repeated(unit, CHUNK_BYTES * 5 + 17));
    }

    /// Catches a guessed line end that is not an operator's: inside a string, an array or a
    /// dictionary a line break after a word looks exactly like one.
    #[test]
    fn a_line_end_inside_a_string_is_not_a_boundary() {
        assert_same(&repeated(b"BT (one word\ntwo words\n) Tj ET\n", CHUNK_BYTES * 3 + 9));
        assert_same(&repeated(b"[(one\n) -5 (two\n)] TJ\n", CHUNK_BYTES * 3 + 9));
        assert_same(&repeated(b"/T <</K word\n/L true\n>> BDC\nEMC\n", CHUNK_BYTES * 3 + 9));
        // A long run with no operator at all at the guessed position.
        let mut long = repeated(b"q Q\n", CHUNK_BYTES - 8);
        long.extend_from_slice(b"(");
        long.extend_from_slice(&repeated(b"words\n", CHUNK_BYTES));
        long.extend_from_slice(b") Tj\n");
        long.extend_from_slice(&repeated(b"q Q\n", CHUNK_BYTES));
        assert_same(&long);
    }

    /// Catches a boundary found inside the samples of an inline image, whether the image is raw
    /// (ended by its length, with false `EI`s inside) or filtered (ended by the first `EI`).
    #[test]
    fn inline_images_survive_chunking() {
        let mut raw = b"q BI /W 8 /H 2 /BPC 8 /CS /Gray ID ".to_vec();
        raw.extend_from_slice(b"a EI b w");
        raw.extend_from_slice(b"12345678");
        raw.extend_from_slice(b" EI Q\n");
        assert_same(&repeated(&raw, CHUNK_BYTES * 3 + 5));

        let filtered = b"q BI /W 8 /H 2 /BPC 8 /CS /Gray /F /AHx ID 00 FF> EI Q\n0 0 1 rg\n";
        assert_same(&repeated(filtered, CHUNK_BYTES * 3 + 5));

        let unknown_space = b"q BI /W 1 /H 1 /BPC 8 /CS /Mine ID x EI Q\n";
        assert_same(&repeated(unknown_space, CHUNK_BYTES * 3 + 5));
    }

    /// Catches the reader going past a bad operator: a whole-stream decode stops at the first one
    /// and keeps what came before, so a chunked read must stop in the same place.
    #[test]
    fn a_bad_operator_stops_the_stream_where_a_whole_decode_stops() {
        let mut data = repeated(b"q 1 0 0 1 0 0 cm Q\n", CHUNK_BYTES * 2);
        data.extend_from_slice(b"0 0 1 rg @ bad \n");
        data.extend_from_slice(&repeated(b"q Q\n", CHUNK_BYTES));
        assert_same(&data);

        let mut unbalanced = repeated(b"q Q\n", CHUNK_BYTES + 8);
        unbalanced.extend_from_slice(b"(never closed Tj q Q\n");
        unbalanced.extend_from_slice(&repeated(b"q Q\n", CHUNK_BYTES));
        assert_same(&unbalanced);
    }

    /// Catches an inline image with no `EI` losing the operators before it. A whole-stream decode
    /// answers nothing at all for this input; the reader keeps what the walk has already seen.
    #[test]
    fn an_unterminated_inline_image_keeps_the_operators_before_it() {
        let mut data = repeated(b"q Q\n", CHUNK_BYTES * 2);
        let good = whole(&data).len();
        data.extend_from_slice(b"BI /W 1 /H 1 ID xx");
        assert!(whole(&data).is_empty(), "the whole-stream decode loses the page");
        assert_eq!(streamed(&data).len(), good);

        // A stream too short to chunk, and one whose image falls just past a chunk boundary.
        assert_eq!(streamed(b"q Q\nBI /W 1 /H 1 ID xx").len(), 2);
        let mut near = repeated(b"q Q\n", CHUNK_BYTES + 400);
        let good = whole(&near).len();
        near.extend_from_slice(b"BI /W 1 /H 1 ID xx");
        let mut reader = OpReader::new(&near);
        assert_eq!(reader.by_ref().count(), good);
        assert!(reader.failed(), "the caller can still tell the stream did not parse");
    }

    /// Catches the reader materialising the stream anyway: a dense stream must never hold more
    /// than a chunk's worth of operators at once.
    #[test]
    fn the_reader_holds_one_chunk_of_operators() {
        let data = repeated(b"q\n", CHUNK_BYTES * 16);
        let mut reader = OpReader::new(&data);
        let mut most = 0;
        let mut seen = 0;
        while reader.next().is_some() {
            seen += 1;
            most = most.max(reader.pending.len());
        }
        assert_eq!(seen, data.len() / 2);
        assert!(most <= CHUNK_BYTES, "held {most} operators");
    }

    /// Catches `finish_count` disagreeing with the list length the appearance addressing used.
    #[test]
    fn finish_count_is_the_whole_stream_length() {
        let data = repeated(b"q Q\n", CHUNK_BYTES * 3);
        let mut reader = OpReader::new(&data);
        reader.by_ref().take(5).for_each(drop);
        assert_eq!(reader.finish_count(), whole(&data).len());
    }

    /// Catches the scanner panicking or spinning on bytes no producer writes.
    #[test]
    fn garbage_does_not_panic() {
        let mut seed = 0x2545_f491_4f6c_dd1du64;
        let mut data = Vec::with_capacity(CHUNK_BYTES * 2);
        for _ in 0..CHUNK_BYTES * 2 {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            data.push(b"()<>[]/%\\ BIEDq\n1.-"[(seed % 19) as usize]);
        }
        let _ = streamed(&data);
        let _ = streamed(&data[..CHUNK_BYTES + 3]);
        let _ = streamed(b"BI");
        let _ = streamed(b"");
        let _ = op_boundary(b"BI", 0);
        let _ = op_boundary(b"BI /W 1 ID", 0);
        let _ = op_boundary(b"(", 0);
    }
}
