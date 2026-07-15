//! PDF function evaluation (ISO 32000-1 §7.10).
//!
//! Supports all four function types: 0 (sampled), 2 (exponential
//! interpolation), 3 (stitching) and 4 (PostScript calculator). Used for
//! Separation/DeviceN tint transforms, shading color functions and transfer
//! functions.
//!
//! Construction is decoupled from PDF object resolution: the caller supplies
//! a resolver closure that maps an [`ObjectId`] to the resolved object plus
//! (for streams) the decoded stream data, so this crate stays independent of
//! the parser.

use zpdf_core::{ObjectId, PdfDict, PdfObject};

/// Maximum nesting for stitching sub-functions (guards cyclic references).
const MAX_FUNCTION_DEPTH: u32 = 8;
/// Sampled functions require `2^n` interpolation corners. Above this limit
/// evaluation would be prohibitively expensive; older code also silently
/// ignored dimensions above 16.
const MAX_SAMPLED_INPUTS: usize = 16;
const MAX_SAMPLED_INTERPOLATIONS: u64 = 65_536;
/// Bound arrays copied out of a PDF object and per-evaluation output buffers.
const MAX_FUNCTION_COMPONENTS: usize = 256;
/// Execution-step budget for the Type 4 calculator (guards runaway loops —
/// the language has no loops, but nested procedures can blow up).
const MAX_PS_STEPS: u32 = 10_000;

/// A parsed, evaluatable PDF function.
#[derive(Debug, Clone)]
pub struct PdfFunction {
    domain: Vec<[f64; 2]>,
    /// Output clamp ranges; required for types 0 and 4, optional otherwise.
    range: Option<Vec<[f64; 2]>>,
    kind: FunctionKind,
}

#[derive(Debug, Clone)]
enum FunctionKind {
    /// Type 0 — sampled.
    Sampled {
        size: Vec<u32>,
        bits_per_sample: u8,
        encode: Vec<[f64; 2]>,
        decode: Vec<[f64; 2]>,
        n_outputs: usize,
        samples: Vec<u8>,
    },
    /// Type 2 — exponential interpolation between C0 and C1.
    Exponential { c0: Vec<f64>, c1: Vec<f64>, n: f64 },
    /// Type 3 — 1-in stitching of k sub-functions.
    Stitching {
        functions: Vec<PdfFunction>,
        bounds: Vec<f64>,
        encode: Vec<[f64; 2]>,
    },
    /// Type 4 — PostScript calculator program (token stream).
    PostScript { program: Vec<PsOp> },
}

/// Resolver supplied by the caller: returns the resolved object and, when the
/// target is a stream, its decoded data.
pub type Resolver<'a> = dyn FnMut(ObjectId) -> Option<(PdfObject, Option<Vec<u8>>)> + 'a;

impl PdfFunction {
    /// Parse a function from a (resolved) dict plus optional decoded stream
    /// data (required for types 0 and 4). `resolve` is used for indirect
    /// sub-functions of stitching functions.
    pub fn parse(
        dict: &PdfDict,
        stream_data: Option<&[u8]>,
        resolve: &mut Resolver<'_>,
    ) -> Option<PdfFunction> {
        Self::parse_depth(dict, stream_data, resolve, 0)
    }

    /// Convenience: parse from any object shape — dict, stream-backed ref, or
    /// direct dict-with-data pair already produced by the caller's resolver.
    pub fn parse_object(obj: &PdfObject, resolve: &mut Resolver<'_>) -> Option<PdfFunction> {
        Self::parse_object_depth(obj, resolve, 0)
    }

    fn parse_object_depth(
        obj: &PdfObject,
        resolve: &mut Resolver<'_>,
        depth: u32,
    ) -> Option<PdfFunction> {
        match obj {
            PdfObject::Ref(id) => {
                let (resolved, data) = resolve(*id)?;
                let dict = resolved.as_dict().ok()?;
                Self::parse_depth(dict, data.as_deref(), resolve, depth)
            }
            PdfObject::Dict(d) => Self::parse_depth(d, None, resolve, depth),
            _ => None,
        }
    }

    fn parse_depth(
        dict: &PdfDict,
        stream_data: Option<&[u8]>,
        resolve: &mut Resolver<'_>,
        depth: u32,
    ) -> Option<PdfFunction> {
        if depth > MAX_FUNCTION_DEPTH {
            return None;
        }
        let ftype = dict.get_i64("FunctionType").ok()?;
        let domain = number_pairs(dict.get("Domain")?)?;
        if domain.is_empty() || domain.len() > MAX_FUNCTION_COMPONENTS || !ordered_pairs(&domain) {
            return None;
        }
        let range = match dict.get("Range") {
            Some(obj) => {
                let pairs = number_pairs(obj)?;
                if pairs.is_empty()
                    || pairs.len() > MAX_FUNCTION_COMPONENTS
                    || !ordered_pairs(&pairs)
                {
                    return None;
                }
                Some(pairs)
            }
            None => None,
        };

        let kind = match ftype {
            0 => {
                let data = stream_data?;
                let size: Vec<u32> = numbers(dict.get("Size")?)?
                    .into_iter()
                    .map(|v| {
                        (v >= 1.0 && v <= u32::MAX as f64 && v.fract() == 0.0).then_some(v as u32)
                    })
                    .collect::<Option<_>>()?;
                if size.len() != domain.len() || size.len() > MAX_SAMPLED_INPUTS {
                    return None;
                }
                let bps = dict.get_i64("BitsPerSample").ok()?;
                if ![1, 2, 4, 8, 12, 16, 24, 32].contains(&bps) {
                    return None;
                }
                let range = range.as_ref()?;
                let n_outputs = range.len();
                let corners = 1u64.checked_shl(u32::try_from(size.len()).ok()?)?;
                if corners.checked_mul(u64::try_from(n_outputs).ok()?)? > MAX_SAMPLED_INTERPOLATIONS
                {
                    return None;
                }
                let encode = match dict.get("Encode") {
                    Some(obj) => number_pairs(obj)?,
                    None => size.iter().map(|&s| [0.0, (s - 1) as f64]).collect(),
                };
                let decode = match dict.get("Decode") {
                    Some(obj) => number_pairs(obj)?,
                    None => range.clone(),
                };
                if encode.len() != size.len() || decode.len() != n_outputs {
                    return None;
                }
                // Sanity: enough sample bits for the full grid.
                let sample_count = size
                    .iter()
                    .try_fold(1u64, |total, &s| total.checked_mul(s as u64))?;
                let total_bits = sample_count
                    .checked_mul(u64::try_from(n_outputs).ok()?)?
                    .checked_mul(bps as u64)?;
                let required_bytes = usize::try_from(total_bits.checked_add(7)? / 8).ok()?;
                if required_bytes > data.len() {
                    return None;
                }
                FunctionKind::Sampled {
                    size,
                    bits_per_sample: bps as u8,
                    encode,
                    decode,
                    n_outputs,
                    // Do not retain arbitrary trailing stream garbage for the
                    // lifetime of the function/document.
                    samples: data[..required_bytes].to_vec(),
                }
            }
            2 => {
                if domain.len() != 1 {
                    return None;
                }
                let c0 = dict
                    .get("C0")
                    .and_then(numbers)
                    .unwrap_or_else(|| vec![0.0]);
                let c1 = dict
                    .get("C1")
                    .and_then(numbers)
                    .unwrap_or_else(|| vec![1.0]);
                if c0.len() != c1.len()
                    || c0.is_empty()
                    || c0.len() > MAX_FUNCTION_COMPONENTS
                    || range.as_ref().is_some_and(|r| r.len() != c0.len())
                {
                    return None;
                }
                let n = dict.get_f64("N").unwrap_or(1.0);
                if !n.is_finite() {
                    return None;
                }
                FunctionKind::Exponential { c0, c1, n }
            }
            3 => {
                if domain.len() != 1 {
                    return None;
                }
                let funcs_obj = dict.get("Functions")?;
                let resolved_funcs;
                let funcs_arr: &[PdfObject] = match funcs_obj {
                    PdfObject::Array(a) => a,
                    PdfObject::Ref(id) => {
                        (resolved_funcs, _) = resolve(*id)?;
                        resolved_funcs.as_array().ok()?
                    }
                    _ => return None,
                };
                if funcs_arr.is_empty() || funcs_arr.len() > MAX_FUNCTION_COMPONENTS {
                    return None;
                }
                let mut functions = Vec::with_capacity(funcs_arr.len());
                for f in funcs_arr {
                    functions.push(Self::parse_object_depth(f, resolve, depth + 1)?);
                }
                let bounds = match dict.get("Bounds") {
                    Some(obj) => numbers(obj)?,
                    None => Vec::new(),
                };
                if bounds.len() + 1 != functions.len() {
                    return None;
                }
                let [domain_min, domain_max] = domain[0];
                if !bounds.windows(2).all(|w| w[0] < w[1])
                    || !bounds
                        .iter()
                        .all(|&bound| domain_min <= bound && bound <= domain_max)
                {
                    return None;
                }
                let encode = number_pairs(dict.get("Encode")?)?;
                if encode.len() != functions.len() {
                    return None;
                }
                let outputs = functions[0].n_outputs()?;
                if functions
                    .iter()
                    .any(|f| f.n_inputs() != 1 || f.n_outputs() != Some(outputs))
                    || range.as_ref().is_some_and(|r| r.len() != outputs)
                {
                    return None;
                }
                FunctionKind::Stitching {
                    functions,
                    bounds,
                    encode,
                }
            }
            4 => {
                let src = stream_data?;
                let program = parse_postscript(src)?;
                range.as_ref()?;
                FunctionKind::PostScript { program }
            }
            _ => return None,
        };

        Some(PdfFunction {
            domain,
            range,
            kind,
        })
    }

    /// Number of input values the function expects.
    pub fn n_inputs(&self) -> usize {
        self.domain.len()
    }

    /// Number of output values, when statically known.
    pub fn n_outputs(&self) -> Option<usize> {
        match &self.kind {
            FunctionKind::Sampled { n_outputs, .. } => Some(*n_outputs),
            FunctionKind::Exponential { c0, .. } => Some(c0.len()),
            FunctionKind::Stitching { functions, .. } => functions[0].n_outputs(),
            FunctionKind::PostScript { .. } => self.range.as_ref().map(|r| r.len()),
        }
    }

    /// Evaluate the function. Inputs are clamped to the domain; outputs are
    /// clamped to the range when one is present. Returns `None` on arity
    /// mismatch or calculator error.
    pub fn eval(&self, inputs: &[f64]) -> Option<Vec<f64>> {
        if inputs.len() != self.domain.len() {
            return None;
        }
        let clamped: Vec<f64> = inputs
            .iter()
            .zip(&self.domain)
            .map(|(&v, d)| v.clamp(d[0].min(d[1]), d[0].max(d[1])))
            .collect();

        let mut out = match &self.kind {
            FunctionKind::Sampled {
                size,
                bits_per_sample,
                encode,
                decode,
                n_outputs,
                samples,
            } => eval_sampled(
                &clamped,
                &self.domain,
                size,
                *bits_per_sample,
                encode,
                decode,
                *n_outputs,
                samples,
            )?,
            FunctionKind::Exponential { c0, c1, n } => {
                let x = clamped[0];
                let xn = if *n == 1.0 { x } else { x.powf(*n) };
                c0.iter().zip(c1).map(|(&a, &b)| a + xn * (b - a)).collect()
            }
            FunctionKind::Stitching {
                functions,
                bounds,
                encode,
            } => {
                let x = clamped[0];
                let [d0, d1] = self.domain[0];
                // Select sub-function k: x in [bounds[k-1], bounds[k]).
                let mut k = bounds.partition_point(|&b| b <= x);
                // Domain edge: x == Domain0 belongs to the first interval even
                // if bounds[0] == Domain0.
                if x <= d0 {
                    k = 0;
                }
                k = k.min(functions.len() - 1);
                let lo = if k == 0 { d0 } else { bounds[k - 1] };
                let hi = if k == functions.len() - 1 {
                    d1
                } else {
                    bounds[k]
                };
                let [e0, e1] = encode[k];
                let t = if (hi - lo).abs() < f64::EPSILON {
                    e0
                } else {
                    e0 + (x - lo) / (hi - lo) * (e1 - e0)
                };
                functions[k].eval(&[t])?
            }
            FunctionKind::PostScript { program } => eval_postscript(program, &clamped)?,
        };

        // `/Range` is optional for Type 2/3 functions, but their arithmetic can
        // still produce NaN or infinity (for example 0^-1 or a fractional power
        // of a negative domain value). Never let non-finite color components
        // escape merely because there is no range to clamp them against.
        if out.iter().any(|value| !value.is_finite()) {
            return None;
        }

        if let Some(range) = &self.range {
            if out.len() > range.len() {
                if matches!(self.kind, FunctionKind::PostScript { .. }) {
                    // PLRM: the results are the TOP n values of the stack.
                    let excess = out.len() - range.len();
                    out.drain(0..excess);
                } else {
                    out.truncate(range.len());
                }
            }
            if out.len() < range.len() {
                return None;
            }
            for (v, r) in out.iter_mut().zip(range) {
                *v = v.clamp(r[0].min(r[1]), r[0].max(r[1]));
            }
        }
        Some(out)
    }
}

// ---------------------------------------------------------------------------
// Type 0 — sampled functions
// ---------------------------------------------------------------------------

/// Read sample index `idx` (in samples, not bytes) of width `bps` bits.
fn read_sample(samples: &[u8], idx: u64, bps: u8) -> f64 {
    let Some(bit) = idx.checked_mul(bps as u64) else {
        return 0.0;
    };
    let max = (1u64 << bps) - 1;
    if bps.is_multiple_of(8) {
        let Ok(byte) = usize::try_from(bit / 8) else {
            return 0.0;
        };
        let width = (bps / 8) as usize;
        let Some(chunk) = byte
            .checked_add(width)
            .and_then(|end| samples.get(byte..end))
        else {
            return 0.0;
        };
        let acc = chunk
            .iter()
            .fold(0u64, |value, &next| (value << 8) | next as u64);
        return acc as f64 / max as f64;
    }
    let mut acc: u64 = 0;
    for i in 0..bps as u64 {
        let b = bit + i;
        let byte = (b / 8) as usize;
        if byte >= samples.len() {
            return 0.0;
        }
        let bitval = (samples[byte] >> (7 - (b % 8))) & 1;
        acc = (acc << 1) | bitval as u64;
    }
    acc as f64 / max as f64
}

#[allow(clippy::too_many_arguments)]
fn eval_sampled(
    inputs: &[f64],
    domain: &[[f64; 2]],
    size: &[u32],
    bps: u8,
    encode: &[[f64; 2]],
    decode: &[[f64; 2]],
    n_outputs: usize,
    samples: &[u8],
) -> Option<Vec<f64>> {
    let m = inputs.len();
    if size.len() != m || encode.len() < m || decode.len() < n_outputs {
        return None;
    }

    // Map each input to a fractional grid coordinate.
    let mut coord = Vec::with_capacity(m);
    for i in 0..m {
        let [d0, d1] = domain[i];
        let [e0, e1] = encode[i];
        let t = if (d1 - d0).abs() < f64::EPSILON {
            e0
        } else {
            e0 + (inputs[i] - d0) / (d1 - d0) * (e1 - e0)
        };
        coord.push(t.clamp(0.0, (size[i] - 1) as f64));
    }

    // Multilinear interpolation over the 2^m surrounding grid points.
    let mut out = vec![0.0f64; n_outputs];
    let corners = 1usize.checked_shl(u32::try_from(m).ok()?)?;
    for corner in 0..corners {
        let mut weight = 1.0f64;
        let mut index: u64 = 0;
        let mut stride: u64 = 1;
        for i in 0..m {
            let base = coord[i].floor();
            let frac = coord[i] - base;
            let hi = (corner >> i) & 1 == 1;
            let gi = if hi {
                (base as u64 + 1).min(size[i] as u64 - 1)
            } else {
                base as u64
            };
            weight *= if hi { frac } else { 1.0 - frac };
            index = index.checked_add(gi.checked_mul(stride)?)?;
            stride = stride.checked_mul(size[i] as u64)?;
        }
        if weight == 0.0 {
            continue;
        }
        for (j, o) in out.iter_mut().enumerate() {
            let sample_index = index
                .checked_mul(u64::try_from(n_outputs).ok()?)?
                .checked_add(u64::try_from(j).ok()?)?;
            let s = read_sample(samples, sample_index, bps);
            *o += weight * s;
        }
    }

    // Decode: sample [0,1] -> Decode range.
    for (j, o) in out.iter_mut().enumerate() {
        let [d0, d1] = decode[j];
        *o = d0 + *o * (d1 - d0);
    }
    Some(out)
}

// ---------------------------------------------------------------------------
// Type 4 — PostScript calculator
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
enum PsOp {
    Num(f64),
    // arithmetic
    Abs,
    Add,
    Atan,
    Ceiling,
    Cos,
    Cvi,
    Cvr,
    Div,
    Exp,
    Floor,
    Idiv,
    Ln,
    Log,
    Mod,
    Mul,
    Neg,
    Round,
    Sin,
    Sqrt,
    Sub,
    Truncate,
    // comparison / boolean / bitwise
    And,
    Bitshift,
    Eq,
    Ne,
    False,
    Ge,
    Gt,
    Le,
    Lt,
    Not,
    Or,
    True,
    Xor,
    // stack
    Copy,
    Dup,
    Exch,
    Index,
    Pop,
    Roll,
    // control: If(body), IfElse(then, else)
    If(Vec<PsOp>),
    IfElse(Vec<PsOp>, Vec<PsOp>),
}

/// Tokenize + parse the calculator program. The outermost braces enclose the
/// whole program.
fn parse_postscript(src: &[u8]) -> Option<Vec<PsOp>> {
    let mut toks = PsTokens { src, pos: 0 };
    // Find the opening brace.
    match toks.next_token()? {
        PsTok::LBrace => {}
        _ => return None,
    }
    let mut budget = MAX_PS_STEPS;
    let program = parse_ps_block(&mut toks, 0, &mut budget)?;
    // Only whitespace/comments may follow the outer procedure.
    toks.next_token().is_none().then_some(program)
}

struct PsTokens<'a> {
    src: &'a [u8],
    pos: usize,
}

enum PsTok<'a> {
    LBrace,
    RBrace,
    Word(&'a [u8]),
}

impl<'a> PsTokens<'a> {
    fn next_token(&mut self) -> Option<PsTok<'a>> {
        while self.pos < self.src.len() {
            let c = self.src[self.pos];
            if c.is_ascii_whitespace() {
                self.pos += 1;
            } else if c == b'%' {
                while self.pos < self.src.len() && self.src[self.pos] != b'\n' {
                    self.pos += 1;
                }
            } else {
                break;
            }
        }
        if self.pos >= self.src.len() {
            return None;
        }
        match self.src[self.pos] {
            b'{' => {
                self.pos += 1;
                Some(PsTok::LBrace)
            }
            b'}' => {
                self.pos += 1;
                Some(PsTok::RBrace)
            }
            _ => {
                let start = self.pos;
                while self.pos < self.src.len() {
                    let c = self.src[self.pos];
                    if c.is_ascii_whitespace() || c == b'{' || c == b'}' || c == b'%' {
                        break;
                    }
                    self.pos += 1;
                }
                Some(PsTok::Word(&self.src[start..self.pos]))
            }
        }
    }
}

fn parse_ps_block(toks: &mut PsTokens<'_>, depth: u32, budget: &mut u32) -> Option<Vec<PsOp>> {
    if depth > 32 {
        return None;
    }
    let mut ops = Vec::new();
    // Pending procedure blocks awaiting an `if` / `ifelse` keyword.
    let mut pending: Vec<Vec<PsOp>> = Vec::new();
    loop {
        let tok = toks.next_token()?;
        match tok {
            PsTok::RBrace => {
                // Dangling procedure blocks with no if/ifelse: invalid.
                return if pending.is_empty() { Some(ops) } else { None };
            }
            PsTok::LBrace => {
                pending.push(parse_ps_block(toks, depth + 1, budget)?);
                if pending.len() > 2 {
                    return None;
                }
            }
            PsTok::Word(w) => {
                let word = std::str::from_utf8(w).ok()?;
                if word == "if" {
                    let body = pending.pop()?;
                    if !pending.is_empty() {
                        return None;
                    }
                    take_ps_budget(budget)?;
                    ops.push(PsOp::If(body));
                } else if word == "ifelse" {
                    let else_b = pending.pop()?;
                    let then_b = pending.pop()?;
                    if !pending.is_empty() {
                        return None;
                    }
                    take_ps_budget(budget)?;
                    ops.push(PsOp::IfElse(then_b, else_b));
                } else {
                    if !pending.is_empty() {
                        return None;
                    }
                    take_ps_budget(budget)?;
                    ops.push(parse_ps_word(word)?);
                }
            }
        }
    }
}

fn take_ps_budget(budget: &mut u32) -> Option<()> {
    *budget = budget.checked_sub(1)?;
    Some(())
}

fn parse_ps_word(w: &str) -> Option<PsOp> {
    if let Ok(n) = w.parse::<f64>() {
        return n.is_finite().then_some(PsOp::Num(n));
    }
    Some(match w {
        "abs" => PsOp::Abs,
        "add" => PsOp::Add,
        "atan" => PsOp::Atan,
        "ceiling" => PsOp::Ceiling,
        "cos" => PsOp::Cos,
        "cvi" => PsOp::Cvi,
        "cvr" => PsOp::Cvr,
        "div" => PsOp::Div,
        "exp" => PsOp::Exp,
        "floor" => PsOp::Floor,
        "idiv" => PsOp::Idiv,
        "ln" => PsOp::Ln,
        "log" => PsOp::Log,
        "mod" => PsOp::Mod,
        "mul" => PsOp::Mul,
        "neg" => PsOp::Neg,
        "round" => PsOp::Round,
        "sin" => PsOp::Sin,
        "sqrt" => PsOp::Sqrt,
        "sub" => PsOp::Sub,
        "truncate" => PsOp::Truncate,
        "and" => PsOp::And,
        "bitshift" => PsOp::Bitshift,
        "eq" => PsOp::Eq,
        "ne" => PsOp::Ne,
        "false" => PsOp::False,
        "ge" => PsOp::Ge,
        "gt" => PsOp::Gt,
        "le" => PsOp::Le,
        "lt" => PsOp::Lt,
        "not" => PsOp::Not,
        "or" => PsOp::Or,
        "true" => PsOp::True,
        "xor" => PsOp::Xor,
        "copy" => PsOp::Copy,
        "dup" => PsOp::Dup,
        "exch" => PsOp::Exch,
        "index" => PsOp::Index,
        "pop" => PsOp::Pop,
        "roll" => PsOp::Roll,
        _ => return None,
    })
}

fn eval_postscript(program: &[PsOp], inputs: &[f64]) -> Option<Vec<f64>> {
    if inputs.len() > 256 || inputs.iter().any(|v| !v.is_finite()) {
        return None;
    }
    let mut stack: Vec<f64> = inputs.to_vec();
    let mut steps = 0u32;
    exec_ps(program, &mut stack, &mut steps)?;
    Some(stack)
}

fn exec_ps(ops: &[PsOp], stack: &mut Vec<f64>, steps: &mut u32) -> Option<()> {
    use PsOp::*;
    for op in ops {
        *steps += 1;
        if *steps > MAX_PS_STEPS || stack.len() > 256 {
            return None;
        }
        macro_rules! un {
            (|$a:ident| $e:expr) => {{
                let $a = stack.pop()?;
                stack.push($e);
            }};
        }
        macro_rules! bin {
            (|$a:ident, $b:ident| $e:expr) => {{
                let $b = stack.pop()?;
                let $a = stack.pop()?;
                stack.push($e);
            }};
        }
        match op {
            Num(n) => stack.push(*n),
            Abs => un!(|a| a.abs()),
            Add => bin!(|a, b| a + b),
            // atan: PostScript wants degrees in 0..360 from num/den.
            Atan => bin!(|a, b| {
                let d = a.atan2(b).to_degrees();
                if d < 0.0 {
                    d + 360.0
                } else {
                    d
                }
            }),
            Ceiling => un!(|a| a.ceil()),
            Cos => un!(|a| a.to_radians().cos()),
            Cvi => un!(|a| a.trunc()),
            Cvr => un!(|a| a),
            Div => bin!(|a, b| if b == 0.0 { 0.0 } else { a / b }),
            Exp => bin!(|a, b| a.powf(b)),
            Floor => un!(|a| a.floor()),
            Idiv => bin!(|a, b| {
                let (a, b) = (ps_i64(a)?, ps_i64(b)?);
                if b == 0 {
                    0.0
                } else {
                    a.checked_div(b)? as f64
                }
            }),
            Ln => un!(|a| if a > 0.0 { a.ln() } else { 0.0 }),
            Log => un!(|a| if a > 0.0 { a.log10() } else { 0.0 }),
            Mod => bin!(|a, b| {
                let (a, b) = (ps_i64(a)?, ps_i64(b)?);
                if b == 0 {
                    0.0
                } else {
                    a.checked_rem(b)? as f64
                }
            }),
            Mul => bin!(|a, b| a * b),
            Neg => un!(|a| -a),
            Round => un!(|a| a.round()),
            Sin => un!(|a| a.to_radians().sin()),
            Sqrt => un!(|a| if a >= 0.0 { a.sqrt() } else { 0.0 }),
            Sub => bin!(|a, b| a - b),
            Truncate => un!(|a| a.trunc()),
            And => bin!(|a, b| (ps_i64(a)? & ps_i64(b)?) as f64),
            Bitshift => bin!(|a, b| {
                let (ai, bi) = (ps_i64(a)?, ps_i64(b)?);
                (if bi >= 0 {
                    ai.wrapping_shl(bi.min(63) as u32)
                } else {
                    ai >> bi.unsigned_abs().min(63)
                }) as f64
            }),
            Eq => bin!(|a, b| (a == b) as i64 as f64),
            Ne => bin!(|a, b| (a != b) as i64 as f64),
            False => stack.push(0.0),
            Ge => bin!(|a, b| (a >= b) as i64 as f64),
            Gt => bin!(|a, b| (a > b) as i64 as f64),
            Le => bin!(|a, b| (a <= b) as i64 as f64),
            Lt => bin!(|a, b| (a < b) as i64 as f64),
            Not => un!(|a| if a == 0.0 || a == 1.0 {
                (a == 0.0) as i64 as f64
            } else {
                !ps_i64(a)? as f64
            }),
            Or => bin!(|a, b| (ps_i64(a)? | ps_i64(b)?) as f64),
            True => stack.push(1.0),
            Xor => bin!(|a, b| (ps_i64(a)? ^ ps_i64(b)?) as f64),
            Copy => {
                let n = ps_count(stack.pop()?)?;
                if n > 0 {
                    let len = stack.len();
                    if n > len || len.checked_add(n)? > 256 {
                        return None;
                    }
                    stack.extend_from_within(len - n..);
                }
            }
            Dup => {
                let v = *stack.last()?;
                stack.push(v);
            }
            Exch => {
                let b = stack.pop()?;
                let a = stack.pop()?;
                stack.push(b);
                stack.push(a);
            }
            Index => {
                let n = ps_count(stack.pop()?)?;
                if n >= stack.len() {
                    return None;
                }
                let v = stack[stack.len() - 1 - n];
                stack.push(v);
            }
            Pop => {
                stack.pop()?;
            }
            Roll => {
                let j = ps_i64(stack.pop()?)?;
                let n = ps_count(stack.pop()?)?;
                if n > stack.len() || n == 0 {
                    if n == 0 {
                        continue;
                    }
                    return None;
                }
                let len = stack.len();
                let slice = &mut stack[len - n..];
                let j = j.rem_euclid(n as i64) as usize;
                slice.rotate_right(j);
            }
            If(body) => {
                let cond = stack.pop()?;
                if cond != 0.0 {
                    exec_ps(body, stack, steps)?;
                }
            }
            IfElse(then_b, else_b) => {
                let cond = stack.pop()?;
                if cond != 0.0 {
                    exec_ps(then_b, stack, steps)?;
                } else {
                    exec_ps(else_b, stack, steps)?;
                }
            }
        }
        if stack.len() > 256 || stack.iter().any(|v| !v.is_finite()) {
            return None;
        }
    }
    Some(())
}

fn ps_i64(value: f64) -> Option<i64> {
    // `i64::MAX as f64` rounds to 2^63, so use an exclusive upper bound.
    const I64_UPPER_EXCLUSIVE: f64 = 9_223_372_036_854_775_808.0;
    (value.is_finite() && value >= i64::MIN as f64 && value < I64_UPPER_EXCLUSIVE)
        .then_some(value.trunc() as i64)
}

fn ps_count(value: f64) -> Option<usize> {
    usize::try_from(ps_i64(value)?).ok()
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

fn numbers(obj: &PdfObject) -> Option<Vec<f64>> {
    let arr = obj.as_array().ok()?;
    if arr.len() > MAX_FUNCTION_COMPONENTS * 2 {
        return None;
    }
    let mut out = Vec::with_capacity(arr.len());
    for v in arr {
        let value = v.as_f64().ok()?;
        if !value.is_finite() {
            return None;
        }
        out.push(value);
    }
    Some(out)
}

fn number_pairs(obj: &PdfObject) -> Option<Vec<[f64; 2]>> {
    let nums = numbers(obj)?;
    if nums.len() % 2 != 0 {
        return None;
    }
    Some(nums.chunks(2).map(|c| [c[0], c[1]]).collect())
}

fn ordered_pairs(pairs: &[[f64; 2]]) -> bool {
    pairs.iter().all(|[min, max]| min <= max)
}

#[cfg(test)]
mod tests {
    use super::*;
    use zpdf_core::{PdfDict, PdfObject};

    fn dict(entries: Vec<(&str, PdfObject)>) -> PdfDict {
        let mut d = PdfDict::default();
        for (k, v) in entries {
            d.0.insert(zpdf_core::PdfName(k.to_string()), v);
        }
        d
    }

    fn arr(vals: &[f64]) -> PdfObject {
        PdfObject::Array(vals.iter().map(|&v| PdfObject::Real(v)).collect())
    }

    fn no_resolve() -> impl FnMut(ObjectId) -> Option<(PdfObject, Option<Vec<u8>>)> {
        |_| None
    }

    #[test]
    fn exponential_linear() {
        let d = dict(vec![
            ("FunctionType", PdfObject::Integer(2)),
            ("Domain", arr(&[0.0, 1.0])),
            ("C0", arr(&[0.0, 0.0, 0.0])),
            ("C1", arr(&[1.0, 0.5, 0.25])),
            ("N", PdfObject::Real(1.0)),
        ]);
        let mut r = no_resolve();
        let f = PdfFunction::parse(&d, None, &mut r).unwrap();
        assert_eq!(f.eval(&[0.0]).unwrap(), vec![0.0, 0.0, 0.0]);
        assert_eq!(f.eval(&[1.0]).unwrap(), vec![1.0, 0.5, 0.25]);
        let mid = f.eval(&[0.5]).unwrap();
        assert!((mid[0] - 0.5).abs() < 1e-9 && (mid[1] - 0.25).abs() < 1e-9);
        // domain clamp
        assert_eq!(f.eval(&[2.0]).unwrap(), vec![1.0, 0.5, 0.25]);
    }

    #[test]
    fn range_less_exponential_rejects_non_finite_results() {
        let make = |domain: [f64; 2], exponent: f64| {
            dict(vec![
                ("FunctionType", PdfObject::Integer(2)),
                ("Domain", arr(&domain)),
                ("C0", arr(&[0.0])),
                ("C1", arr(&[1.0])),
                ("N", PdfObject::Real(exponent)),
            ])
        };

        let mut r = no_resolve();
        let reciprocal = PdfFunction::parse(&make([0.0, 1.0], -1.0), None, &mut r).unwrap();
        assert!(reciprocal.eval(&[0.0]).is_none());

        let mut r = no_resolve();
        let fractional = PdfFunction::parse(&make([-1.0, 1.0], 0.5), None, &mut r).unwrap();
        assert!(fractional.eval(&[-1.0]).is_none());
    }

    #[test]
    fn sampled_1d_8bit() {
        // 3 samples, 1 output: 0, 128, 255 -> ramp 0..1
        let d = dict(vec![
            ("FunctionType", PdfObject::Integer(0)),
            ("Domain", arr(&[0.0, 1.0])),
            ("Range", arr(&[0.0, 1.0])),
            ("Size", PdfObject::Array(vec![PdfObject::Integer(3)])),
            ("BitsPerSample", PdfObject::Integer(8)),
        ]);
        let mut r = no_resolve();
        let f = PdfFunction::parse(&d, Some(&[0, 128, 255]), &mut r).unwrap();
        assert!((f.eval(&[0.0]).unwrap()[0] - 0.0).abs() < 1e-6);
        assert!((f.eval(&[1.0]).unwrap()[0] - 1.0).abs() < 1e-6);
        let q = f.eval(&[0.25]).unwrap()[0]; // halfway to sample 1 = ~0.25
        assert!((q - 128.0 / 255.0 / 2.0).abs() < 0.01, "got {q}");
    }

    #[test]
    fn sampled_multi_output() {
        // 2 samples x 3 outputs (RGB ramp black->white), 8 bps
        let d = dict(vec![
            ("FunctionType", PdfObject::Integer(0)),
            ("Domain", arr(&[0.0, 1.0])),
            ("Range", arr(&[0.0, 1.0, 0.0, 1.0, 0.0, 1.0])),
            ("Size", PdfObject::Array(vec![PdfObject::Integer(2)])),
            ("BitsPerSample", PdfObject::Integer(8)),
        ]);
        let mut r = no_resolve();
        let f = PdfFunction::parse(&d, Some(&[0, 0, 0, 255, 255, 255]), &mut r).unwrap();
        let mid = f.eval(&[0.5]).unwrap();
        assert_eq!(mid.len(), 3);
        for v in mid {
            assert!((v - 0.5).abs() < 0.01);
        }
    }

    #[test]
    fn stitching_two_halves() {
        let half = |c0: f64, c1: f64| {
            PdfObject::Dict(dict(vec![
                ("FunctionType", PdfObject::Integer(2)),
                ("Domain", arr(&[0.0, 1.0])),
                ("C0", arr(&[c0])),
                ("C1", arr(&[c1])),
                ("N", PdfObject::Real(1.0)),
            ]))
        };
        let d = dict(vec![
            ("FunctionType", PdfObject::Integer(3)),
            ("Domain", arr(&[0.0, 1.0])),
            (
                "Functions",
                PdfObject::Array(vec![half(0.0, 1.0), half(1.0, 0.0)]),
            ),
            ("Bounds", arr(&[0.5])),
            ("Encode", arr(&[0.0, 1.0, 0.0, 1.0])),
        ]);
        let mut r = no_resolve();
        let f = PdfFunction::parse(&d, None, &mut r).unwrap();
        assert!((f.eval(&[0.25]).unwrap()[0] - 0.5).abs() < 1e-9);
        assert!((f.eval(&[0.75]).unwrap()[0] - 0.5).abs() < 1e-9);
        assert!((f.eval(&[0.5]).unwrap()[0] - 1.0).abs() < 1e-9);
    }

    #[test]
    fn postscript_basic() {
        let d = dict(vec![
            ("FunctionType", PdfObject::Integer(4)),
            ("Domain", arr(&[0.0, 1.0])),
            ("Range", arr(&[0.0, 1.0, 0.0, 1.0])),
        ]);
        // out = (1-x, x*x)
        let src = b"{ dup 1 exch sub exch dup mul }";
        let mut r = no_resolve();
        let f = PdfFunction::parse(&d, Some(src), &mut r).unwrap();
        let out = f.eval(&[0.25]).unwrap();
        assert!((out[0] - 0.75).abs() < 1e-9);
        assert!((out[1] - 0.0625).abs() < 1e-9);
    }

    #[test]
    fn postscript_ifelse_roll() {
        let d = dict(vec![
            ("FunctionType", PdfObject::Integer(4)),
            ("Domain", arr(&[0.0, 1.0])),
            ("Range", arr(&[0.0, 1.0])),
        ]);
        // out = x < 0.5 ? 0 : 1
        let src = b"{ 0.5 lt { 0 } { 1 } ifelse }";
        let mut r = no_resolve();
        let f = PdfFunction::parse(&d, Some(src), &mut r).unwrap();
        assert_eq!(f.eval(&[0.2]).unwrap()[0], 0.0);
        assert_eq!(f.eval(&[0.7]).unwrap()[0], 1.0);
    }

    #[test]
    fn postscript_separation_like() {
        // Typical Separation tint transform: tint -> CMYK
        let d = dict(vec![
            ("FunctionType", PdfObject::Integer(4)),
            ("Domain", arr(&[0.0, 1.0])),
            ("Range", arr(&[0.0, 1.0, 0.0, 1.0, 0.0, 1.0, 0.0, 1.0])),
        ]);
        let src = b"{ dup 0.9 mul exch dup 0.2 mul exch dup 0.1 mul exch 0.05 mul }";
        let mut r = no_resolve();
        let f = PdfFunction::parse(&d, Some(src), &mut r).unwrap();
        let out = f.eval(&[1.0]).unwrap();
        assert_eq!(out.len(), 4);
        assert!((out[0] - 0.9).abs() < 1e-9);
        assert!((out[3] - 0.05).abs() < 1e-9);
    }

    #[test]
    fn postscript_results_are_stack_top() {
        // A program that does not consume its input: the results are the TOP
        // n stack values, not the bottom (which would include the input).
        let d = dict(vec![
            ("FunctionType", PdfObject::Integer(4)),
            ("Domain", arr(&[0.0, 1.0])),
            ("Range", arr(&[0.0, 1.0, 0.0, 1.0, 0.0, 1.0])),
        ]);
        let src = b"{ 0.1 0.2 0.3 }";
        let mut r = no_resolve();
        let f = PdfFunction::parse(&d, Some(src), &mut r).unwrap();
        let out = f.eval(&[0.42]).unwrap();
        assert_eq!(out, vec![0.1, 0.2, 0.3]);
    }

    #[test]
    fn malformed_rejected() {
        let mut r = no_resolve();
        // wrong type
        let d = dict(vec![
            ("FunctionType", PdfObject::Integer(9)),
            ("Domain", arr(&[0.0, 1.0])),
        ]);
        assert!(PdfFunction::parse(&d, None, &mut r).is_none());
        // type 0 without stream data
        let d0 = dict(vec![
            ("FunctionType", PdfObject::Integer(0)),
            ("Domain", arr(&[0.0, 1.0])),
            ("Range", arr(&[0.0, 1.0])),
            ("Size", PdfObject::Array(vec![PdfObject::Integer(2)])),
            ("BitsPerSample", PdfObject::Integer(8)),
        ]);
        assert!(PdfFunction::parse(&d0, None, &mut r).is_none());
        // type 0 with too few samples for the grid
        assert!(PdfFunction::parse(&d0, Some(&[1]), &mut r).is_none());

        // Grid-size arithmetic is checked instead of overflowing on hostile
        // dimensions, and dimensions above the supported interpolation limit
        // are rejected rather than silently ignored.
        let huge = dict(vec![
            ("FunctionType", PdfObject::Integer(0)),
            ("Domain", arr(&[0.0, 1.0, 0.0, 1.0])),
            ("Range", arr(&[0.0, 1.0])),
            ("Size", arr(&[u32::MAX as f64, u32::MAX as f64])),
            ("BitsPerSample", PdfObject::Integer(32)),
        ]);
        assert!(PdfFunction::parse(&huge, Some(&[]), &mut r).is_none());

        let too_many_dims = dict(vec![
            ("FunctionType", PdfObject::Integer(0)),
            ("Domain", arr(&[0.0, 1.0].repeat(17))),
            ("Range", arr(&[0.0, 1.0])),
            ("Size", arr(&[1.0; 17])),
            ("BitsPerSample", PdfObject::Integer(8)),
        ]);
        assert!(PdfFunction::parse(&too_many_dims, Some(&[0]), &mut r).is_none());
    }

    #[test]
    fn sampled_function_does_not_retain_trailing_stream_bytes() {
        let d = dict(vec![
            ("FunctionType", PdfObject::Integer(0)),
            ("Domain", arr(&[0.0, 1.0])),
            ("Range", arr(&[0.0, 1.0])),
            ("Size", arr(&[2.0])),
            ("BitsPerSample", PdfObject::Integer(8)),
        ]);
        let mut r = no_resolve();
        let f = PdfFunction::parse(&d, Some(&[0, 255, 1, 2, 3, 4]), &mut r).unwrap();
        let FunctionKind::Sampled { samples, .. } = &f.kind else {
            panic!("expected sampled function");
        };
        assert_eq!(samples, &[0, 255]);
    }

    #[test]
    fn postscript_integer_overflow_is_an_eval_error() {
        let d = dict(vec![
            ("FunctionType", PdfObject::Integer(4)),
            ("Domain", arr(&[0.0, 1.0])),
            ("Range", arr(&[0.0, 1.0])),
        ]);
        let mut r = no_resolve();
        let f = PdfFunction::parse(&d, Some(b"{ -9223372036854775808 -1 idiv }"), &mut r).unwrap();
        assert!(f.eval(&[0.5]).is_none());
    }
}
