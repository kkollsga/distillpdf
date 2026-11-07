//! Operator-at-a-time content stream decoding.
//!
//! `lopdf::content::Content::decode` builds the whole `Vec<Operation>` before a walker sees the
//! first operator, at about 600 bytes an operator: the USGS map page's 382,527 operators cost
//! 230 MiB of AST to walk 4.7 MiB of content, and a hostile stream of `q\n` costs far more per
//! input byte. No walker in this crate looks back at an earlier operator, so none needs the list.
//!
//! [`OpReader`] hands the same operators over one at a time from `Content::decode_iter`, the fork's
//! parser run as an iterator, so nothing but the current operator is held.
//!
//! **The one difference from `Content::decode`.** An inline image with no `EI` is a hard parser
//! failure, and the whole-stream decode answers `Err`, losing every operator before it too. Here
//! the operators before the bad image have already been handed over and are kept. That is the
//! "degrade, never lose the page" rule of [`crate::walker`], applied to a malformed stream.

use lopdf::content::{Content, Operation};

/// Yields a content stream's operators in order. See the module docs.
pub(crate) struct OpReader<'a> {
    operations: Box<dyn Iterator<Item = lopdf::Result<Operation>> + 'a>,
    /// Operators handed over so far.
    yielded: usize,
    /// The parser refused the rest of the stream outright, the case where a whole-stream decode
    /// answers `Err`. Callers whose contract distinguishes that read it after the walk.
    failed: bool,
}

impl<'a> OpReader<'a> {
    pub(crate) fn new(data: &'a [u8]) -> Self {
        Self { operations: Box::new(Content::decode_iter(data)), yielded: 0, failed: false }
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
}

impl Iterator for OpReader<'_> {
    type Item = Operation;

    fn next(&mut self) -> Option<Operation> {
        match self.operations.next()? {
            Ok(op) => {
                self.yielded += 1;
                Some(op)
            }
            Err(_) => {
                self.failed = true;
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Long enough that a reader working in chunks of a page's usual size would have to cut.
    const CHUNK_BYTES: usize = 32 * 1024;

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

    /// Catches `finish_count` disagreeing with the list length the appearance addressing used.
    #[test]
    fn finish_count_is_the_whole_stream_length() {
        let data = repeated(b"q Q\n", CHUNK_BYTES * 3);
        let mut reader = OpReader::new(&data);
        reader.by_ref().take(5).for_each(drop);
        assert_eq!(reader.finish_count(), whole(&data).len());
    }

    /// Catches the reader panicking or spinning on bytes no producer writes.
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
    }

    /// Catches an operator with a digit in it being cut: `d0` and `d1` are operators, and a scanner
    /// that reads words as letters only split them from what follows.
    #[test]
    fn operators_with_digits_read_the_same() {
        assert_same(&repeated(b"500 0 d0 q Q\n1 2 3 4 5 6 d1\n", CHUNK_BYTES * 3));
    }
}
