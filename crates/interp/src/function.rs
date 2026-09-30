//! PDF functions (types 0, 2, 3 and 4), evaluated as MuPDF evaluates them:
//! inputs clipped to /Domain, outputs clipped to /Range, sampled functions
//! interpolated linearly whatever their /Order.

use pdf_core::{Document, Object};

/// Most inputs or outputs a function may declare.
const MAX_COMPONENTS: usize = 32;
/// Bound on the samples of one type 0 function.
const MAX_SAMPLES: usize = 1 << 24;
/// Nesting bound for stitching functions.
const MAX_DEPTH: usize = 8;
/// Operand stack depth of the PostScript calculator (ISO 32000 says 100).
const PS_STACK: usize = 100;
/// Bound on the parsed program of one type 4 function.
const MAX_PS_OPS: usize = 1 << 16;

#[derive(Debug)]
pub(crate) struct Function {
    domain: Vec<[f64; 2]>,
    range: Option<Vec<[f64; 2]>>,
    kind: Kind,
}

#[derive(Debug)]
enum Kind {
    Sampled(Sampled),
    Exponential {
        c0: Vec<f64>,
        c1: Vec<f64>,
        n: f64,
    },
    Stitching {
        functions: Vec<Function>,
        bounds: Vec<f64>,
        encode: Vec<[f64; 2]>,
    },
    PostScript {
        code: Vec<PsOp>,
    },
}

impl Function {
    /// Loads a function object (dictionary or stream); `None` when it is
    /// malformed.
    pub(crate) fn load(doc: &Document, object: &Object) -> Option<Function> {
        load(doc, object, 0)
    }

    /// Evaluates at `input`, writing up to `out.len()` outputs (missing
    /// ones are 0).
    pub(crate) fn eval(&self, input: &[f64], out: &mut [f64]) {
        out.iter_mut().for_each(|v| *v = 0.0);
        let mut x = [0.0f64; MAX_COMPONENTS];
        let m = self.domain.len().min(MAX_COMPONENTS);
        for (i, slot) in x.iter_mut().enumerate().take(m) {
            let [d0, d1] = self.domain[i];
            let v = input.get(i).copied().unwrap_or(d0);
            *slot = clamp(v, d0, d1);
        }
        match &self.kind {
            Kind::Sampled(sampled) => sampled.eval(&x[..m], &self.domain, out),
            Kind::Exponential { c0, c1, n } => {
                let t = x[0];
                let p = if *n == 1.0 { t } else { t.powf(*n) };
                for (j, slot) in out.iter_mut().enumerate().take(c0.len()) {
                    let p = if p.is_finite() { p } else { 0.0 };
                    *slot = c0[j] + p * (c1[j] - c0[j]);
                }
            }
            Kind::Stitching {
                functions,
                bounds,
                encode,
            } => {
                let [d0, d1] = self.domain[0];
                let t = x[0];
                let k = bounds.iter().position(|&b| t < b).unwrap_or(bounds.len());
                let low = if k == 0 { d0 } else { bounds[k - 1] };
                let high = if k == bounds.len() { d1 } else { bounds[k] };
                let [e0, e1] = encode[k];
                let u = interpolate(t, low, high, e0, e1);
                functions[k].eval(&[u], out);
            }
            Kind::PostScript { code } => {
                let mut stack = PsStack::default();
                for &v in &x[..m] {
                    stack.push(PsValue::Real(v));
                }
                run_ps(code, &mut stack, 0);
                let n = self.range.as_ref().map_or(0, Vec::len).min(out.len());
                let results = stack.take_numbers(n);
                for (slot, v) in out.iter_mut().zip(results) {
                    *slot = v;
                }
            }
        }
        if let Some(range) = &self.range {
            for (slot, [r0, r1]) in out.iter_mut().zip(range) {
                *slot = clamp(*slot, *r0, *r1);
            }
        }
    }
}

fn clamp(v: f64, lo: f64, hi: f64) -> f64 {
    if v.is_nan() {
        return lo;
    }
    if lo <= hi {
        v.clamp(lo, hi)
    } else {
        v.clamp(hi, lo)
    }
}

fn interpolate(x: f64, x0: f64, x1: f64, y0: f64, y1: f64) -> f64 {
    if x1 == x0 {
        return y0;
    }
    y0 + (x - x0) * (y1 - y0) / (x1 - x0)
}

fn load(doc: &Document, object: &Object, depth: usize) -> Option<Function> {
    if depth > MAX_DEPTH {
        return None;
    }
    let resolved = doc.resolve(object).ok()?;
    let (dict, stream) = match &resolved {
        Object::Dict(dict) => (dict, None),
        Object::Stream(stream) => (&stream.dict, Some(stream)),
        _ => return None,
    };
    let number_pairs = |key: &[u8]| -> Option<Vec<[f64; 2]>> {
        let items = doc.resolve_array(dict.get(key)?).ok()??;
        let values: Vec<f64> = items
            .iter()
            .filter_map(|o| doc.resolve_f64(o).ok().flatten())
            .collect();
        if values.len() != items.len()
            || !values.len().is_multiple_of(2)
            || values.len() / 2 > MAX_COMPONENTS
        {
            return None;
        }
        Some(values.as_chunks::<2>().0.to_vec())
    };
    let numbers = |key: &[u8]| -> Option<Vec<f64>> {
        let items = doc.resolve_array(dict.get(key)?).ok()??;
        let values: Vec<f64> = items
            .iter()
            .filter_map(|o| doc.resolve_f64(o).ok().flatten())
            .collect();
        (values.len() == items.len()).then_some(values)
    };
    let function_type = doc.resolve_i64(dict.get(b"FunctionType")?).ok()??;
    let domain = number_pairs(b"Domain").filter(|d| !d.is_empty())?;
    let range = number_pairs(b"Range").filter(|r| !r.is_empty());
    let kind = match function_type {
        0 => {
            let range = range.clone()?;
            let stream = stream?;
            let size: Vec<usize> = numbers(b"Size")?
                .into_iter()
                .map(|v| {
                    if v.is_finite() && v >= 1.0 {
                        v as usize
                    } else {
                        1
                    }
                })
                .collect();
            if size.len() != domain.len() {
                return None;
            }
            let bps = doc.resolve_i64(dict.get(b"BitsPerSample")?).ok()??;
            if !matches!(bps, 1 | 2 | 4 | 8 | 12 | 16 | 24 | 32) {
                return None;
            }
            let outputs = range.len();
            let mut total = outputs;
            for &s in &size {
                total = total.checked_mul(s).filter(|&t| t <= MAX_SAMPLES)?;
            }
            let encode = number_pairs(b"Encode")
                .filter(|e| e.len() == size.len())
                .unwrap_or_else(|| size.iter().map(|&s| [0.0, (s - 1) as f64]).collect());
            let decode = number_pairs(b"Decode")
                .filter(|d| d.len() == outputs)
                .unwrap_or_else(|| range.clone());
            let data = doc.decode_stream(stream).ok()?.data;
            let samples = read_samples(&data, bps as u32, total);
            Kind::Sampled(Sampled {
                size,
                encode,
                decode,
                outputs,
                samples,
            })
        }
        2 => {
            let c0 = numbers(b"C0").unwrap_or_else(|| vec![0.0]);
            let c1 = numbers(b"C1").unwrap_or_else(|| vec![1.0]);
            if c0.len() != c1.len() || c0.len() > MAX_COMPONENTS {
                return None;
            }
            let n = doc.resolve_f64(dict.get(b"N")?).ok()??;
            Kind::Exponential { c0, c1, n }
        }
        3 => {
            let items = doc.resolve_array(dict.get(b"Functions")?).ok()??;
            if items.is_empty() || items.len() > 1024 {
                return None;
            }
            let mut functions = Vec::with_capacity(items.len());
            for item in &items {
                functions.push(load(doc, item, depth + 1)?);
            }
            let mut bounds = numbers(b"Bounds").unwrap_or_default();
            bounds.truncate(functions.len() - 1);
            if bounds.len() + 1 != functions.len() {
                return None;
            }
            let encode = number_pairs(b"Encode")
                .filter(|e| e.len() == functions.len())
                .unwrap_or_else(|| vec![[0.0, 1.0]; functions.len()]);
            Kind::Stitching {
                functions,
                bounds,
                encode,
            }
        }
        4 => {
            range.as_ref()?;
            let data = doc.decode_stream(stream?).ok()?.data;
            let code = parse_ps(&data)?;
            Kind::PostScript { code }
        }
        _ => return None,
    };
    Some(Function {
        domain,
        range,
        kind,
    })
}

/// Samples normalised to 0..=1 (divided by 2^bps - 1); short data reads 0.
fn read_samples(data: &[u8], bps: u32, count: usize) -> Vec<f64> {
    let max = ((1u64 << bps) - 1) as f64;
    let mut out = Vec::with_capacity(count);
    let mut bit = 0usize;
    for _ in 0..count {
        let mut value = 0u64;
        for _ in 0..bps {
            let byte = data.get(bit / 8).copied().unwrap_or(0);
            value = (value << 1) | u64::from((byte >> (7 - bit % 8)) & 1);
            bit += 1;
        }
        out.push(value as f64 / max);
    }
    out
}

#[derive(Debug)]
struct Sampled {
    size: Vec<usize>,
    encode: Vec<[f64; 2]>,
    decode: Vec<[f64; 2]>,
    outputs: usize,
    /// Normalised to 0..=1, first input varying fastest.
    samples: Vec<f64>,
}

impl Sampled {
    fn eval(&self, x: &[f64], domain: &[[f64; 2]], out: &mut [f64]) {
        let Sampled {
            size,
            encode,
            decode,
            outputs,
            samples,
        } = self;
        let m = size.len();
        // Per input: the lower sample index, the fraction towards the next one.
        let mut index = [0usize; MAX_COMPONENTS];
        let mut frac = [0.0f64; MAX_COMPONENTS];
        for i in 0..m {
            let e = interpolate(x[i], domain[i][0], domain[i][1], encode[i][0], encode[i][1]);
            let e = clamp(e, 0.0, (size[i] - 1) as f64);
            let lower = e.floor();
            index[i] = lower as usize;
            frac[i] = if index[i] + 1 < size[i] {
                e - lower
            } else {
                0.0
            };
        }
        // Multilinear interpolation over the 2^m corners (nearest lower
        // sample beyond 12 inputs).
        let corners = if m <= 12 { 1usize << m } else { 1 };
        for (j, slot) in out.iter_mut().enumerate().take(*outputs) {
            let mut acc = 0.0;
            for corner in 0..corners {
                let mut weight = 1.0;
                let mut offset = 0usize;
                let mut stride = *outputs;
                for i in 0..m {
                    let high = (corner >> i) & 1 == 1;
                    let (w, idx) = if high {
                        (frac[i], index[i] + 1)
                    } else {
                        (1.0 - frac[i], index[i])
                    };
                    weight *= w;
                    offset += idx.min(size[i] - 1) * stride;
                    stride *= size[i];
                }
                if weight != 0.0 {
                    acc += weight * samples.get(offset + j).copied().unwrap_or(0.0);
                }
            }
            let [d0, d1] = decode[j];
            *slot = d0 + acc * (d1 - d0);
        }
    }
}

// ----- type 4: the PostScript calculator ------------------------------------

#[derive(Clone, Copy, Debug, PartialEq)]
enum PsOp {
    Push(PsValue),
    /// Run the block at `then` when true; the block ends at `end`.
    If {
        then_end: usize,
    },
    /// `{then} {else} ifelse`: blocks [pc+1, then_end), [then_end, else_end).
    IfElse {
        then_end: usize,
        else_end: usize,
    },
    Op(PsOperator),
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum PsValue {
    Int(i64),
    Real(f64),
    Bool(bool),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PsOperator {
    Abs,
    Add,
    And,
    Atan,
    Bitshift,
    Ceiling,
    Copy,
    Cos,
    Cvi,
    Cvr,
    Div,
    Dup,
    Eq,
    Exch,
    Exp,
    Floor,
    Ge,
    Gt,
    Idiv,
    Index,
    Le,
    Ln,
    Log,
    Lt,
    Mod,
    Mul,
    Ne,
    Neg,
    Not,
    Or,
    Pop,
    Roll,
    Round,
    Sin,
    Sqrt,
    Sub,
    Truncate,
    Xor,
}

fn ps_operator(word: &[u8]) -> Option<PsOperator> {
    use PsOperator::*;
    Some(match word {
        b"abs" => Abs,
        b"add" => Add,
        b"and" => And,
        b"atan" => Atan,
        b"bitshift" => Bitshift,
        b"ceiling" => Ceiling,
        b"copy" => Copy,
        b"cos" => Cos,
        b"cvi" => Cvi,
        b"cvr" => Cvr,
        b"div" => Div,
        b"dup" => Dup,
        b"eq" => Eq,
        b"exch" => Exch,
        b"exp" => Exp,
        b"floor" => Floor,
        b"ge" => Ge,
        b"gt" => Gt,
        b"idiv" => Idiv,
        b"index" => Index,
        b"le" => Le,
        b"ln" => Ln,
        b"log" => Log,
        b"lt" => Lt,
        b"mod" => Mod,
        b"mul" => Mul,
        b"ne" => Ne,
        b"neg" => Neg,
        b"not" => Not,
        b"or" => Or,
        b"pop" => Pop,
        b"roll" => Roll,
        b"round" => Round,
        b"sin" => Sin,
        b"sqrt" => Sqrt,
        b"sub" => Sub,
        b"truncate" => Truncate,
        b"xor" => Xor,
        _ => return None,
    })
}

/// Parses `{ ... }` into a flat program where `if`/`ifelse` blocks follow
/// their operator.
fn parse_ps(data: &[u8]) -> Option<Vec<PsOp>> {
    let mut pos = 0usize;
    skip_ws(data, &mut pos);
    if data.get(pos) != Some(&b'{') {
        return None;
    }
    pos += 1;
    let mut code = Vec::new();
    parse_block(data, &mut pos, &mut code, 0)?;
    Some(code)
}

fn skip_ws(data: &[u8], pos: &mut usize) {
    while let Some(&b) = data.get(*pos) {
        if b == b'%' {
            while let Some(&c) = data.get(*pos) {
                if c == b'\n' || c == b'\r' {
                    break;
                }
                *pos += 1;
            }
        } else if b.is_ascii_whitespace() || b == 0 {
            *pos += 1;
        } else {
            break;
        }
    }
}

/// Parses until the matching `}`; nested blocks become If/IfElse ops.
fn parse_block(data: &[u8], pos: &mut usize, code: &mut Vec<PsOp>, depth: usize) -> Option<()> {
    if depth > 64 {
        return None;
    }
    // Blocks parsed but not yet claimed by if/ifelse: (start, end) in `code`.
    let mut pending: Vec<Vec<PsOp>> = Vec::new();
    loop {
        if code.len() > MAX_PS_OPS {
            return None;
        }
        skip_ws(data, pos);
        let &b = data.get(*pos)?;
        match b {
            b'}' => {
                *pos += 1;
                return Some(());
            }
            b'{' => {
                *pos += 1;
                let mut inner = Vec::new();
                parse_block(data, pos, &mut inner, depth + 1)?;
                pending.push(inner);
            }
            _ => {
                let start = *pos;
                while let Some(&c) = data.get(*pos) {
                    if c.is_ascii_whitespace() || matches!(c, b'{' | b'}' | b'%') {
                        break;
                    }
                    *pos += 1;
                }
                let word = &data[start..*pos];
                match word {
                    b"if" => {
                        let block = pending.pop()?;
                        pending.clear();
                        let base = code.len() + 1;
                        code.push(PsOp::If {
                            then_end: base + block.len(),
                        });
                        code.extend(shift(block, base));
                    }
                    b"ifelse" => {
                        let else_block = pending.pop()?;
                        let then_block = pending.pop()?;
                        pending.clear();
                        let base = code.len() + 1;
                        let then_end = base + then_block.len();
                        let else_end = then_end + else_block.len();
                        code.push(PsOp::IfElse { then_end, else_end });
                        code.extend(shift(then_block, base));
                        code.extend(shift(else_block, then_end));
                    }
                    b"true" => code.push(PsOp::Push(PsValue::Bool(true))),
                    b"false" => code.push(PsOp::Push(PsValue::Bool(false))),
                    _ => {
                        if let Some(op) = ps_operator(word) {
                            code.push(PsOp::Op(op));
                        } else {
                            let text = std::str::from_utf8(word).ok()?;
                            if let Ok(i) = text.parse::<i64>() {
                                code.push(PsOp::Push(PsValue::Int(i)));
                            } else if let Ok(r) = text.parse::<f64>() {
                                code.push(PsOp::Push(PsValue::Real(r)));
                            } else {
                                return None;
                            }
                        }
                    }
                }
            }
        }
    }
}

/// Relocates a block's jump targets to start at `base`.
fn shift(block: Vec<PsOp>, base: usize) -> impl Iterator<Item = PsOp> {
    block.into_iter().map(move |op| match op {
        PsOp::If { then_end } => PsOp::If {
            then_end: then_end + base,
        },
        PsOp::IfElse { then_end, else_end } => PsOp::IfElse {
            then_end: then_end + base,
            else_end: else_end + base,
        },
        other => other,
    })
}

#[derive(Default)]
struct PsStack {
    values: Vec<PsValue>,
}

impl PsStack {
    fn push(&mut self, v: PsValue) {
        if self.values.len() < PS_STACK {
            self.values.push(v);
        }
    }

    fn pop(&mut self) -> Option<PsValue> {
        self.values.pop()
    }

    fn pop_num(&mut self) -> f64 {
        match self.values.pop() {
            Some(PsValue::Int(i)) => i as f64,
            Some(PsValue::Real(r)) => r,
            Some(PsValue::Bool(b)) => f64::from(u8::from(b)),
            None => 0.0,
        }
    }

    fn pop_int(&mut self) -> i64 {
        match self.values.pop() {
            Some(PsValue::Int(i)) => i,
            Some(PsValue::Real(r)) if r.is_finite() => r as i64,
            Some(PsValue::Bool(b)) => i64::from(b),
            _ => 0,
        }
    }

    /// The top `n` values as numbers, bottom first.
    fn take_numbers(&mut self, n: usize) -> Vec<f64> {
        let mut out = vec![0.0; n];
        for slot in out.iter_mut().rev() {
            *slot = self.pop_num();
        }
        out
    }
}

fn run_ps(code: &[PsOp], stack: &mut PsStack, start: usize) {
    run_range(code, stack, start, code.len());
}

fn run_range(code: &[PsOp], stack: &mut PsStack, start: usize, end: usize) {
    let mut pc = start;
    while pc < end.min(code.len()) {
        match code[pc] {
            PsOp::Push(v) => {
                stack.push(v);
                pc += 1;
            }
            PsOp::If { then_end } => {
                let cond = matches!(stack.pop(), Some(PsValue::Bool(true)));
                if cond {
                    run_range(code, stack, pc + 1, then_end);
                }
                pc = then_end;
            }
            PsOp::IfElse { then_end, else_end } => {
                let cond = matches!(stack.pop(), Some(PsValue::Bool(true)));
                if cond {
                    run_range(code, stack, pc + 1, then_end);
                } else {
                    run_range(code, stack, then_end, else_end);
                }
                pc = else_end;
            }
            PsOp::Op(op) => {
                apply_op(op, stack);
                pc += 1;
            }
        }
    }
}

fn apply_op(op: PsOperator, stack: &mut PsStack) {
    use PsOperator::*;
    use PsValue::{Bool, Int, Real};
    match op {
        Abs => match stack.pop() {
            Some(Int(i)) => stack.push(Int(i.saturating_abs())),
            Some(Real(r)) => stack.push(Real(r.abs())),
            _ => stack.push(Int(0)),
        },
        Neg => match stack.pop() {
            Some(Int(i)) => stack.push(Int(i.saturating_neg())),
            Some(Real(r)) => stack.push(Real(-r)),
            _ => stack.push(Int(0)),
        },
        Add | Sub | Mul => {
            let b = stack.pop();
            let a = stack.pop();
            match (a, b) {
                (Some(Int(x)), Some(Int(y))) => {
                    let r = match op {
                        Add => x.checked_add(y),
                        Sub => x.checked_sub(y),
                        _ => x.checked_mul(y),
                    };
                    match r {
                        Some(v) => stack.push(Int(v)),
                        None => {
                            let (x, y) = (x as f64, y as f64);
                            stack.push(Real(match op {
                                Add => x + y,
                                Sub => x - y,
                                _ => x * y,
                            }));
                        }
                    }
                }
                (a, b) => {
                    let (x, y) = (num(a), num(b));
                    stack.push(Real(match op {
                        Add => x + y,
                        Sub => x - y,
                        _ => x * y,
                    }));
                }
            }
        }
        Div => {
            let y = stack.pop_num();
            let x = stack.pop_num();
            stack.push(Real(if y == 0.0 { 0.0 } else { x / y }));
        }
        Idiv | Mod => {
            let y = stack.pop_int();
            let x = stack.pop_int();
            let v = if y == 0 {
                0
            } else if op == Idiv {
                x.wrapping_div(y)
            } else {
                x.wrapping_rem(y)
            };
            stack.push(Int(v));
        }
        Atan => {
            let den = stack.pop_num();
            let n = stack.pop_num();
            let mut angle = n.atan2(den).to_degrees();
            if angle < 0.0 {
                angle += 360.0;
            }
            stack.push(Real(angle));
        }
        Ceiling | Floor | Round | Truncate => match stack.pop() {
            Some(Int(i)) => stack.push(Int(i)),
            other => {
                let r = num(other);
                stack.push(Real(match op {
                    Ceiling => r.ceil(),
                    Floor => r.floor(),
                    Round => (r + 0.5).floor(),
                    _ => r.trunc(),
                }));
            }
        },
        Cos => {
            let r = stack.pop_num();
            stack.push(Real(r.to_radians().cos()));
        }
        Sin => {
            let r = stack.pop_num();
            stack.push(Real(r.to_radians().sin()));
        }
        Sqrt => {
            let r = stack.pop_num();
            stack.push(Real(if r < 0.0 { 0.0 } else { r.sqrt() }));
        }
        Exp => {
            let e = stack.pop_num();
            let b = stack.pop_num();
            let v = b.powf(e);
            stack.push(Real(if v.is_finite() { v } else { 0.0 }));
        }
        Ln | Log => {
            let r = stack.pop_num();
            let v = if r <= 0.0 {
                0.0
            } else if op == Ln {
                r.ln()
            } else {
                r.log10()
            };
            stack.push(Real(v));
        }
        Cvi => {
            let i = stack.pop_int();
            stack.push(Int(i));
        }
        Cvr => {
            let r = stack.pop_num();
            stack.push(Real(r));
        }
        Eq | Ne => {
            let b = stack.pop();
            let a = stack.pop();
            let equal = match (a, b) {
                (Some(Bool(x)), Some(Bool(y))) => x == y,
                (a, b) => num(a) == num(b),
            };
            stack.push(Bool(if op == Eq { equal } else { !equal }));
        }
        Ge | Gt | Le | Lt => {
            let y = stack.pop_num();
            let x = stack.pop_num();
            stack.push(Bool(match op {
                Ge => x >= y,
                Gt => x > y,
                Le => x <= y,
                _ => x < y,
            }));
        }
        And | Or | Xor => {
            let b = stack.pop();
            let a = stack.pop();
            match (a, b) {
                (Some(Bool(x)), Some(Bool(y))) => stack.push(Bool(match op {
                    And => x && y,
                    Or => x || y,
                    _ => x ^ y,
                })),
                (a, b) => {
                    let (x, y) = (int(a), int(b));
                    stack.push(Int(match op {
                        And => x & y,
                        Or => x | y,
                        _ => x ^ y,
                    }));
                }
            }
        }
        Not => match stack.pop() {
            Some(Bool(b)) => stack.push(Bool(!b)),
            other => stack.push(Int(!int(other))),
        },
        Bitshift => {
            let shift = stack.pop_int();
            let value = stack.pop_int();
            let v = if shift >= 0 {
                if shift >= 64 {
                    0
                } else {
                    value.wrapping_shl(shift as u32)
                }
            } else if -shift >= 64 {
                0
            } else {
                value.wrapping_shr((-shift) as u32)
            };
            stack.push(Int(v));
        }
        Dup => {
            if let Some(&top) = stack.values.last() {
                stack.push(top);
            }
        }
        Exch => {
            let n = stack.values.len();
            if n >= 2 {
                stack.values.swap(n - 1, n - 2);
            }
        }
        Pop => {
            stack.pop();
        }
        Copy => {
            let n = stack.pop_int();
            let len = stack.values.len();
            if n > 0 && (n as usize) <= len {
                let start = len - n as usize;
                for i in start..len {
                    let v = stack.values[i];
                    stack.push(v);
                }
            }
        }
        Index => {
            let n = stack.pop_int();
            let len = stack.values.len();
            if n >= 0 && (n as usize) < len {
                let v = stack.values[len - 1 - n as usize];
                stack.push(v);
            }
        }
        Roll => {
            let j = stack.pop_int();
            let n = stack.pop_int();
            let len = stack.values.len();
            if n > 0 && (n as usize) <= len {
                let n = n as usize;
                let slice = &mut stack.values[len - n..];
                let j = j.rem_euclid(n as i64) as usize;
                slice.rotate_right(j);
            }
        }
    }
}

fn num(v: Option<PsValue>) -> f64 {
    match v {
        Some(PsValue::Int(i)) => i as f64,
        Some(PsValue::Real(r)) => r,
        Some(PsValue::Bool(b)) => f64::from(u8::from(b)),
        None => 0.0,
    }
}

fn int(v: Option<PsValue>) -> i64 {
    match v {
        Some(PsValue::Int(i)) => i,
        Some(PsValue::Real(r)) if r.is_finite() => r as i64,
        Some(PsValue::Bool(b)) => i64::from(b),
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ps(code: &str, inputs: &[f64], outputs: usize) -> Vec<f64> {
        let program = parse_ps(code.as_bytes()).expect("program parses");
        let mut stack = PsStack::default();
        for &v in inputs {
            stack.push(PsValue::Real(v));
        }
        run_ps(&program, &mut stack, 0);
        stack.take_numbers(outputs)
    }

    #[test]
    fn postscript_ifelse_picks_the_branch_by_condition() {
        let code = "{ dup 0.5 gt { pop 1 } { 2 mul } ifelse }";
        assert_eq!(ps(code, &[0.75], 1), vec![1.0]);
        assert_eq!(ps(code, &[0.25], 1), vec![0.5]);
    }

    #[test]
    fn postscript_roll_and_index_follow_the_operand_order() {
        assert_eq!(ps("{ 1 2 3 3 1 roll }", &[], 3), vec![3.0, 1.0, 2.0]);
        assert_eq!(ps("{ 7 8 9 2 index }", &[], 4), vec![7.0, 8.0, 9.0, 7.0]);
    }
}
