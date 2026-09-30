//! Type 2 charstring interpreter (CFF), producing cubic outlines.

use crate::cff::Index;
use crate::error::{FontError, Result};
use crate::outline::{Outline, OutlineBuilder};

/// Type 2 operand stack limit.
const MAX_STACK: usize = 48;
/// Subroutine nesting limit (the Type 2 specification allows 10).
const MAX_SUBR_DEPTH: u32 = 16;
/// Most operators and operands executed for one glyph.
const MAX_WORK: u32 = 1_000_000;

pub(crate) struct Type2Context<'a> {
    pub(crate) cff: &'a [u8],
    pub(crate) global_subrs: &'a Index,
    pub(crate) local_subrs: Option<&'a Index>,
}

/// Accent composition requested by an `endchar` with four arguments.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Seac {
    pub(crate) adx: f64,
    pub(crate) ady: f64,
    pub(crate) base_code: u8,
    pub(crate) accent_code: u8,
}

pub(crate) struct Type2Output {
    pub(crate) outline: Outline,
    /// Width argument relative to nominalWidthX, when the charstring gave one.
    pub(crate) width: Option<f64>,
    pub(crate) seac: Option<Seac>,
}

pub(crate) fn subr_bias(count: u32) -> i64 {
    if count < 1240 {
        107
    } else if count < 33900 {
        1131
    } else {
        32768
    }
}

pub(crate) fn run_type2(ctx: &Type2Context<'_>, charstring: &[u8]) -> Result<Type2Output> {
    let mut m = Machine {
        ctx,
        stack: Vec::with_capacity(MAX_STACK),
        transient: [0.0; 32],
        b: OutlineBuilder::new(),
        x: 0.0,
        y: 0.0,
        stems: 0,
        width: None,
        width_done: false,
        seac: None,
        work: 0,
        seed: 0x2545_F491,
    };
    m.exec(charstring, 0)?;
    Ok(Type2Output {
        outline: m.b.finish()?,
        width: m.width,
        seac: m.seac,
    })
}

enum Flow {
    Continue,
    Return,
    End,
}

struct Machine<'a, 'c> {
    ctx: &'a Type2Context<'c>,
    stack: Vec<f64>,
    transient: [f64; 32],
    b: OutlineBuilder,
    x: f64,
    y: f64,
    stems: usize,
    width: Option<f64>,
    width_done: bool,
    seac: Option<Seac>,
    work: u32,
    seed: u32,
}

fn byte(code: &[u8], i: &mut usize) -> Result<u8> {
    let b = *code
        .get(*i)
        .ok_or(FontError::Truncated("Type 2 charstring"))?;
    *i += 1;
    Ok(b)
}

impl Machine<'_, '_> {
    fn push(&mut self, v: f64) -> Result<()> {
        if self.stack.len() >= MAX_STACK {
            return Err(FontError::LimitExceeded("Type 2 operand stack"));
        }
        self.stack.push(v);
        Ok(())
    }

    fn pop(&mut self) -> Result<f64> {
        self.stack
            .pop()
            .ok_or(FontError::Malformed("Type 2 operand stack underflow"))
    }

    /// Takes the optional leading width operand before the first stack-clearing operator.
    fn take_width(&mut self, has_extra: bool) {
        if !self.width_done {
            self.width_done = true;
            if has_extra && !self.stack.is_empty() {
                self.width = Some(self.stack.remove(0));
            }
        }
    }

    fn move_rel(&mut self, dx: f64, dy: f64) -> Result<()> {
        self.x += dx;
        self.y += dy;
        self.b.move_to(self.x as f32, self.y as f32)
    }

    fn line_rel(&mut self, dx: f64, dy: f64) -> Result<()> {
        self.x += dx;
        self.y += dy;
        self.b.line_to(self.x as f32, self.y as f32)
    }

    fn curve_rel(&mut self, d: [f64; 6]) -> Result<()> {
        let c1 = (self.x + d[0], self.y + d[1]);
        let c2 = (c1.0 + d[2], c1.1 + d[3]);
        self.x = c2.0 + d[4];
        self.y = c2.1 + d[5];
        self.b.curve_to(
            (c1.0 as f32, c1.1 as f32),
            (c2.0 as f32, c2.1 as f32),
            self.x as f32,
            self.y as f32,
        )
    }

    fn call_subr(&mut self, global: bool, depth: u32) -> Result<Flow> {
        if depth >= MAX_SUBR_DEPTH {
            return Err(FontError::LimitExceeded("Type 2 subroutine depth"));
        }
        let index = if global {
            Some(self.ctx.global_subrs)
        } else {
            self.ctx.local_subrs
        };
        let index = index.ok_or(FontError::Missing("Type 2 local subroutines"))?;
        let n = self.pop()? as i64 + subr_bias(index.count());
        let n = u32::try_from(n).map_err(|_| FontError::Malformed("Type 2 subroutine index"))?;
        let subr = index
            .get(self.ctx.cff, n)
            .ok_or(FontError::Malformed("Type 2 subroutine index"))?;
        self.exec(subr, depth + 1)
    }

    fn exec(&mut self, code: &[u8], depth: u32) -> Result<Flow> {
        let mut i = 0usize;
        while i < code.len() {
            self.work += 1;
            if self.work > MAX_WORK {
                return Err(FontError::LimitExceeded("Type 2 charstring work"));
            }
            let b0 = code[i];
            i += 1;
            match b0 {
                32..=246 => self.push(f64::from(b0) - 139.0)?,
                247..=250 => {
                    let b1 = byte(code, &mut i)?;
                    self.push(f64::from(b0 - 247) * 256.0 + f64::from(b1) + 108.0)?;
                }
                251..=254 => {
                    let b1 = byte(code, &mut i)?;
                    self.push(-f64::from(b0 - 251) * 256.0 - f64::from(b1) - 108.0)?;
                }
                28 => {
                    let v = i16::from_be_bytes([byte(code, &mut i)?, byte(code, &mut i)?]);
                    self.push(f64::from(v))?;
                }
                255 => {
                    let bytes = [
                        byte(code, &mut i)?,
                        byte(code, &mut i)?,
                        byte(code, &mut i)?,
                        byte(code, &mut i)?,
                    ];
                    self.push(f64::from(i32::from_be_bytes(bytes)) / 65536.0)?;
                }
                1 | 3 | 18 | 23 => {
                    self.take_width(self.stack.len() % 2 == 1);
                    self.stems += self.stack.len() / 2;
                    self.stack.clear();
                }
                19 | 20 => {
                    self.take_width(self.stack.len() % 2 == 1);
                    self.stems += self.stack.len() / 2;
                    self.stack.clear();
                    i += self.stems.div_ceil(8);
                }
                21 => {
                    self.take_width(self.stack.len() > 2);
                    let dy = self.pop()?;
                    let dx = self.pop()?;
                    self.stack.clear();
                    self.move_rel(dx, dy)?;
                }
                22 => {
                    self.take_width(self.stack.len() > 1);
                    let dx = self.pop()?;
                    self.stack.clear();
                    self.move_rel(dx, 0.0)?;
                }
                4 => {
                    self.take_width(self.stack.len() > 1);
                    let dy = self.pop()?;
                    self.stack.clear();
                    self.move_rel(0.0, dy)?;
                }
                5 => {
                    let args = std::mem::take(&mut self.stack);
                    for &[dx, dy] in args.as_chunks::<2>().0 {
                        self.line_rel(dx, dy)?;
                    }
                    self.stack = args;
                    self.stack.clear();
                }
                6 | 7 => {
                    let args = std::mem::take(&mut self.stack);
                    let mut horizontal = b0 == 6;
                    for &d in &args {
                        if horizontal {
                            self.line_rel(d, 0.0)?;
                        } else {
                            self.line_rel(0.0, d)?;
                        }
                        horizontal = !horizontal;
                    }
                    self.stack = args;
                    self.stack.clear();
                }
                8 => {
                    let args = std::mem::take(&mut self.stack);
                    for c in args.as_chunks::<6>().0 {
                        self.curve_rel(*c)?;
                    }
                    self.stack = args;
                    self.stack.clear();
                }
                24 => {
                    let args = std::mem::take(&mut self.stack);
                    if args.len() >= 8 {
                        let curves = (args.len() - 2) / 6;
                        for c in args[..curves * 6].as_chunks::<6>().0 {
                            self.curve_rel(*c)?;
                        }
                        let rest = &args[curves * 6..];
                        if rest.len() >= 2 {
                            self.line_rel(rest[0], rest[1])?;
                        }
                    }
                    self.stack = args;
                    self.stack.clear();
                }
                25 => {
                    let args = std::mem::take(&mut self.stack);
                    if args.len() >= 8 {
                        let lines = (args.len() - 6) / 2;
                        for &[dx, dy] in args[..lines * 2].as_chunks::<2>().0 {
                            self.line_rel(dx, dy)?;
                        }
                        let c = &args[lines * 2..];
                        if c.len() >= 6 {
                            self.curve_rel([c[0], c[1], c[2], c[3], c[4], c[5]])?;
                        }
                    }
                    self.stack = args;
                    self.stack.clear();
                }
                26 => {
                    let args = std::mem::take(&mut self.stack);
                    let (mut dx1, rest) = if args.len() % 4 == 1 {
                        (args[0], &args[1..])
                    } else {
                        (0.0, &args[..])
                    };
                    for &[a, b, c, d] in rest.as_chunks::<4>().0 {
                        self.curve_rel([dx1, a, b, c, 0.0, d])?;
                        dx1 = 0.0;
                    }
                    self.stack = args;
                    self.stack.clear();
                }
                27 => {
                    let args = std::mem::take(&mut self.stack);
                    let (mut dy1, rest) = if args.len() % 4 == 1 {
                        (args[0], &args[1..])
                    } else {
                        (0.0, &args[..])
                    };
                    for &[a, b, c, d] in rest.as_chunks::<4>().0 {
                        self.curve_rel([a, dy1, b, c, d, 0.0])?;
                        dy1 = 0.0;
                    }
                    self.stack = args;
                    self.stack.clear();
                }
                30 | 31 => {
                    let args = std::mem::take(&mut self.stack);
                    self.alternating_curves(&args, b0 == 31)?;
                    self.stack = args;
                    self.stack.clear();
                }
                10 | 29 => match self.call_subr(b0 == 29, depth)? {
                    Flow::End => return Ok(Flow::End),
                    Flow::Continue | Flow::Return => {}
                },
                11 => return Ok(Flow::Return),
                14 => {
                    self.take_width(self.stack.len() == 1 || self.stack.len() == 5);
                    if self.stack.len() >= 4 {
                        let n = self.stack.len();
                        let code_of = |v: f64| u8::try_from(v as i64).ok();
                        if let (Some(base_code), Some(accent_code)) =
                            (code_of(self.stack[n - 2]), code_of(self.stack[n - 1]))
                        {
                            self.seac = Some(Seac {
                                adx: self.stack[n - 4],
                                ady: self.stack[n - 3],
                                base_code,
                                accent_code,
                            });
                        }
                    }
                    self.stack.clear();
                    self.b.close()?;
                    return Ok(Flow::End);
                }
                12 => {
                    let b1 = byte(code, &mut i)?;
                    self.escape(b1)?;
                }
                // Reserved operators: clear the operands and carry on.
                _ => self.stack.clear(),
            }
        }
        Ok(Flow::Continue)
    }

    fn alternating_curves(&mut self, args: &[f64], start_horizontal: bool) -> Result<()> {
        let mut horizontal = start_horizontal;
        let mut k = 0usize;
        while args.len() - k >= 4 {
            let last = args.len() - k == 5;
            let extra = if last { args[k + 4] } else { 0.0 };
            if horizontal {
                self.curve_rel([args[k], 0.0, args[k + 1], args[k + 2], extra, args[k + 3]])?;
            } else {
                self.curve_rel([0.0, args[k], args[k + 1], args[k + 2], args[k + 3], extra])?;
            }
            k += if last { 5 } else { 4 };
            horizontal = !horizontal;
        }
        Ok(())
    }

    fn escape(&mut self, op: u8) -> Result<()> {
        match op {
            // dotsection (deprecated)
            0 => self.stack.clear(),
            3 => {
                let (b, a) = (self.pop()?, self.pop()?);
                self.push(f64::from(u8::from(a != 0.0 && b != 0.0)))?;
            }
            4 => {
                let (b, a) = (self.pop()?, self.pop()?);
                self.push(f64::from(u8::from(a != 0.0 || b != 0.0)))?;
            }
            5 => {
                let a = self.pop()?;
                self.push(f64::from(u8::from(a == 0.0)))?;
            }
            9 => {
                let a = self.pop()?;
                self.push(a.abs())?;
            }
            10 => {
                let (b, a) = (self.pop()?, self.pop()?);
                self.push(a + b)?;
            }
            11 => {
                let (b, a) = (self.pop()?, self.pop()?);
                self.push(a - b)?;
            }
            12 => {
                let (b, a) = (self.pop()?, self.pop()?);
                self.push(if b == 0.0 { 0.0 } else { a / b })?;
            }
            14 => {
                let a = self.pop()?;
                self.push(-a)?;
            }
            15 => {
                let (b, a) = (self.pop()?, self.pop()?);
                self.push(f64::from(u8::from(a == b)))?;
            }
            18 => {
                self.pop()?;
            }
            20 => {
                let idx = self.pop()?;
                let val = self.pop()?;
                if let Some(slot) = usize::try_from(idx as i64)
                    .ok()
                    .and_then(|i| self.transient.get_mut(i))
                {
                    *slot = val;
                }
            }
            21 => {
                let idx = self.pop()?;
                let val = usize::try_from(idx as i64)
                    .ok()
                    .and_then(|i| self.transient.get(i))
                    .copied()
                    .unwrap_or(0.0);
                self.push(val)?;
            }
            22 => {
                let (v2, v1, s2, s1) = (self.pop()?, self.pop()?, self.pop()?, self.pop()?);
                self.push(if v1 <= v2 { s1 } else { s2 })?;
            }
            23 => {
                // Deterministic xorshift in (0, 1].
                self.seed ^= self.seed << 13;
                self.seed ^= self.seed >> 17;
                self.seed ^= self.seed << 5;
                self.push((f64::from(self.seed % 65535) + 1.0) / 65536.0)?;
            }
            24 => {
                let (b, a) = (self.pop()?, self.pop()?);
                self.push(a * b)?;
            }
            26 => {
                let a = self.pop()?;
                self.push(a.abs().sqrt())?;
            }
            27 => {
                let a = *self
                    .stack
                    .last()
                    .ok_or(FontError::Malformed("Type 2 operand stack underflow"))?;
                self.push(a)?;
            }
            28 => {
                let (b, a) = (self.pop()?, self.pop()?);
                self.push(b)?;
                self.push(a)?;
            }
            29 => {
                let idx = self.pop()?;
                let len = self.stack.len();
                if len == 0 {
                    return Err(FontError::Malformed("Type 2 operand stack underflow"));
                }
                let k = if idx < 0.0 {
                    0
                } else {
                    (idx as usize).min(len - 1)
                };
                let v = self.stack[len - 1 - k];
                self.push(v)?;
            }
            30 => {
                let j = self.pop()? as i64;
                let n = self.pop()? as i64;
                let len = self.stack.len() as i64;
                if n > 0 && n <= len {
                    let n = n as usize;
                    let start = self.stack.len() - n;
                    let shift = j.rem_euclid(n as i64) as usize;
                    self.stack[start..].rotate_right(shift);
                }
            }
            34 => {
                let a = self.take_args(7)?;
                self.curve_rel([a[0], 0.0, a[1], a[2], a[3], 0.0])?;
                self.curve_rel([a[4], 0.0, a[5], -a[2], a[6], 0.0])?;
            }
            35 => {
                let a = self.take_args(13)?;
                self.curve_rel([a[0], a[1], a[2], a[3], a[4], a[5]])?;
                self.curve_rel([a[6], a[7], a[8], a[9], a[10], a[11]])?;
            }
            36 => {
                let a = self.take_args(9)?;
                self.curve_rel([a[0], a[1], a[2], a[3], a[4], 0.0])?;
                self.curve_rel([a[5], 0.0, a[6], a[7], a[8], -(a[1] + a[3] + a[7])])?;
            }
            37 => {
                let a = self.take_args(11)?;
                let dx: f64 = a[0] + a[2] + a[4] + a[6] + a[8];
                let dy: f64 = a[1] + a[3] + a[5] + a[7] + a[9];
                let (dx6, dy6) = if dx.abs() > dy.abs() {
                    (a[10], -dy)
                } else {
                    (-dx, a[10])
                };
                self.curve_rel([a[0], a[1], a[2], a[3], a[4], a[5]])?;
                self.curve_rel([a[6], a[7], a[8], a[9], dx6, dy6])?;
            }
            // Reserved escape operators.
            _ => self.stack.clear(),
        }
        Ok(())
    }

    /// Removes the bottom `n` operands for a flex operator; the rest of the stack is dropped.
    fn take_args(&mut self, n: usize) -> Result<Vec<f64>> {
        if self.stack.len() < n {
            return Err(FontError::Malformed("Type 2 flex operands"));
        }
        let out = self.stack[..n].to_vec();
        self.stack.clear();
        Ok(out)
    }
}
