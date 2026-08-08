//! `function` — PDF function objects (32000-1 §7.10), evaluated.
//!
//! A PDF function is a black box `f: R^m -> R^n` that several unrelated constructs hand
//! their numbers to. Three of them matter here:
//!
//! * a **`Separation` / `DeviceN`** colour space's *tint transform*, which turns the `scn`
//!   tint a page paints with into a colour in an alternate space. Without it a `.1 scn`
//!   reads as the grey level `0.1` — near-black — where the page meant a pale spot colour;
//! * a **`Separation` image**'s per-sample tints (the same transform, per pixel);
//! * an **axial / radial shading**'s colour ramp.
//!
//! All three want one evaluator, so this module is deliberately standalone: it knows
//! nothing about colour, images or SVG, and its only input is a PDF object.
//!
//! **Types 0 (sampled), 2 (exponential) and 3 (stitching) are evaluated.** Type 1
//! (function-based, `/Domain` 2-in) and Type 4 (a PostScript calculator program) are not —
//! [`Function::parse`] returns `None` for them, which is the caller's signal to degrade on
//! its own terms rather than to invent a number. An *array* of 1-output functions standing
//! in for one n-output function (legal wherever a function is expected) is supported,
//! because shadings routinely write one.
//!
//! Every entry point is total: malformed input yields `None` or a clamped value, never a
//! panic, an unbounded loop, or an allocation sized by the file. The caps below are the
//! whole of the hostile-input story — a function is parsed from attacker-controlled bytes.

use crate::access::{read_resolved, DocumentAccess};
use crate::pdfobj::{content_bytes, num_resolved};
use lopdf::{Dictionary, Object};

/// Nesting cap for a Type 3 stitching function whose sub-functions are themselves Type 3
/// (or an array of arrays). Without it a self-referential `/Functions` entry recurses until
/// the stack dies. Declared in `lib.rs` with the crate's other DoS caps.
use crate::MAX_FN_DEPTH;
/// A function of more than this many inputs is not one we can serve: the sampled
/// interpolator's corner count is `2^m`, and no real tint transform or shading has more
/// than a handful of colorants.
const MAX_INPUTS: usize = 8;
/// …and more outputs than any colour space has components.
const MAX_OUTPUTS: usize = 32;
/// Above this many inputs the sampled interpolator drops from multilinear (`2^m` corners)
/// to nearest-sample. Bounds the per-evaluation work; 4 inputs is already beyond every
/// tint transform in practice.
const MAX_INTERP_INPUTS: usize = 4;
/// Ceiling on a sampled function's declared grid, so an absurd `/Size` cannot make the
/// index arithmetic overflow or imply a buffer the file never carried.
const MAX_SAMPLE_GRID: usize = 1 << 22;

/// A parsed, evaluable PDF function.
pub(crate) struct Function {
    /// `/Domain` as `m` `[lo, hi]` pairs — every input is clamped into it before use.
    domain: Vec<[f32; 2]>,
    /// `/Range` as `n` `[lo, hi]` pairs. Required for Types 0 and 4, optional elsewhere;
    /// when present every output is clamped into it.
    range: Option<Vec<[f32; 2]>>,
    kind: Kind,
}

enum Kind {
    /// Type 0 — samples on a rectangular grid, multilinearly interpolated.
    Sampled(Sampled),
    /// Type 2 — `y_j = C0_j + x^N (C1_j - C0_j)`.
    Exponential { c0: Vec<f32>, c1: Vec<f32>, n: f32 },
    /// Type 3 — `k` sub-functions stitched over `k-1` interior bounds.
    Stitching { funcs: Vec<Function>, bounds: Vec<f32>, encode: Vec<[f32; 2]> },
    /// `n` one-output functions standing in for one `n`-output function.
    Array(Vec<Function>),
}

struct Sampled {
    size: Vec<usize>,
    bps: u32,
    encode: Vec<[f32; 2]>,
    decode: Vec<[f32; 2]>,
    n_out: usize,
    data: Vec<u8>,
}

/// PDF's own `Interpolate` (§7.10.2): map `x` from `[xmin, xmax]` onto `[ymin, ymax]`.
/// A degenerate source interval yields `ymin` rather than a NaN.
fn interpolate(x: f32, xmin: f32, xmax: f32, ymin: f32, ymax: f32) -> f32 {
    if xmax == xmin {
        ymin
    } else {
        ymin + (x - xmin) * (ymax - ymin) / (xmax - xmin)
    }
}

/// A numeric array as `f32`s, following indirect references (array entries may legally be
/// indirect). `None` when the key is absent or is not an array.
fn floats(access: &dyn DocumentAccess, dictionary: &Dictionary, key: &[u8]) -> Option<Vec<f32>> {
    read_resolved(access, dictionary.get(key).ok()?, |resolved| {
        Some(
            resolved
                .as_array()
                .ok()?
                .iter()
                .map(|object| num_resolved(access, object))
                .collect(),
        )
    })
    .ok()
    .flatten()
}

/// A flat `[lo, hi, lo, hi, …]` array as pairs; `None` unless the length is a positive
/// even number within `cap`.
fn pairs(v: Vec<f32>, cap: usize) -> Option<Vec<[f32; 2]>> {
    if v.is_empty() || !v.len().is_multiple_of(2) || v.len() / 2 > cap {
        return None;
    }
    Some(v.chunks_exact(2).map(|c| [c[0], c[1]]).collect())
}

impl Function {
    /// Parse a function object — a dictionary, a stream, or an array of one-output
    /// functions — following an indirect reference first.
    ///
    /// `None` means **"do not evaluate this"**, never "the answer is zero": an unsupported
    /// type (1 or 4), a malformed dictionary, or a sampled function whose stream is empty.
    /// Callers degrade on that signal; nothing here fabricates a value.
    pub(crate) fn parse(access: &dyn DocumentAccess, object: &Object) -> Option<Function> {
        Function::parse_at(access, object, 0)
    }

    fn parse_at(access: &dyn DocumentAccess, object: &Object, depth: u32) -> Option<Function> {
        if depth > MAX_FN_DEPTH {
            return None;
        }
        read_resolved(access, object, |resolved| Function::parse_direct(access, resolved, depth))
            .ok()
            .flatten()
    }

    fn parse_direct(access: &dyn DocumentAccess, object: &Object, depth: u32) -> Option<Function> {
        // An ARRAY of n one-output functions is legal wherever one n-output function is
        // (§8.7.4.5.5). Its domain is the shared domain of its members.
        if let Object::Array(a) = object {
            if a.is_empty() || a.len() > MAX_OUTPUTS {
                return None;
            }
            let funcs: Vec<Function> = a
                .iter()
                .map(|element| Function::parse_at(access, element, depth + 1))
                .collect::<Option<_>>()?;
            let domain = funcs[0].domain.clone();
            return Some(Function { domain, range: None, kind: Kind::Array(funcs) });
        }
        let dict = match object {
            Object::Dictionary(d) => d,
            Object::Stream(s) => &s.dict,
            _ => return None,
        };
        let domain = pairs(floats(access, dict, b"Domain")?, MAX_INPUTS)?;
        let range = floats(access, dict, b"Range").and_then(|v| pairs(v, MAX_OUTPUTS));
        let ftype = read_resolved(access, dict.get(b"FunctionType").ok()?, |value| value.as_i64().ok())
            .ok()
            .flatten()?;
        let kind = match ftype {
            0 => {
                let stream = match object {
                    Object::Stream(s) => s,
                    _ => return None, // a Type 0 function IS a stream; a dict alone is malformed
                };
                Kind::Sampled(Sampled::parse(access, dict, stream, &domain, range.as_ref()?)?)
            }
            2 => {
                // 1-in by definition (§7.10.3). `/C0`/`/C1` default to [0.0]/[1.0], and
                // their length is what states the output count.
                if domain.len() != 1 {
                    return None;
                }
                let c0 = floats(access, dict, b"C0").unwrap_or_else(|| vec![0.0]);
                let c1 = floats(access, dict, b"C1").unwrap_or_else(|| vec![1.0]);
                if c0.is_empty() || c0.len() != c1.len() || c0.len() > MAX_OUTPUTS {
                    return None;
                }
                let n = num_resolved(access, dict.get(b"N").ok()?);
                if !n.is_finite() {
                    return None;
                }
                Kind::Exponential { c0, c1, n }
            }
            3 => {
                if domain.len() != 1 {
                    return None;
                }
                let funcs = read_resolved(access, dict.get(b"Functions").ok()?, |value| {
                    let subs = value.as_array().ok()?;
                    if subs.is_empty() || subs.len() > MAX_OUTPUTS {
                        return None;
                    }
                    subs.iter()
                        .map(|function| Function::parse_at(access, function, depth + 1))
                        .collect::<Option<Vec<_>>>()
                })
                .ok()
                .flatten()?;
                let bounds = floats(access, dict, b"Bounds")?;
                let encode = pairs(floats(access, dict, b"Encode")?, MAX_OUTPUTS)?;
                if bounds.len() + 1 != funcs.len() || encode.len() != funcs.len() {
                    return None;
                }
                // Bounds must be non-decreasing and inside the domain, else the search
                // below picks a sub-function the file never meant.
                if bounds.windows(2).any(|w| w[1] < w[0]) {
                    return None;
                }
                Kind::Stitching { funcs, bounds, encode }
            }
            // Type 1 (function-based) and Type 4 (PostScript calculator) are not evaluated.
            _ => return None,
        };
        Some(Function { domain, range, kind })
    }

    /// Evaluate. `None` when the input arity is wrong for this function — a caller that
    /// gets `None` must degrade, exactly as it does for an unparseable function.
    ///
    /// Inputs are clamped into `/Domain` and outputs into `/Range` (both per the spec), so
    /// a hostile or merely sloppy caller cannot drive a colour out of gamut.
    pub(crate) fn eval(&self, input: &[f32]) -> Option<Vec<f32>> {
        if input.len() != self.domain.len() {
            return None;
        }
        let x: Vec<f32> = input
            .iter()
            .zip(&self.domain)
            .map(|(v, d)| {
                let (lo, hi) = (d[0].min(d[1]), d[0].max(d[1]));
                if v.is_nan() {
                    lo
                } else {
                    v.clamp(lo, hi)
                }
            })
            .collect();
        let mut out = match &self.kind {
            Kind::Sampled(s) => s.eval(&x, &self.domain)?,
            Kind::Exponential { c0, c1, n } => {
                // `x^N` is NaN for a negative base and a fractional exponent; the domain
                // clamp above is what normally prevents it, and this guard covers a file
                // that authored a negative domain anyway.
                let t = x[0];
                let p = if t < 0.0 && n.fract() != 0.0 { 0.0 } else { t.powf(*n) };
                let p = if p.is_finite() { p } else { 0.0 };
                c0.iter().zip(c1).map(|(a, b)| a + p * (b - a)).collect()
            }
            Kind::Stitching { funcs, bounds, encode } => {
                let (d0, d1) = (self.domain[0][0], self.domain[0][1]);
                let t = x[0];
                // The sub-interval `t` falls in: the first bound it is below, else the last.
                let i = bounds.iter().position(|b| t < *b).unwrap_or(bounds.len());
                let lo = if i == 0 { d0 } else { bounds[i - 1] };
                let hi = if i == bounds.len() { d1 } else { bounds[i] };
                let e = encode[i];
                funcs[i].eval(&[interpolate(t, lo, hi, e[0], e[1])])?
            }
            Kind::Array(funcs) => {
                let mut v = Vec::with_capacity(funcs.len());
                for f in funcs {
                    // Each member is one output; anything else is a malformed array.
                    v.push(*f.eval(&x)?.first()?);
                }
                v
            }
        };
        if let Some(r) = &self.range {
            for (o, b) in out.iter_mut().zip(r) {
                let (lo, hi) = (b[0].min(b[1]), b[0].max(b[1]));
                *o = if o.is_nan() { lo } else { o.clamp(lo, hi) };
            }
            out.truncate(r.len());
        }
        Some(out)
    }

    /// How many values [`Function::eval`] yields, when that is decidable from the
    /// definition alone. `None` for a Type 3 whose sub-functions disagree.
    pub(crate) fn n_outputs(&self) -> Option<usize> {
        if let Some(r) = &self.range {
            return Some(r.len());
        }
        match &self.kind {
            Kind::Sampled(s) => Some(s.n_out),
            Kind::Exponential { c0, .. } => Some(c0.len()),
            Kind::Array(f) => Some(f.len()),
            Kind::Stitching { funcs, .. } => {
                let first = funcs.first()?.n_outputs()?;
                funcs.iter().all(|f| f.n_outputs() == Some(first)).then_some(first)
            }
        }
    }
}

/// Read `bits` bits starting at `bit_off` from a big-endian bit stream. Out-of-range reads
/// yield `0` — a sampled function whose stream is a few bytes short degrades to black at
/// the tail rather than refusing the whole colour.
fn read_bits(data: &[u8], bit_off: u64, bits: u32) -> u32 {
    let end_bit = bit_off + bits as u64;
    let start_byte = (bit_off / 8) as usize;
    let end_byte = end_bit.div_ceil(8) as usize;
    if end_byte > data.len() || bits == 0 || bits > 32 {
        return 0;
    }
    let mut v: u64 = 0;
    for b in &data[start_byte..end_byte] {
        v = (v << 8) | *b as u64;
    }
    let extra = end_byte as u64 * 8 - end_bit;
    ((v >> extra) & ((1u64 << bits) - 1)) as u32
}

impl Sampled {
    fn parse(
        access: &dyn DocumentAccess,
        dict: &Dictionary,
        stream: &lopdf::Stream,
        domain: &[[f32; 2]],
        range: &[[f32; 2]],
    ) -> Option<Sampled> {
        let size: Vec<usize> = floats(access, dict, b"Size")?
            .iter()
            .map(|v| if *v >= 1.0 && *v <= MAX_SAMPLE_GRID as f32 { Some(*v as usize) } else { None })
            .collect::<Option<_>>()?;
        if size.len() != domain.len() {
            return None;
        }
        // The whole grid must be addressable without overflowing, and small enough that the
        // index arithmetic below stays in `usize` — an absurd `/Size` is refused, not trusted.
        let cells = size.iter().try_fold(1usize, |a, s| a.checked_mul(*s))?;
        let n_out = range.len();
        if cells == 0 || cells > MAX_SAMPLE_GRID || n_out == 0 || cells.checked_mul(n_out).is_none() {
            return None;
        }
        let bps = read_resolved(access, dict.get(b"BitsPerSample").ok()?, |value| value.as_i64().ok())
            .ok()
            .flatten()? as u32;
        if !matches!(bps, 1 | 2 | 4 | 8 | 12 | 16 | 24 | 32) {
            return None;
        }
        // `/Encode` defaults to [0 Size_i-1] per dimension, `/Decode` to `/Range`.
        let encode = match floats(access, dict, b"Encode").and_then(|v| pairs(v, MAX_INPUTS)) {
            Some(e) if e.len() == size.len() => e,
            _ => size.iter().map(|s| [0.0, (*s - 1) as f32]).collect(),
        };
        let decode = match floats(access, dict, b"Decode").and_then(|v| pairs(v, MAX_OUTPUTS)) {
            Some(d) if d.len() == n_out => d,
            _ => range.to_vec(),
        };
        let data = content_bytes(stream);
        if data.is_empty() {
            return None; // a sampled function with no samples has nothing to say
        }
        Some(Sampled { size, bps, encode, decode, n_out, data })
    }

    /// One raw sample, normalized to `0..=1`.
    fn sample(&self, cell: usize, j: usize) -> f32 {
        let idx = cell as u64 * self.n_out as u64 + j as u64;
        let raw = read_bits(&self.data, idx * self.bps as u64, self.bps);
        let max = if self.bps >= 32 { u32::MAX as f32 } else { ((1u64 << self.bps) - 1) as f32 };
        raw as f32 / max
    }

    /// The flat cell index of a grid coordinate. The FIRST dimension varies fastest
    /// (§7.10.2) — reading it the other way transposes every multi-input function.
    fn cell(&self, idx: &[usize]) -> usize {
        let mut flat = 0usize;
        for i in (0..idx.len()).rev() {
            flat = flat * self.size[i] + idx[i].min(self.size[i] - 1);
        }
        flat
    }

    fn eval(&self, x: &[f32], domain: &[[f32; 2]]) -> Option<Vec<f32>> {
        // §7.10.2: encode each input FROM its domain interval ONTO its `/Encode` interval,
        // then clamp to the grid. Split the result into a base cell + interpolation fraction.
        let mut base = Vec::with_capacity(x.len());
        let mut frac = Vec::with_capacity(x.len());
        for (i, v) in x.iter().enumerate() {
            let hi = (self.size[i] - 1) as f32;
            let d = domain[i];
            let e = interpolate(*v, d[0], d[1], self.encode[i][0], self.encode[i][1]).clamp(0.0, hi);
            let e = if e.is_nan() { 0.0 } else { e };
            let b = e.floor();
            base.push(b as usize);
            frac.push(e - b);
        }
        let mut out = vec![0.0f32; self.n_out];
        if x.len() <= MAX_INTERP_INPUTS {
            // Multilinear: every corner of the enclosing cell, weighted by its fraction.
            for corner in 0..(1usize << x.len()) {
                let mut w = 1.0f32;
                let mut idx = Vec::with_capacity(x.len());
                for i in 0..x.len() {
                    if corner >> i & 1 == 1 {
                        w *= frac[i];
                        idx.push((base[i] + 1).min(self.size[i] - 1));
                    } else {
                        w *= 1.0 - frac[i];
                        idx.push(base[i]);
                    }
                }
                if w == 0.0 {
                    continue;
                }
                let cell = self.cell(&idx);
                for (j, o) in out.iter_mut().enumerate() {
                    *o += w * self.sample(cell, j);
                }
            }
        } else {
            // Too many inputs to interpolate affordably: nearest sample.
            let idx: Vec<usize> = base.iter().zip(&frac).enumerate().map(|(i, (b, f))| if *f > 0.5 { (b + 1).min(self.size[i] - 1) } else { *b }).collect();
            let cell = self.cell(&idx);
            for (j, o) in out.iter_mut().enumerate() {
                *o = self.sample(cell, j);
            }
        }
        // Decode each normalized sample into its output range.
        for (j, o) in out.iter_mut().enumerate() {
            *o = interpolate(*o, 0.0, 1.0, self.decode[j][0], self.decode[j][1]);
        }
        Some(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::access::test_adapter;
    use lopdf::{dictionary, Document, Stream};

    fn parse(doc: &Document, object: &Object) -> Option<Function> {
        Function::parse(&test_adapter(doc), object)
    }

    fn nums(v: &[f32]) -> Object {
        Object::Array(v.iter().map(|x| Object::Real(*x)).collect())
    }

    fn approx(got: &[f32], want: &[f32]) {
        assert_eq!(got.len(), want.len(), "arity: {got:?} vs {want:?}");
        for (a, b) in got.iter().zip(want) {
            assert!((a - b).abs() < 1e-4, "{got:?} != {want:?}");
        }
    }

    /// A document holding one object, plus that object as a reference — so every test also
    /// exercises the indirect path a real file writes its functions with.
    fn one(o: Object) -> (Document, Object) {
        let mut doc = Document::with_version("1.5");
        let id = doc.add_object(o);
        (doc, Object::Reference(id))
    }

    /// A Type 2 exponential: `C0` -> `C1` over the unit domain with exponent `n`.
    fn exp_fn(c0: &[f32], c1: &[f32], n: f32) -> Object {
        Object::Dictionary(dictionary! {
            "FunctionType" => 2, "Domain" => nums(&[0.0, 1.0]),
            "C0" => nums(c0), "C1" => nums(c1), "N" => Object::Real(n),
        })
    }

    #[test]
    fn a_type_2_exponential_interpolates_between_its_endpoints() {
        // THE case the Separation defect turns on: a spot colour whose tint transform is a
        // straight ramp from white to a pale lavender. At tint .1 the answer is nearly
        // white — the value that used to be read as the grey level 0.1, i.e. near-BLACK.
        let pale = [0.776, 0.776, 0.878]; // (198,198,224)
        let (doc, r) = one(exp_fn(&[1.0, 1.0, 1.0], &pale, 1.0));
        let f = parse(&doc, &r).expect("a Type 2 function must parse");
        approx(&f.eval(&[0.0]).unwrap(), &[1.0, 1.0, 1.0]);
        approx(&f.eval(&[1.0]).unwrap(), &pale);
        approx(&f.eval(&[0.5]).unwrap(), &[0.888, 0.888, 0.939]);
        let t1 = f.eval(&[0.1]).unwrap();
        assert!(t1.iter().all(|v| *v > 0.9), "tint .1 must stay pale, got {t1:?}");
        assert_eq!(f.n_outputs(), Some(3));
    }

    #[test]
    fn a_type_2_exponent_is_applied_to_the_input_not_the_output() {
        // N=2 bends the ramp; reading N as a no-op (or applying it after the lerp) gives
        // 0.5 here instead of 0.25.
        let (doc, r) = one(exp_fn(&[0.0], &[1.0], 2.0));
        let f = parse(&doc, &r).unwrap();
        approx(&f.eval(&[0.5]).unwrap(), &[0.25]);
        approx(&f.eval(&[1.0]).unwrap(), &[1.0]);
        // C0/C1 default to [0.0]/[1.0] when absent (§7.10.3).
        let (doc, r) = one(Object::Dictionary(dictionary! {
            "FunctionType" => 2, "Domain" => nums(&[0.0, 1.0]), "N" => 1,
        }));
        approx(&parse(&doc, &r).unwrap().eval(&[0.25]).unwrap(), &[0.25]);
    }

    /// A Type 0 sampled function: `size` grid, `bps` bits per sample, `n_out` outputs.
    fn sampled(size: &[i64], bps: i64, n_out: usize, data: Vec<u8>, extra: Option<(&str, Object)>) -> Object {
        let mut d = dictionary! {
            "FunctionType" => 0,
            "Domain" => nums(&vec![0.0, 1.0].repeat(size.len()).chunks(2).flat_map(|c| c.to_vec()).collect::<Vec<f32>>()),
            "Range" => nums(&[0.0, 1.0].repeat(n_out)),
            "Size" => Object::Array(size.iter().map(|s| Object::Integer(*s)).collect()),
            "BitsPerSample" => bps,
        };
        if let Some((k, v)) = extra {
            d.set(k, v);
        }
        Object::Stream(Stream::new(d, data))
    }

    #[test]
    fn a_type_0_sampled_function_interpolates_between_its_samples() {
        // Two samples, 0 and 255 at 8 bits: the midpoint is the linear interpolation, not
        // the nearest sample. (A nearest-sample reading gives 0.0 or 1.0 here.)
        let (doc, r) = one(sampled(&[2], 8, 1, vec![0, 255], None));
        let f = parse(&doc, &r).expect("a Type 0 function must parse");
        approx(&f.eval(&[0.0]).unwrap(), &[0.0]);
        approx(&f.eval(&[1.0]).unwrap(), &[1.0]);
        approx(&f.eval(&[0.5]).unwrap(), &[0.5]);
        approx(&f.eval(&[0.25]).unwrap(), &[0.25]);
        assert_eq!(f.n_outputs(), Some(1));
    }

    #[test]
    fn a_type_0_function_reads_every_legal_bit_depth() {
        // Sub-byte and multi-byte samples are a bit stream, not a byte array — the depth
        // most spot-colour ramps use is 8, but 1/2/4/12/16 are all legal and all appear.
        for (bps, data) in [
            (1u32, vec![0b0100_0000u8]),      // samples 0,1 in the top two bits
            (2, vec![0b0011_0000]),           // 0, 3
            (4, vec![0x0F]),                  // 0, 15
            (8, vec![0, 255]),
            (12, vec![0x00, 0x0F, 0xFF]),     // 0x000, 0xFFF
            (16, vec![0, 0, 255, 255]),
        ] {
            let (doc, r) = one(sampled(&[2], bps as i64, 1, data, None));
            let f = parse(&doc, &r).unwrap_or_else(|| panic!("bps {bps} must parse"));
            approx(&f.eval(&[0.0]).unwrap(), &[0.0]);
            approx(&f.eval(&[1.0]).unwrap(), &[1.0]);
        }
        // 3 bits per sample is not a legal depth — refuse it rather than mis-read the stream.
        let (doc, r) = one(sampled(&[2], 3, 1, vec![0xFF], None));
        assert!(parse(&doc, &r).is_none(), "an illegal /BitsPerSample must not parse");
    }

    #[test]
    fn a_type_0_functions_first_dimension_varies_fastest() {
        // §7.10.2's sample order. Reading it the other way transposes every multi-input
        // function — a DeviceN tint transform would then swap its colorants.
        // 2x2 grid, one output: cells (0,0)=0 (1,0)=85 (0,1)=170 (1,1)=255.
        let (doc, r) = one(sampled(&[2, 2], 8, 1, vec![0, 85, 170, 255], None));
        let f = parse(&doc, &r).unwrap();
        approx(&f.eval(&[0.0, 0.0]).unwrap(), &[0.0]);
        approx(&f.eval(&[1.0, 0.0]).unwrap(), &[85.0 / 255.0]);
        approx(&f.eval(&[0.0, 1.0]).unwrap(), &[170.0 / 255.0]);
        approx(&f.eval(&[1.0, 1.0]).unwrap(), &[1.0]);
        // …and the bilinear centre is the mean of all four.
        approx(&f.eval(&[0.5, 0.5]).unwrap(), &[(0.0 + 85.0 + 170.0 + 255.0) / 4.0 / 255.0]);
        // Wrong arity is refused, not silently padded.
        assert!(f.eval(&[0.5]).is_none());
        assert!(f.eval(&[0.5, 0.5, 0.5]).is_none());
    }

    #[test]
    fn a_type_0_decode_array_remaps_the_samples() {
        // `/Decode` defaults to `/Range`; stating it inverts or rescales the ramp.
        let inv = Some(("Decode", nums(&[1.0, 0.0])));
        let (doc, r) = one(sampled(&[2], 8, 1, vec![0, 255], inv));
        let f = parse(&doc, &r).unwrap();
        approx(&f.eval(&[0.0]).unwrap(), &[1.0]);
        approx(&f.eval(&[1.0]).unwrap(), &[0.0]);
    }

    #[test]
    fn a_type_3_stitches_its_subfunctions_over_their_bounds() {
        // Two ramps stitched at 0.5: 0->1 on the left half, 1->0 on the right. Getting the
        // sub-interval search or the /Encode wrong shows up as a discontinuity at the seam.
        let mut doc = Document::with_version("1.5");
        let up = doc.add_object(exp_fn(&[0.0], &[1.0], 1.0));
        let down = doc.add_object(exp_fn(&[1.0], &[0.0], 1.0));
        let id = doc.add_object(Object::Dictionary(dictionary! {
            "FunctionType" => 3, "Domain" => nums(&[0.0, 1.0]),
            "Functions" => Object::Array(vec![Object::Reference(up), Object::Reference(down)]),
            "Bounds" => nums(&[0.5]),
            "Encode" => nums(&[0.0, 1.0, 0.0, 1.0]),
        }));
        let f = parse(&doc, &Object::Reference(id)).expect("a Type 3 must parse");
        approx(&f.eval(&[0.0]).unwrap(), &[0.0]);
        approx(&f.eval(&[0.25]).unwrap(), &[0.5]);
        approx(&f.eval(&[0.5]).unwrap(), &[1.0]); // the seam, from the RIGHT piece
        approx(&f.eval(&[0.75]).unwrap(), &[0.5]);
        approx(&f.eval(&[1.0]).unwrap(), &[0.0]);
        assert_eq!(f.n_outputs(), Some(1));
        // A /Bounds whose length does not match /Functions is malformed, not a guess.
        let bad = doc.add_object(Object::Dictionary(dictionary! {
            "FunctionType" => 3, "Domain" => nums(&[0.0, 1.0]),
            "Functions" => Object::Array(vec![Object::Reference(up), Object::Reference(down)]),
            "Bounds" => nums(&[0.3, 0.6]), "Encode" => nums(&[0.0, 1.0, 0.0, 1.0]),
        }));
        assert!(parse(&doc, &Object::Reference(bad)).is_none());
    }

    #[test]
    fn an_array_of_one_output_functions_acts_as_one_n_output_function() {
        // Legal wherever a function is expected, and what shadings routinely write.
        let mut doc = Document::with_version("1.5");
        let ids: Vec<Object> = [0.2f32, 0.4, 0.6]
            .iter()
            .map(|v| Object::Reference(doc.add_object(exp_fn(&[0.0], &[*v], 1.0))))
            .collect();
        let f = parse(&doc, &Object::Array(ids)).expect("an array of functions must parse");
        approx(&f.eval(&[1.0]).unwrap(), &[0.2, 0.4, 0.6]);
        approx(&f.eval(&[0.5]).unwrap(), &[0.1, 0.2, 0.3]);
        assert_eq!(f.n_outputs(), Some(3));
    }

    #[test]
    fn inputs_are_clamped_to_the_domain_and_outputs_to_the_range() {
        // The spec requires both clamps; without them a caller can drive a colour far out
        // of gamut (or into NaN, via `x^N` on a negative base).
        let (doc, r) = one(Object::Dictionary(dictionary! {
            "FunctionType" => 2, "Domain" => nums(&[0.0, 1.0]),
            "Range" => nums(&[0.0, 0.5]),
            "C0" => nums(&[0.0]), "C1" => nums(&[1.0]), "N" => 1,
        }));
        let f = parse(&doc, &r).unwrap();
        approx(&f.eval(&[5.0]).unwrap(), &[0.5]); // domain-clamped to 1, then range-clamped
        approx(&f.eval(&[-5.0]).unwrap(), &[0.0]);
        approx(&f.eval(&[f32::NAN]).unwrap(), &[0.0]);
        // A fractional exponent over a domain that reaches below zero must not go NaN.
        let (doc, r) = one(Object::Dictionary(dictionary! {
            "FunctionType" => 2, "Domain" => nums(&[-1.0, 1.0]),
            "C0" => nums(&[0.0]), "C1" => nums(&[1.0]), "N" => Object::Real(0.5),
        }));
        let out = parse(&doc, &r).unwrap().eval(&[-1.0]).unwrap();
        assert!(out[0].is_finite(), "a negative base with a fractional exponent must not be NaN");
    }

    #[test]
    fn an_unsupported_function_type_refuses_rather_than_guessing() {
        // Type 4 (PostScript calculator) and Type 1 (function-based) are not evaluated. The
        // `None` is load-bearing: it is what tells a caller to degrade on its own terms.
        // Answering 0.0 here would paint every Type-4 spot colour black.
        for t in [1, 4, 7] {
            let (doc, r) = one(Object::Stream(Stream::new(
                dictionary! { "FunctionType" => t, "Domain" => nums(&[0.0, 1.0]), "Range" => nums(&[0.0, 1.0]) },
                b"{ dup }".to_vec(),
            )));
            assert!(parse(&doc, &r).is_none(), "FunctionType {t} must not evaluate");
        }
        // No /Domain at all is not a function either.
        let (doc, r) = one(Object::Dictionary(dictionary! { "FunctionType" => 2, "N" => 1 }));
        assert!(parse(&doc, &r).is_none());
    }

    #[test]
    fn hostile_sampled_definitions_are_refused_before_anything_is_allocated() {
        // A zero-size sample stream says nothing …
        let (doc, r) = one(sampled(&[2], 8, 1, vec![], None));
        assert!(parse(&doc, &r).is_none(), "an empty sample stream must not parse");
        // … an absurd /Size must not be trusted to size an index …
        for size in [&[0i64][..], &[i64::MAX][..], &[1 << 20, 1 << 20][..]] {
            let (doc, r) = one(sampled(size, 8, 1, vec![1, 2, 3, 4], None));
            assert!(parse(&doc, &r).is_none(), "/Size {size:?} must be refused");
        }
        // … a /Size of the wrong arity for /Domain is malformed …
        let (doc, r) = one(sampled(&[2, 2], 8, 1, vec![0, 1, 2, 3], Some(("Domain", nums(&[0.0, 1.0])))));
        assert!(parse(&doc, &r).is_none(), "/Size must match /Domain's arity");
        // … and a stream shorter than its declared grid degrades to zero at the tail rather
        // than reading past the buffer.
        let (doc, r) = one(sampled(&[4], 8, 1, vec![255], None));
        let f = parse(&doc, &r).expect("a short stream still parses");
        approx(&f.eval(&[0.0]).unwrap(), &[1.0]);
        assert!(f.eval(&[1.0]).unwrap()[0].is_finite(), "reading past the samples must be finite");
    }

    #[test]
    fn a_self_referential_stitching_function_terminates() {
        // `/Functions [<itself>]`: without the depth cap this recurses until the stack dies.
        let mut doc = Document::with_version("1.5");
        let id = doc.add_object(Object::Null);
        doc.set_object(id, Object::Dictionary(dictionary! {
            "FunctionType" => 3, "Domain" => nums(&[0.0, 1.0]),
            "Functions" => Object::Array(vec![Object::Reference(id)]),
            "Bounds" => Object::Array(vec![]), "Encode" => nums(&[0.0, 1.0]),
        }));
        let t = std::time::Instant::now();
        assert!(parse(&doc, &Object::Reference(id)).is_none());
        assert!(t.elapsed().as_secs() < 5, "the depth cap is not bounding the parse");
        // An array of arrays nests too, and is bounded by the same cap.
        let mut deep = Object::Array(vec![exp_fn(&[0.0], &[1.0], 1.0)]);
        for _ in 0..MAX_FN_DEPTH + 2 {
            deep = Object::Array(vec![deep]);
        }
        let (doc2, _) = one(Object::Null);
        assert!(parse(&doc2, &deep).is_none(), "nesting past the cap must refuse");
    }
}
