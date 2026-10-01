//! ICC colour management that reproduces what MuPDF gets from Little-CMS 2.
//!
//! MuPDF converts every colour through an lcms2 link built with relative
//! colorimetric intent, black point compensation and
//! `cmsFLAGS_LOWRESPRECALC`: the source→PCS→destination chain is sampled into
//! a 16-bit grid (33 points for one channel, 17 otherwise) which lcms2 then
//! walks with fixed-point tetrahedral/linear interpolation. This module
//! rebuilds that chain (ICC v2/v4 parsing, matrix/TRC and LUT profiles, BPC
//! detection, PCS conversions, the grid resampling and the fixed-point
//! interpolators) so results agree with lcms2 at the 16-bit level, which keeps
//! 8-bit output within one level of PyMuPDF.
//!
//! The bundled profiles in `icc/` are MuPDF 1.27's `resources/icc/*.icc`
//! (Artifex Software, AGPL-3.0-only, the licence of this crate): they are the
//! profiles behind DeviceGray, DeviceRGB, DeviceCMYK and Lab in MuPDF.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex};

pub(crate) static GRAY_ICC: &[u8] = include_bytes!("icc/gray.icc");
pub(crate) static RGB_ICC: &[u8] = include_bytes!("icc/rgb.icc");
pub(crate) static CMYK_ICC: &[u8] = include_bytes!("icc/cmyk.icc");
pub(crate) static LAB_ICC: &[u8] = include_bytes!("icc/lab.icc");

const fn sig(s: &[u8; 4]) -> u32 {
    u32::from_be_bytes(*s)
}

const SIG_A2B: [u32; 4] = [sig(b"A2B0"), sig(b"A2B1"), sig(b"A2B2"), sig(b"A2B1")];
const SIG_B2A: [u32; 4] = [sig(b"B2A0"), sig(b"B2A1"), sig(b"B2A2"), sig(b"B2A1")];
const SIG_RXYZ: u32 = sig(b"rXYZ");
const SIG_GXYZ: u32 = sig(b"gXYZ");
const SIG_BXYZ: u32 = sig(b"bXYZ");
const SIG_RTRC: u32 = sig(b"rTRC");
const SIG_GTRC: u32 = sig(b"gTRC");
const SIG_BTRC: u32 = sig(b"bTRC");
const SIG_KTRC: u32 = sig(b"kTRC");
const SIG_WTPT: u32 = sig(b"wtpt");

const TYPE_CURV: u32 = sig(b"curv");
const TYPE_PARA: u32 = sig(b"para");
const TYPE_MFT1: u32 = sig(b"mft1");
const TYPE_MFT2: u32 = sig(b"mft2");
const TYPE_MAB: u32 = sig(b"mAB ");
const TYPE_MBA: u32 = sig(b"mBA ");

pub(crate) const SPACE_XYZ: u32 = sig(b"XYZ ");
pub(crate) const SPACE_LAB: u32 = sig(b"Lab ");
pub(crate) const SPACE_RGB: u32 = sig(b"RGB ");
pub(crate) const SPACE_GRAY: u32 = sig(b"GRAY");
pub(crate) const SPACE_CMYK: u32 = sig(b"CMYK");

const CLASS_INPUT: u32 = sig(b"scnr");
const CLASS_DISPLAY: u32 = sig(b"mntr");
const CLASS_OUTPUT: u32 = sig(b"prtr");
const CLASS_LINK: u32 = sig(b"link");
const CLASS_ABSTRACT: u32 = sig(b"abst");
const CLASS_NAMED: u32 = sig(b"nmcl");
const CLASS_COLORSPACE: u32 = sig(b"spac");

const D50: [f64; 3] = [0.9642, 1.0, 0.8249];
const MAX_ENCODEABLE_XYZ: f64 = 1.0 + 32767.0 / 32768.0;
const INP_ADJ: f64 = 1.0 / MAX_ENCODEABLE_XYZ;
const OUTP_ADJ: f64 = MAX_ENCODEABLE_XYZ;
const PERCEPTUAL_BLACK: [f64; 3] = [0.00336, 0.0034731, 0.00287];
const MATRIX_DET_TOLERANCE: f64 = 0.0001;
const CLOSE_ENOUGH: f64 = 1.0 / 65535.0;
const V2_TO_V4: f64 = 65535.0 / 65280.0;
const V4_TO_V2: f64 = 65280.0 / 65535.0;
const REVERSE_SAMPLES: usize = 4096;
const PRELINEARIZATION_POINTS: usize = 4096;
const MAX_NODES_IN_CURVE: usize = 4097;
const MAX_CHANNELS: usize = 16;

/// ICC rendering intent, numbered like the ICC header and lcms2.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Intent {
    Perceptual = 0,
    RelativeColorimetric = 1,
    Saturation = 2,
    AbsoluteColorimetric = 3,
}

impl Intent {
    /// `fz_lookup_rendering_intent`: an unknown name is relative colorimetric.
    pub(crate) fn from_pdf_name(name: &[u8]) -> Intent {
        match name {
            b"Perceptual" => Intent::Perceptual,
            b"Saturation" => Intent::Saturation,
            b"AbsoluteColorimetric" => Intent::AbsoluteColorimetric,
            _ => Intent::RelativeColorimetric,
        }
    }
}

// ---------------------------------------------------------------------------
// lcms2 fixed-point helpers (lcms2_internal.h, cmsintrp.c)
// ---------------------------------------------------------------------------

#[inline]
fn to_fixed_domain(a: i32) -> i32 {
    a + ((a + 0x7fff) / 0xffff)
}

#[inline]
fn fixed_to_int(x: i32) -> i32 {
    x >> 16
}

#[inline]
fn fixed_rest(x: i32) -> i32 {
    x & 0xffff
}

#[inline]
fn round_fixed_to_int(x: i32) -> i32 {
    x.wrapping_add(0x8000) >> 16
}

#[inline]
fn linear_interp(a: i32, l: i32, h: i32) -> u16 {
    let dif = (h.wrapping_sub(l) as u32)
        .wrapping_mul(a as u32)
        .wrapping_add(0x8000);
    (dif >> 16).wrapping_add(l as u32) as u16
}

#[inline]
fn quick_saturate_word(d: f64) -> u16 {
    let d = d + 0.5;
    if d <= 0.0 {
        0
    } else if d >= 65535.0 {
        0xffff
    } else {
        d.floor() as u16
    }
}

#[inline]
fn from_8_to_16(v: u8) -> u16 {
    (u16::from(v) << 8) | u16::from(v)
}

fn quantize_val(i: usize, max_samples: usize) -> u16 {
    quick_saturate_word(i as f64 * 65535.0 / (max_samples - 1) as f64)
}

// ---------------------------------------------------------------------------
// CIE helpers (cmspcs.c)
// ---------------------------------------------------------------------------

fn lab_f(t: f64) -> f64 {
    const LIMIT: f64 = (24.0 / 116.0) * (24.0 / 116.0) * (24.0 / 116.0);
    if t <= LIMIT {
        (841.0 / 108.0) * t + (16.0 / 116.0)
    } else {
        t.cbrt()
    }
}

fn lab_f_inv(t: f64) -> f64 {
    const LIMIT: f64 = 24.0 / 116.0;
    if t <= LIMIT {
        (108.0 / 841.0) * (t - (16.0 / 116.0))
    } else {
        t * t * t
    }
}

fn xyz_to_lab(xyz: [f64; 3]) -> [f64; 3] {
    let fx = lab_f(xyz[0] / D50[0]);
    let fy = lab_f(xyz[1] / D50[1]);
    let fz = lab_f(xyz[2] / D50[2]);
    [116.0 * fy - 16.0, 500.0 * (fx - fy), 200.0 * (fy - fz)]
}

fn lab_to_xyz(lab: [f64; 3]) -> [f64; 3] {
    let y = (lab[0] + 16.0) / 116.0;
    let x = y + 0.002 * lab[1];
    let z = y - 0.005 * lab[2];
    [
        lab_f_inv(x) * D50[0],
        lab_f_inv(y) * D50[1],
        lab_f_inv(z) * D50[2],
    ]
}

// ---------------------------------------------------------------------------
// Tone curves (cmsgamma.c)
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
enum Curve {
    /// Tabulated 16-bit curve (`curv` with two or more entries, LUT tables).
    Table(Vec<u16>),
    /// Parametric curve in lcms2 numbering (ICC `para` type + 1, `curv`
    /// gamma = type 1); negative kinds are the analytic inverses.
    Param { kind: i32, p: [f64; 7] },
}

impl Curve {
    fn gamma(g: f64) -> Curve {
        Curve::Param {
            kind: 1,
            p: [g, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0],
        }
    }

    /// `cmsEvalToneCurveFloat`.
    fn eval_f32(&self, v: f32) -> f32 {
        match self {
            Curve::Table(t) => {
                let i = quick_saturate_word(f64::from(v) * 65535.0);
                (f64::from(lin_lerp_1d(t, i)) / 65535.0) as f32
            }
            Curve::Param { kind, p } => eval_parametric(*kind, p, f64::from(v)) as f32,
        }
    }

    /// `cmsReverseToneCurveEx` with 4096 samples.
    fn reversed(&self) -> Curve {
        match self {
            Curve::Param { kind, p } if *kind > 0 => Curve::Param { kind: -kind, p: *p },
            Curve::Param { .. } => self.clone(),
            Curve::Table(t) => Curve::Table(reverse_table(t)),
        }
    }
}

/// `LinLerp1D`: fixed-point linear interpolation over a 16-bit table.
fn lin_lerp_1d(t: &[u16], v: u16) -> u16 {
    let domain = t.len() as i32 - 1;
    if v == 0xffff || domain == 0 {
        return t[domain as usize];
    }
    let val3 = to_fixed_domain(domain * i32::from(v));
    let cell0 = fixed_to_int(val3) as usize;
    let rest = fixed_rest(val3);
    linear_interp(rest, i32::from(t[cell0]), i32::from(t[cell0 + 1]))
}

/// `DefaultEvalParametricFn` for lcms2 types ±1..=±5.
fn eval_parametric(kind: i32, p: &[f64; 7], r: f64) -> f64 {
    let tol = MATRIX_DET_TOLERANCE;
    match kind {
        1 => {
            if r < 0.0 {
                if (p[0] - 1.0).abs() < tol { r } else { 0.0 }
            } else {
                r.powf(p[0])
            }
        }
        -1 => {
            if r < 0.0 {
                if (p[0] - 1.0).abs() < tol { r } else { 0.0 }
            } else if p[0].abs() < tol {
                f64::INFINITY
            } else {
                r.powf(1.0 / p[0])
            }
        }
        2 => {
            if p[1].abs() < tol {
                0.0
            } else {
                let disc = -p[2] / p[1];
                if r >= disc {
                    let e = p[1] * r + p[2];
                    if e > 0.0 { e.powf(p[0]) } else { 0.0 }
                } else {
                    0.0
                }
            }
        }
        -2 => {
            if p[0].abs() < tol || p[1].abs() < tol || r < 0.0 {
                0.0
            } else {
                ((r.powf(1.0 / p[0]) - p[2]) / p[1]).max(0.0)
            }
        }
        3 => {
            if p[1].abs() < tol {
                0.0
            } else {
                let disc = (-p[2] / p[1]).max(0.0);
                if r >= disc {
                    let e = p[1] * r + p[2];
                    if e > 0.0 { e.powf(p[0]) + p[3] } else { 0.0 }
                } else {
                    p[3]
                }
            }
        }
        -3 => {
            if p[0].abs() < tol || p[1].abs() < tol {
                0.0
            } else if r >= p[3] {
                let e = r - p[3];
                if e > 0.0 {
                    (e.powf(1.0 / p[0]) - p[2]) / p[1]
                } else {
                    0.0
                }
            } else {
                -p[2] / p[1]
            }
        }
        4 => {
            if r >= p[4] {
                let e = p[1] * r + p[2];
                if e > 0.0 { e.powf(p[0]) } else { 0.0 }
            } else {
                r * p[3]
            }
        }
        -4 => {
            let e = p[1] * p[4] + p[2];
            let disc = if e < 0.0 { 0.0 } else { e.powf(p[0]) };
            if r >= disc {
                if p[0].abs() < tol || p[1].abs() < tol {
                    0.0
                } else {
                    (r.powf(1.0 / p[0]) - p[2]) / p[1]
                }
            } else if p[3].abs() < tol {
                0.0
            } else {
                r / p[3]
            }
        }
        5 => {
            if r >= p[4] {
                let e = p[1] * r + p[2];
                if e > 0.0 { e.powf(p[0]) + p[5] } else { p[5] }
            } else {
                r * p[3] + p[6]
            }
        }
        -5 => {
            let disc = p[3] * p[4] + p[6];
            if r >= disc {
                let e = r - p[5];
                if e < 0.0 || p[0].abs() < tol || p[1].abs() < tol {
                    0.0
                } else {
                    (e.powf(1.0 / p[0]) - p[2]) / p[1]
                }
            } else if p[3].abs() < tol {
                0.0
            } else {
                (r - p[6]) / p[3]
            }
        }
        _ => r,
    }
}

/// `GetInterval`: the table cell containing an output value. Ascending
/// tables take the highest matching cell, descending ones the lowest, as
/// lcms2's scan order does.
fn get_interval(input: f64, t: &[u16]) -> Option<usize> {
    let domain = t.len() - 1;
    if domain < 1 {
        return None;
    }
    if t[0] < t[domain] {
        (0..domain).rev().find(|&i| cell_contains(t, i, input))
    } else {
        (0..domain).find(|&i| cell_contains(t, i, input))
    }
}

fn cell_contains(t: &[u16], i: usize, input: f64) -> bool {
    let y0 = f64::from(t[i]);
    let y1 = f64::from(t[i + 1]);
    if y0 <= y1 {
        input >= y0 && input <= y1
    } else {
        input >= y1 && input <= y0
    }
}

/// Monotone (non-decreasing) tables are searched with a moving cursor
/// instead of lcms2's full scan per sample: the highest cell containing a
/// value never moves down as the value grows, so the answer is the same.
fn table_is_monotone(t: &[u16]) -> bool {
    t.windows(2).all(|w| w[0] <= w[1])
}

fn reverse_table(t: &[u16]) -> Vec<u16> {
    let ascending = t[0] <= t[t.len() - 1];
    let monotone = table_is_monotone(t);
    let domain = t.len() - 1;
    let entries = domain as f64;
    let mut out = vec![0u16; REVERSE_SAMPLES];
    let (mut a, mut b) = (0.0f64, 0.0f64);
    let mut cursor = 0usize;
    for (i, slot) in out.iter_mut().enumerate() {
        let y = i as f64 * 65535.0 / (REVERSE_SAMPLES - 1) as f64;
        let interval = if monotone && domain >= 1 {
            while cursor + 1 < domain && f64::from(t[cursor + 1]) < y {
                cursor += 1;
            }
            while cursor + 1 < domain && cell_contains(t, cursor + 1, y) {
                cursor += 1;
            }
            cell_contains(t, cursor, y).then_some(cursor)
        } else {
            get_interval(y, t)
        };
        if let Some(j) = interval {
            let x1 = f64::from(t[j]);
            let x2 = f64::from(t[j + 1]);
            let y1 = j as f64 * 65535.0 / entries;
            let y2 = (j + 1) as f64 * 65535.0 / entries;
            if x1 == x2 {
                *slot = quick_saturate_word(if ascending { y2 } else { y1 });
                continue;
            }
            a = (y2 - y1) / (x2 - x1);
            b = y2 - a * x2;
        }
        *slot = quick_saturate_word(a * y + b);
    }
    out
}

/// `cmsIsToneCurveLinear` on a tabulated curve.
fn table_is_linear(t: &[u16]) -> bool {
    t.iter()
        .enumerate()
        .all(|(i, &v)| (i32::from(v) - i32::from(quantize_val(i, t.len()))).abs() <= 0x0f)
}

// ---------------------------------------------------------------------------
// Pipeline stages (cmslut.c)
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MatrixKind {
    Plain,
    V2ToV4,
    V4ToV2,
}

#[derive(Clone, Debug)]
struct MatrixStage {
    rows: usize,
    cols: usize,
    m: Vec<f64>,
    offset: Option<Vec<f64>>,
    kind: MatrixKind,
}

#[derive(Clone, Debug)]
struct Clut {
    n_in: usize,
    n_out: usize,
    domain: [i32; 4],
    opta: [i32; 4],
    table: Vec<u16>,
    trilinear: bool,
}

#[derive(Clone, Debug)]
enum Stage {
    Curves(Vec<Curve>),
    Matrix(MatrixStage),
    Clut(Clut),
    Xyz2Lab,
    Lab2Xyz,
}

impl MatrixStage {
    fn square(m: [f64; 9], offset: Option<[f64; 3]>, kind: MatrixKind) -> MatrixStage {
        MatrixStage {
            rows: 3,
            cols: 3,
            m: m.to_vec(),
            offset: offset.map(|o| o.to_vec()),
            kind,
        }
    }

    fn eval(&self, input: &[f32], out: &mut [f32]) {
        for i in 0..self.rows {
            let mut tmp = 0.0f64;
            for (j, x) in input.iter().enumerate().take(self.cols) {
                tmp += f64::from(*x) * self.m[i * self.cols + j];
            }
            if let Some(off) = &self.offset {
                tmp += off[i];
            }
            out[i] = tmp as f32;
        }
    }
}

impl Clut {
    fn new(grid: &[usize], n_in: usize, n_out: usize, table: Vec<u16>) -> Option<Clut> {
        if !(n_in == 1 || n_in == 3 || n_in == 4) || n_out == 0 || n_out > MAX_CHANNELS {
            return None;
        }
        let mut domain = [0i32; 4];
        let mut opta = [0i32; 4];
        let mut total = n_out;
        for i in 0..n_in {
            if grid[i] < 2 || grid[i] > 255 {
                return None;
            }
            domain[i] = grid[i] as i32 - 1;
            total = total.checked_mul(grid[i])?;
        }
        opta[0] = n_out as i32;
        for i in 1..n_in {
            opta[i] = opta[i - 1] * grid[n_in - i] as i32;
        }
        if table.len() != total {
            return None;
        }
        Some(Clut {
            n_in,
            n_out,
            domain,
            opta,
            table,
            trilinear: false,
        })
    }

    fn lerp16(&self, input: &[u16], out: &mut [u16]) {
        match self.n_in {
            1 => self.eval1(input, out),
            3 if self.trilinear => self.trilinear(input, out),
            3 => self.tetrahedral(input, out),
            _ => self.eval4(input, out),
        }
    }

    /// `Eval1Input`.
    fn eval1(&self, input: &[u16], out: &mut [u16]) {
        let t = &self.table;
        let d0 = self.domain[0];
        let o0 = self.opta[0] as usize;
        if input[0] == 0xffff || d0 == 0 {
            let y0 = d0 as usize * o0;
            out[..self.n_out].copy_from_slice(&t[y0..y0 + self.n_out]);
            return;
        }
        let fk = to_fixed_domain(i32::from(input[0]) * d0);
        let k0 = fixed_to_int(fk) as usize;
        let rk = fixed_rest(fk);
        let base0 = o0 * k0;
        let base1 = o0 * (k0 + 1);
        for c in 0..self.n_out {
            out[c] = linear_interp(rk, i32::from(t[base0 + c]), i32::from(t[base1 + c]));
        }
    }

    /// `TrilinearInterp16`.
    fn trilinear(&self, input: &[u16], out: &mut [u16]) {
        let t = &self.table;
        let fx = to_fixed_domain(i32::from(input[0]) * self.domain[0]);
        let fy = to_fixed_domain(i32::from(input[1]) * self.domain[1]);
        let fz = to_fixed_domain(i32::from(input[2]) * self.domain[2]);
        let (rx, ry, rz) = (fixed_rest(fx), fixed_rest(fy), fixed_rest(fz));
        let x0 = self.opta[2] * fixed_to_int(fx);
        let x1 = x0 + if input[0] == 0xffff { 0 } else { self.opta[2] };
        let y0 = self.opta[1] * fixed_to_int(fy);
        let y1 = y0 + if input[1] == 0xffff { 0 } else { self.opta[1] };
        let z0 = self.opta[0] * fixed_to_int(fz);
        let z1 = z0 + if input[2] == 0xffff { 0 } else { self.opta[0] };
        let lerp = |a: i32, l: i32, h: i32| -> i32 {
            i32::from((l + round_fixed_to_int((h - l).wrapping_mul(a))) as u16)
        };
        for c in 0..self.n_out {
            let dens = |i: i32, j: i32, k: i32| i32::from(t[(i + j + k) as usize + c]);
            let d000 = dens(x0, y0, z0);
            let d001 = dens(x0, y0, z1);
            let d010 = dens(x0, y1, z0);
            let d011 = dens(x0, y1, z1);
            let d100 = dens(x1, y0, z0);
            let d101 = dens(x1, y0, z1);
            let d110 = dens(x1, y1, z0);
            let d111 = dens(x1, y1, z1);
            let dx00 = lerp(rx, d000, d100);
            let dx01 = lerp(rx, d001, d101);
            let dx10 = lerp(rx, d010, d110);
            let dx11 = lerp(rx, d011, d111);
            let dxy0 = lerp(ry, dx00, dx10);
            let dxy1 = lerp(ry, dx01, dx11);
            out[c] = lerp(rz, dxy0, dxy1) as u16;
        }
    }

    /// `TetrahedralInterp16` (Sakamoto ordering, lcms2's rounding).
    fn tetrahedral(&self, input: &[u16], out: &mut [u16]) {
        let t = &self.table;
        let fx = to_fixed_domain(i32::from(input[0]) * self.domain[0]);
        let fy = to_fixed_domain(i32::from(input[1]) * self.domain[1]);
        let fz = to_fixed_domain(i32::from(input[2]) * self.domain[2]);
        let (rx, ry, rz) = (fixed_rest(fx), fixed_rest(fy), fixed_rest(fz));
        let base = (self.opta[2] * fixed_to_int(fx)
            + self.opta[1] * fixed_to_int(fy)
            + self.opta[0] * fixed_to_int(fz)) as usize;
        let mut x1 = if input[0] == 0xffff { 0 } else { self.opta[2] };
        let mut y1 = if input[1] == 0xffff { 0 } else { self.opta[1] };
        let mut z1 = if input[2] == 0xffff { 0 } else { self.opta[0] };
        // Which vertices form the tetrahedron and how their differences chain.
        let order = if rx >= ry {
            if ry >= rz {
                y1 += x1;
                z1 += y1;
                0
            } else if rz >= rx {
                x1 += z1;
                y1 += x1;
                1
            } else {
                z1 += x1;
                y1 += z1;
                2
            }
        } else if rx >= rz {
            x1 += y1;
            z1 += x1;
            3
        } else if ry >= rz {
            z1 += y1;
            x1 += z1;
            4
        } else {
            y1 += z1;
            x1 += y1;
            5
        };
        for c in 0..self.n_out {
            let at = |o: i32| i32::from(t[base + o as usize + c]);
            let c0 = at(0);
            let (mut c1, mut c2, mut c3) = (at(x1), at(y1), at(z1));
            match order {
                0 => {
                    c3 -= c2;
                    c2 -= c1;
                    c1 -= c0;
                }
                1 => {
                    c2 -= c1;
                    c1 -= c3;
                    c3 -= c0;
                }
                2 => {
                    c2 -= c3;
                    c3 -= c1;
                    c1 -= c0;
                }
                3 => {
                    c3 -= c1;
                    c1 -= c2;
                    c2 -= c0;
                }
                4 => {
                    c1 -= c3;
                    c3 -= c2;
                    c2 -= c0;
                }
                _ => {
                    c1 -= c2;
                    c2 -= c3;
                    c3 -= c0;
                }
            }
            let rest = c1
                .wrapping_mul(rx)
                .wrapping_add(c2.wrapping_mul(ry))
                .wrapping_add(c3.wrapping_mul(rz))
                .wrapping_add(0x8001);
            out[c] = (c0 + ((rest + (rest >> 16)) >> 16)) as u16;
        }
    }

    /// `Eval4Inputs`: two tetrahedral lookups on the first channel's two
    /// neighbouring hyperplanes, then linear interpolation between them.
    fn eval4(&self, input: &[u16], out: &mut [u16]) {
        let fk = to_fixed_domain(i32::from(input[0]) * self.domain[0]);
        let fx = to_fixed_domain(i32::from(input[1]) * self.domain[1]);
        let fy = to_fixed_domain(i32::from(input[2]) * self.domain[2]);
        let fz = to_fixed_domain(i32::from(input[3]) * self.domain[3]);
        let rk = fixed_rest(fk);
        let (rx, ry, rz) = (fixed_rest(fx), fixed_rest(fy), fixed_rest(fz));
        let k0 = self.opta[3] * fixed_to_int(fk);
        let k1 = k0 + if input[0] == 0xffff { 0 } else { self.opta[3] };
        let x0 = self.opta[2] * fixed_to_int(fx);
        let x1 = x0 + if input[1] == 0xffff { 0 } else { self.opta[2] };
        let y0 = self.opta[1] * fixed_to_int(fy);
        let y1 = y0 + if input[2] == 0xffff { 0 } else { self.opta[1] };
        let z0 = self.opta[0] * fixed_to_int(fz);
        let z1 = z0 + if input[3] == 0xffff { 0 } else { self.opta[0] };
        let mut tmp1 = [0u16; MAX_CHANNELS];
        let mut tmp2 = [0u16; MAX_CHANNELS];
        for (plane, tmp) in [(k0, &mut tmp1), (k1, &mut tmp2)] {
            let t = &self.table[plane as usize..];
            for c in 0..self.n_out {
                let dens = |i: i32, j: i32, k: i32| i32::from(t[(i + j + k) as usize + c]);
                let c0 = dens(x0, y0, z0);
                let (c1, c2, c3) = if rx >= ry && ry >= rz {
                    (
                        dens(x1, y0, z0) - c0,
                        dens(x1, y1, z0) - dens(x1, y0, z0),
                        dens(x1, y1, z1) - dens(x1, y1, z0),
                    )
                } else if rx >= rz && rz >= ry {
                    (
                        dens(x1, y0, z0) - c0,
                        dens(x1, y1, z1) - dens(x1, y0, z1),
                        dens(x1, y0, z1) - dens(x1, y0, z0),
                    )
                } else if rz >= rx && rx >= ry {
                    (
                        dens(x1, y0, z1) - dens(x0, y0, z1),
                        dens(x1, y1, z1) - dens(x1, y0, z1),
                        dens(x0, y0, z1) - c0,
                    )
                } else if ry >= rx && rx >= rz {
                    (
                        dens(x1, y1, z0) - dens(x0, y1, z0),
                        dens(x0, y1, z0) - c0,
                        dens(x1, y1, z1) - dens(x1, y1, z0),
                    )
                } else if ry >= rz && rz >= rx {
                    (
                        dens(x1, y1, z1) - dens(x0, y1, z1),
                        dens(x0, y1, z0) - c0,
                        dens(x0, y1, z1) - dens(x0, y1, z0),
                    )
                } else if rz >= ry && ry >= rx {
                    (
                        dens(x1, y1, z1) - dens(x0, y1, z1),
                        dens(x0, y1, z1) - dens(x0, y0, z1),
                        dens(x0, y0, z1) - c0,
                    )
                } else {
                    (0, 0, 0)
                };
                let rest = c1
                    .wrapping_mul(rx)
                    .wrapping_add(c2.wrapping_mul(ry))
                    .wrapping_add(c3.wrapping_mul(rz));
                tmp[c] = (c0 + round_fixed_to_int(to_fixed_domain(rest))) as u16;
            }
        }
        for c in 0..self.n_out {
            out[c] = linear_interp(rk, i32::from(tmp1[c]), i32::from(tmp2[c]));
        }
    }

    /// `EvaluateCLUTfloatIn16`.
    fn eval_float(&self, input: &[f32], out: &mut [f32]) {
        let mut in16 = [0u16; MAX_CHANNELS];
        let mut out16 = [0u16; MAX_CHANNELS];
        for (i, v) in input[..self.n_in].iter().enumerate() {
            in16[i] = quick_saturate_word(f64::from(*v) * 65535.0);
        }
        self.lerp16(&in16, &mut out16);
        for (o, v) in out[..self.n_out].iter_mut().zip(&out16) {
            *o = f32::from(*v) / 65535.0;
        }
    }
}

impl Stage {
    fn out_channels(&self, n_in: usize) -> usize {
        match self {
            Stage::Curves(c) => c.len(),
            Stage::Matrix(m) => m.rows,
            Stage::Clut(c) => c.n_out,
            Stage::Xyz2Lab | Stage::Lab2Xyz => n_in,
        }
    }

    fn eval(&self, input: &[f32], out: &mut [f32]) {
        match self {
            Stage::Curves(curves) => {
                for (i, c) in curves.iter().enumerate() {
                    out[i] = c.eval_f32(input[i]);
                }
            }
            Stage::Matrix(m) => m.eval(input, out),
            Stage::Clut(c) => c.eval_float(input, out),
            Stage::Lab2Xyz => {
                let lab = [
                    f64::from(input[0]) * 100.0,
                    f64::from(input[1]) * 255.0 - 128.0,
                    f64::from(input[2]) * 255.0 - 128.0,
                ];
                let xyz = lab_to_xyz(lab);
                for i in 0..3 {
                    out[i] = (xyz[i] / MAX_ENCODEABLE_XYZ) as f32;
                }
            }
            Stage::Xyz2Lab => {
                let xyz = [
                    f64::from(input[0]) * MAX_ENCODEABLE_XYZ,
                    f64::from(input[1]) * MAX_ENCODEABLE_XYZ,
                    f64::from(input[2]) * MAX_ENCODEABLE_XYZ,
                ];
                let lab = xyz_to_lab(xyz);
                out[0] = (lab[0] / 100.0) as f32;
                out[1] = ((lab[1] + 128.0) / 255.0) as f32;
                out[2] = ((lab[2] + 128.0) / 255.0) as f32;
            }
        }
    }
}

/// `_LUTevalFloat`: run the stages in single precision.
fn eval_pipeline_float(stages: &[Stage], input: &[f32], out: &mut [f32]) -> usize {
    let mut a = [0.0f32; MAX_CHANNELS];
    let mut b = [0.0f32; MAX_CHANNELS];
    let mut n = input.len();
    a[..n].copy_from_slice(input);
    for stage in stages {
        let n_out = stage.out_channels(n);
        stage.eval(&a[..n], &mut b[..n_out]);
        a[..n_out].copy_from_slice(&b[..n_out]);
        n = n_out;
    }
    out[..n].copy_from_slice(&a[..n]);
    n
}

/// `_LUTeval16`: 16-bit in and out around a single precision evaluation.
fn eval_pipeline_16(stages: &[Stage], input: &[u16], out: &mut [u16]) -> usize {
    let mut fin = [0.0f32; MAX_CHANNELS];
    let mut fout = [0.0f32; MAX_CHANNELS];
    for (i, v) in input.iter().enumerate() {
        fin[i] = f32::from(*v) / 65535.0;
    }
    let n = eval_pipeline_float(stages, &fin[..input.len()], &mut fout);
    for i in 0..n {
        out[i] = quick_saturate_word(f64::from(fout[i]) * 65535.0);
    }
    n
}

fn matrix_is_identity(m: &[f64]) -> bool {
    (0..3).all(|i| {
        (0..3).all(|j| (m[i * 3 + j] - if i == j { 1.0 } else { 0.0 }).abs() < CLOSE_ENOUGH)
    })
}

fn mat3_mul(a: &[f64], b: &[f64]) -> [f64; 9] {
    let mut r = [0.0; 9];
    for i in 0..3 {
        for j in 0..3 {
            r[i * 3 + j] = (0..3).map(|k| a[i * 3 + k] * b[k * 3 + j]).sum();
        }
    }
    r
}

fn mat3_inverse(m: &[f64]) -> Option<[f64; 9]> {
    let c0 = m[4] * m[8] - m[5] * m[7];
    let c1 = -m[3] * m[8] + m[5] * m[6];
    let c2 = m[3] * m[7] - m[4] * m[6];
    let det = m[0] * c0 + m[1] * c1 + m[2] * c2;
    if det.abs() < MATRIX_DET_TOLERANCE {
        return None;
    }
    Some([
        c0 / det,
        (m[2] * m[7] - m[1] * m[8]) / det,
        (m[1] * m[5] - m[2] * m[4]) / det,
        c1 / det,
        (m[0] * m[8] - m[2] * m[6]) / det,
        (m[2] * m[3] - m[0] * m[5]) / det,
        c2 / det,
        (m[1] * m[6] - m[0] * m[7]) / det,
        (m[0] * m[4] - m[1] * m[3]) / det,
    ])
}

/// `PreOptimize`: drop paired no-ops and merge adjacent plain 3x3 matrices.
fn pre_optimize(stages: &mut Vec<Stage>) {
    loop {
        let mut changed = false;
        let mut i = 0;
        while i + 1 < stages.len() {
            let drop_pair = match (&stages[i], &stages[i + 1]) {
                (Stage::Xyz2Lab, Stage::Lab2Xyz) | (Stage::Lab2Xyz, Stage::Xyz2Lab) => true,
                (Stage::Matrix(a), Stage::Matrix(b)) => {
                    (a.kind == MatrixKind::V4ToV2 && b.kind == MatrixKind::V2ToV4)
                        || (a.kind == MatrixKind::V2ToV4 && b.kind == MatrixKind::V4ToV2)
                }
                _ => false,
            };
            if drop_pair {
                stages.drain(i..i + 2);
                changed = true;
            } else {
                i += 1;
            }
        }
        // `_MultiplyMatrix` stops at the first pair it cannot merge.
        let mut i = 0;
        while i + 1 < stages.len() {
            if let (Stage::Matrix(a), Stage::Matrix(b)) = (&stages[i], &stages[i + 1]) {
                if a.offset.is_some()
                    || b.offset.is_some()
                    || a.rows != 3
                    || a.cols != 3
                    || b.rows != 3
                    || b.cols != 3
                {
                    break;
                }
                let res = mat3_mul(&b.m, &a.m);
                stages.drain(i..i + 2);
                if !matrix_is_identity(&res) {
                    stages.insert(
                        i,
                        Stage::Matrix(MatrixStage::square(res, None, MatrixKind::Plain)),
                    );
                }
                changed = true;
            } else {
                i += 1;
            }
        }
        if !changed {
            break;
        }
    }
}

// ---------------------------------------------------------------------------
// Profiles (cmsio0.c, cmstypes.c, cmsio1.c)
// ---------------------------------------------------------------------------

static NEXT_PROFILE_ID: AtomicU64 = AtomicU64::new(1);

/// A parsed ICC profile: header fields plus the tag directory over the raw
/// bytes. Only the pieces lcms2 touches when linking are decoded.
#[derive(Debug)]
pub(crate) struct Profile {
    id: u64,
    data: Vec<u8>,
    version: u32,
    class: u32,
    space: u32,
    pcs: u32,
    header_intent: u32,
    tags: Vec<(u32, usize, usize)>,
}

fn rd32(data: &[u8], off: usize) -> Option<u32> {
    data.get(off..off + 4)
        .map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
}

fn rd16(data: &[u8], off: usize) -> Option<u16> {
    data.get(off..off + 2)
        .map(|b| u16::from_be_bytes([b[0], b[1]]))
}

fn s15f16(v: u32) -> f64 {
    let fix = v as i32;
    let abs = fix.unsigned_abs();
    let whole = f64::from((abs >> 16) & 0xffff);
    let frac = f64::from(abs & 0xffff) / 65536.0;
    if fix < 0 {
        -(whole + frac)
    } else {
        whole + frac
    }
}

fn channels_of(space: u32) -> Option<usize> {
    match space {
        SPACE_GRAY => Some(1),
        SPACE_RGB | SPACE_LAB => Some(3),
        SPACE_CMYK => Some(4),
        _ => None,
    }
}

impl Profile {
    /// Parse a profile the way `cmsOpenProfileFromMem` + MuPDF's checks do;
    /// `None` is a broken profile or one with a channel count MuPDF rejects.
    pub(crate) fn parse(bytes: &[u8]) -> Option<Arc<Profile>> {
        if bytes.len() < 132 || &bytes[36..40] != b"acsp" {
            return None;
        }
        let count = rd32(bytes, 128)? as usize;
        if count > 100 {
            return None;
        }
        let mut tags = Vec::with_capacity(count);
        for i in 0..count {
            let at = 132 + i * 12;
            let sig = rd32(bytes, at)?;
            let off = rd32(bytes, at + 4)? as usize;
            let size = rd32(bytes, at + 8)? as usize;
            if off.checked_add(size).is_none_or(|end| end > bytes.len()) {
                continue;
            }
            tags.push((sig, off, size));
        }
        let space = rd32(bytes, 16)?;
        channels_of(space)?;
        Some(Arc::new(Profile {
            id: NEXT_PROFILE_ID.fetch_add(1, Ordering::Relaxed),
            data: bytes.to_vec(),
            version: rd32(bytes, 8)?,
            class: rd32(bytes, 12)?,
            space,
            pcs: rd32(bytes, 20)?,
            header_intent: rd32(bytes, 64)?,
            tags,
        }))
    }

    pub(crate) fn components(&self) -> usize {
        channels_of(self.space).unwrap_or(0)
    }

    pub(crate) fn is_lab(&self) -> bool {
        self.space == SPACE_LAB
    }

    /// MuPDF's md5 identity: the same object or the same bytes.
    pub(crate) fn same_profile(&self, other: &Profile) -> bool {
        self.id == other.id || self.data == other.data
    }

    fn tag(&self, sig: u32) -> Option<(usize, &[u8])> {
        self.tags
            .iter()
            .find(|t| t.0 == sig)
            .map(|&(_, off, size)| (off, &self.data[off..off + size]))
    }

    fn has_tag(&self, sig: u32) -> bool {
        self.tags.iter().any(|t| t.0 == sig)
    }

    fn is_matrix_shaper(&self) -> bool {
        match self.space {
            SPACE_GRAY => self.has_tag(SIG_KTRC),
            SPACE_RGB => [SIG_RXYZ, SIG_GXYZ, SIG_BXYZ, SIG_RTRC, SIG_GTRC, SIG_BTRC]
                .iter()
                .all(|&s| self.has_tag(s)),
            _ => false,
        }
    }

    /// `cmsIsCLUT`.
    fn is_clut(&self, intent: Intent, output: bool) -> bool {
        if self.class == CLASS_LINK {
            return self.header_intent == intent as u32;
        }
        let table = if output { &SIG_B2A } else { &SIG_A2B };
        self.has_tag(table[intent as usize])
    }

    /// `cmsIsIntentSupported`.
    fn supports_intent(&self, intent: Intent, output: bool) -> bool {
        self.is_clut(intent, output) || self.is_matrix_shaper()
    }

    fn read_curve_tag(&self, sig: u32) -> Option<Curve> {
        let (_, data) = self.tag(sig)?;
        read_curve(data).map(|(c, _)| c)
    }

    fn read_xyz_tag(&self, sig: u32) -> Option<[f64; 3]> {
        let (_, data) = self.tag(sig)?;
        Some([
            s15f16(rd32(data, 8)?),
            s15f16(rd32(data, 12)?),
            s15f16(rd32(data, 16)?),
        ])
    }

    /// `_cmsReadMediaWhitePoint`.
    fn media_white_point(&self) -> [f64; 3] {
        match self.read_xyz_tag(SIG_WTPT) {
            Some(wp) if !(self.version < 0x0400_0000 && self.class == CLASS_DISPLAY) => wp,
            _ => D50,
        }
    }

    /// `_cmsReadInputLUT`: device → PCS stages.
    fn input_stages(&self, intent: Intent) -> Option<Vec<Stage>> {
        let mut tag = SIG_A2B[intent as usize];
        if !self.has_tag(tag) {
            tag = SIG_A2B[0];
        }
        if let Some((off, data)) = self.tag(tag) {
            let (mut stages, kind) = read_lut_tag(&self.data, off, data, false)?;
            if kind == TYPE_MFT2 && self.pcs == SPACE_LAB {
                if self.space == SPACE_LAB {
                    stages.insert(0, lab_v4_to_v2());
                }
                stages.push(lab_v2_to_v4());
            }
            return Some(stages);
        }
        if self.space == SPACE_GRAY {
            // BuildGrayInputMatrixPipeline
            let trc = self.read_curve_tag(SIG_KTRC)?;
            let mut stages = Vec::new();
            if self.pcs == SPACE_LAB {
                stages.push(Stage::Matrix(MatrixStage {
                    rows: 3,
                    cols: 1,
                    m: vec![1.0, 1.0, 1.0],
                    offset: None,
                    kind: MatrixKind::Plain,
                }));
                let empty = Curve::Table(vec![0x8080, 0x8080]);
                stages.push(Stage::Curves(vec![trc, empty.clone(), empty]));
            } else {
                stages.push(Stage::Curves(vec![trc]));
                stages.push(Stage::Matrix(MatrixStage {
                    rows: 3,
                    cols: 1,
                    m: vec![INP_ADJ * D50[0], INP_ADJ * D50[1], INP_ADJ * D50[2]],
                    offset: None,
                    kind: MatrixKind::Plain,
                }));
            }
            return Some(stages);
        }
        // BuildRGBInputMatrixShaper
        let mat = self.colorant_matrix()?;
        let curves = vec![
            self.read_curve_tag(SIG_RTRC)?,
            self.read_curve_tag(SIG_GTRC)?,
            self.read_curve_tag(SIG_BTRC)?,
        ];
        let mut stages = vec![
            Stage::Curves(curves),
            Stage::Matrix(MatrixStage::square(
                mat.map(|v| v * INP_ADJ),
                None,
                MatrixKind::Plain,
            )),
        ];
        if self.pcs == SPACE_LAB {
            stages.push(Stage::Xyz2Lab);
        }
        Some(stages)
    }

    /// `_cmsReadOutputLUT`: PCS → device stages.
    fn output_stages(&self, intent: Intent) -> Option<Vec<Stage>> {
        let mut tag = SIG_B2A[intent as usize];
        if !self.has_tag(tag) {
            tag = SIG_B2A[0];
        }
        if let Some((off, data)) = self.tag(tag) {
            let (mut stages, kind) = read_lut_tag(&self.data, off, data, true)?;
            if self.pcs == SPACE_LAB {
                for stage in &mut stages {
                    if let Stage::Clut(c) = stage
                        && c.n_in == 3
                    {
                        c.trilinear = true;
                    }
                }
            }
            if kind == TYPE_MFT2 && self.pcs == SPACE_LAB {
                stages.insert(0, lab_v4_to_v2());
                if self.space == SPACE_LAB {
                    stages.push(lab_v2_to_v4());
                }
            }
            return Some(stages);
        }
        if self.space == SPACE_GRAY {
            // BuildGrayOutputPipeline
            let rev = self.read_curve_tag(SIG_KTRC)?.reversed();
            let pick = if self.pcs == SPACE_LAB {
                vec![1.0, 0.0, 0.0]
            } else {
                vec![0.0, OUTP_ADJ * D50[1], 0.0]
            };
            return Some(vec![
                Stage::Matrix(MatrixStage {
                    rows: 1,
                    cols: 3,
                    m: pick,
                    offset: None,
                    kind: MatrixKind::Plain,
                }),
                Stage::Curves(vec![rev]),
            ]);
        }
        // BuildRGBOutputMatrixShaper
        let inv = mat3_inverse(&self.colorant_matrix()?)?;
        let curves = vec![
            self.read_curve_tag(SIG_RTRC)?.reversed(),
            self.read_curve_tag(SIG_GTRC)?.reversed(),
            self.read_curve_tag(SIG_BTRC)?.reversed(),
        ];
        let mut stages = Vec::new();
        if self.pcs == SPACE_LAB {
            stages.push(Stage::Lab2Xyz);
        }
        stages.push(Stage::Matrix(MatrixStage::square(
            inv.map(|v| v * OUTP_ADJ),
            None,
            MatrixKind::Plain,
        )));
        stages.push(Stage::Curves(curves));
        Some(stages)
    }

    /// `ReadICCMatrixRGB2XYZ`: colorants as columns of a row-major matrix.
    fn colorant_matrix(&self) -> Option<[f64; 9]> {
        let r = self.read_xyz_tag(SIG_RXYZ)?;
        let g = self.read_xyz_tag(SIG_GXYZ)?;
        let b = self.read_xyz_tag(SIG_BXYZ)?;
        Some([r[0], g[0], b[0], r[1], g[1], b[1], r[2], g[2], b[2]])
    }
}

fn lab_v2_to_v4() -> Stage {
    Stage::Matrix(MatrixStage::square(
        [V2_TO_V4, 0.0, 0.0, 0.0, V2_TO_V4, 0.0, 0.0, 0.0, V2_TO_V4],
        None,
        MatrixKind::V2ToV4,
    ))
}

fn lab_v4_to_v2() -> Stage {
    Stage::Matrix(MatrixStage::square(
        [V4_TO_V2, 0.0, 0.0, 0.0, V4_TO_V2, 0.0, 0.0, 0.0, V4_TO_V2],
        None,
        MatrixKind::V4ToV2,
    ))
}

/// `Type_Curve_Read` / `Type_ParametricCurve_Read`; returns the curve and the
/// number of bytes it occupies (type header included).
fn read_curve(data: &[u8]) -> Option<(Curve, usize)> {
    match rd32(data, 0)? {
        TYPE_CURV => {
            let count = rd32(data, 8)? as usize;
            match count {
                0 => Some((Curve::gamma(1.0), 12)),
                1 => Some((Curve::gamma(f64::from(rd16(data, 12)?) / 256.0), 14)),
                n if n <= 0x7fff => {
                    let mut t = Vec::with_capacity(n);
                    for i in 0..n {
                        t.push(rd16(data, 12 + 2 * i)?);
                    }
                    Some((Curve::Table(t), 12 + 2 * n))
                }
                _ => None,
            }
        }
        TYPE_PARA => {
            let kind = rd16(data, 8)? as usize;
            let n = *[1usize, 3, 4, 5, 7].get(kind)?;
            let mut p = [0.0f64; 7];
            for (i, slot) in p.iter_mut().take(n).enumerate() {
                *slot = s15f16(rd32(data, 12 + 4 * i)?);
            }
            Some((
                Curve::Param {
                    kind: kind as i32 + 1,
                    p,
                },
                12 + 4 * n,
            ))
        }
        _ => None,
    }
}

/// `ReadSetOfCurves`: `n` embedded curves, each padded to four bytes.
fn read_curve_set(profile: &[u8], mut at: usize, n: usize) -> Option<Stage> {
    let mut curves = Vec::with_capacity(n);
    for _ in 0..n {
        let (curve, used) = read_curve(profile.get(at..)?)?;
        curves.push(curve);
        at = (at + used + 3) & !3;
    }
    Some(Stage::Curves(curves))
}

fn read_mft_matrix(data: &[u8], n_in: usize) -> Option<Option<Stage>> {
    let mut m = [0.0f64; 9];
    for (i, slot) in m.iter_mut().enumerate() {
        *slot = s15f16(rd32(data, 12 + 4 * i)?);
    }
    Some(
        (n_in == 3 && !matrix_is_identity(&m))
            .then(|| Stage::Matrix(MatrixStage::square(m, None, MatrixKind::Plain))),
    )
}

/// `Type_LUT8_Read`, `Type_LUT16_Read`, `Type_LUTA2B_Read`, `Type_LUTB2A_Read`.
/// Returns the stages and the tag type they came from.
fn read_lut_tag(profile: &[u8], off: usize, data: &[u8], b2a: bool) -> Option<(Vec<Stage>, u32)> {
    let kind = rd32(data, 0)?;
    let n_in = usize::from(*data.get(8)?);
    let n_out = usize::from(*data.get(9)?);
    if n_in == 0 || n_in > MAX_CHANNELS || n_out == 0 || n_out > MAX_CHANNELS {
        return None;
    }
    let mut stages = Vec::new();
    match kind {
        TYPE_MFT1 | TYPE_MFT2 => {
            let grid = usize::from(*data.get(10)?);
            if grid == 1 {
                return None;
            }
            if let Some(m) = read_mft_matrix(data, n_in)? {
                stages.push(m);
            }
            let clut_len = if grid == 0 {
                0
            } else {
                grid.checked_pow(n_in as u32)?.checked_mul(n_out)?
            };
            let mut at = 48;
            if kind == TYPE_MFT1 {
                let tables = |at: usize, n: usize| -> Option<Stage> {
                    let mut curves = Vec::with_capacity(n);
                    for c in 0..n {
                        let bytes = data.get(at + 256 * c..at + 256 * (c + 1))?;
                        curves.push(Curve::Table(
                            bytes.iter().map(|&b| from_8_to_16(b)).collect(),
                        ));
                    }
                    Some(Stage::Curves(curves))
                };
                stages.push(tables(at, n_in)?);
                at += 256 * n_in;
                if clut_len > 0 {
                    let bytes = data.get(at..at + clut_len)?;
                    let table = bytes.iter().map(|&b| from_8_to_16(b)).collect();
                    stages.push(Stage::Clut(Clut::new(&[grid; 4], n_in, n_out, table)?));
                    at += clut_len;
                }
                stages.push(tables(at, n_out)?);
            } else {
                let in_entries = usize::from(rd16(data, 48)?);
                let out_entries = usize::from(rd16(data, 50)?);
                if in_entries > 0x7fff || out_entries > 0x7fff {
                    return None;
                }
                at = 52;
                let tables =
                    |at: usize, n: usize, entries: usize| -> Option<(Option<Stage>, usize)> {
                        if entries == 0 {
                            return Some((None, 0));
                        }
                        if entries < 2 {
                            return None;
                        }
                        let mut curves = Vec::with_capacity(n);
                        for c in 0..n {
                            let base = at + 2 * entries * c;
                            let mut t = Vec::with_capacity(entries);
                            for i in 0..entries {
                                t.push(rd16(data, base + 2 * i)?);
                            }
                            curves.push(Curve::Table(t));
                        }
                        Some((Some(Stage::Curves(curves)), 2 * entries * n))
                    };
                let (stage, used) = tables(at, n_in, in_entries)?;
                stages.extend(stage);
                at += used;
                if clut_len > 0 {
                    let mut table = Vec::with_capacity(clut_len);
                    for i in 0..clut_len {
                        table.push(rd16(data, at + 2 * i)?);
                    }
                    stages.push(Stage::Clut(Clut::new(&[grid; 4], n_in, n_out, table)?));
                    at += 2 * clut_len;
                }
                let (stage, _) = tables(at, n_out, out_entries)?;
                stages.extend(stage);
            }
        }
        TYPE_MAB | TYPE_MBA => {
            if (kind == TYPE_MAB) == b2a {
                return None;
            }
            let off_b = rd32(data, 12)? as usize;
            let off_mat = rd32(data, 16)? as usize;
            let off_m = rd32(data, 20)? as usize;
            let off_c = rd32(data, 24)? as usize;
            let off_a = rd32(data, 28)? as usize;
            let matrix = |at: usize| -> Option<Stage> {
                let mut m = [0.0f64; 9];
                for (i, slot) in m.iter_mut().enumerate() {
                    *slot = s15f16(rd32(profile, at + 4 * i)?);
                }
                let mut o = [0.0f64; 3];
                for (i, slot) in o.iter_mut().enumerate() {
                    *slot = s15f16(rd32(profile, at + 36 + 4 * i)?);
                }
                Some(Stage::Matrix(MatrixStage::square(
                    m,
                    Some(o),
                    MatrixKind::Plain,
                )))
            };
            let clut = |at: usize| -> Option<Stage> {
                let mut grid = [0usize; 4];
                for (i, g) in grid.iter_mut().enumerate().take(n_in.min(4)) {
                    *g = usize::from(*profile.get(at + i)?);
                }
                let precision = *profile.get(at + 16)?;
                let len = grid[..n_in.min(4)]
                    .iter()
                    .try_fold(n_out, |acc, &g| acc.checked_mul(g))?;
                let table = match precision {
                    1 => profile
                        .get(at + 20..at + 20 + len)?
                        .iter()
                        .map(|&b| from_8_to_16(b))
                        .collect(),
                    2 => {
                        let mut t = Vec::with_capacity(len);
                        for i in 0..len {
                            t.push(rd16(profile, at + 20 + 2 * i)?);
                        }
                        t
                    }
                    _ => return None,
                };
                Some(Stage::Clut(Clut::new(&grid, n_in, n_out, table)?))
            };
            if kind == TYPE_MAB {
                if off_a != 0 {
                    stages.push(read_curve_set(profile, off + off_a, n_in)?);
                }
                if off_c != 0 {
                    stages.push(clut(off + off_c)?);
                }
                if off_m != 0 {
                    stages.push(read_curve_set(profile, off + off_m, n_out)?);
                }
                if off_mat != 0 {
                    stages.push(matrix(off + off_mat)?);
                }
                if off_b != 0 {
                    stages.push(read_curve_set(profile, off + off_b, n_out)?);
                }
            } else {
                if off_b != 0 {
                    stages.push(read_curve_set(profile, off + off_b, n_in)?);
                }
                if off_mat != 0 {
                    stages.push(matrix(off + off_mat)?);
                }
                if off_m != 0 {
                    stages.push(read_curve_set(profile, off + off_m, n_in)?);
                }
                if off_c != 0 {
                    stages.push(clut(off + off_c)?);
                }
                if off_a != 0 {
                    stages.push(read_curve_set(profile, off + off_a, n_out)?);
                }
            }
        }
        _ => return None,
    }
    Some((stages, kind))
}

// ---------------------------------------------------------------------------
// Black point detection (cmssamp.c) and intent conversion (cmscnvrt.c)
// ---------------------------------------------------------------------------

fn darkest_colorant(space: u32) -> Option<Vec<u16>> {
    match space {
        SPACE_GRAY => Some(vec![0]),
        SPACE_RGB => Some(vec![0, 0, 0]),
        SPACE_CMYK => Some(vec![0xffff; 4]),
        SPACE_LAB => Some(vec![0, 0x8080, 0x8080]),
        _ => None,
    }
}

fn white_colorant(space: u32) -> Option<Vec<u16>> {
    match space {
        SPACE_GRAY => Some(vec![0xffff]),
        SPACE_RGB => Some(vec![0xffff; 3]),
        SPACE_CMYK => Some(vec![0; 4]),
        SPACE_LAB => Some(vec![0xffff, 0x8080, 0x8080]),
        _ => None,
    }
}

fn lab_from_float(out: &[f32]) -> [f64; 3] {
    [
        f64::from(out[0]) * 100.0,
        f64::from(out[1]) * 255.0 - 128.0,
        f64::from(out[2]) * 255.0 - 128.0,
    ]
}

/// `BlackPointAsDarkerColorant`.
fn black_point_as_darker_colorant(profile: &Profile, intent: Intent) -> [f64; 3] {
    if !profile.supports_intent(intent, false) {
        return [0.0; 3];
    }
    let Some(black) = darkest_colorant(profile.space) else {
        return [0.0; 3];
    };
    let Some(mut stages) = profile.input_stages(intent) else {
        return [0.0; 3];
    };
    if profile.pcs == SPACE_XYZ {
        stages.push(Stage::Xyz2Lab);
    }
    pre_optimize(&mut stages);
    let input: Vec<f32> = black.iter().map(|&v| f32::from(v) / 65535.0).collect();
    let mut out = [0.0f32; MAX_CHANNELS];
    eval_pipeline_float(&stages, &input, &mut out);
    let mut lab = lab_from_float(&out);
    lab[0] = if lab[0] > 95.0 {
        0.0
    } else {
        lab[0].clamp(0.0, 50.0)
    };
    lab[1] = 0.0;
    lab[2] = 0.0;
    lab_to_xyz(lab)
}

/// `CreateRoundtripXForm` without optimisation: Lab → device through the
/// profile's `intent` output table, then back to Lab through its relative
/// colorimetric input table.
fn roundtrip_stages(profile: &Profile, intent: Intent) -> Option<Vec<Stage>> {
    let out_stages = profile.output_stages(intent)?;
    let in_stages = profile.input_stages(Intent::RelativeColorimetric)?;
    let mut stages = Vec::new();
    if profile.pcs == SPACE_XYZ {
        stages.push(Stage::Lab2Xyz);
    }
    stages.extend(out_stages);
    stages.extend(in_stages);
    if profile.pcs == SPACE_XYZ {
        stages.push(Stage::Xyz2Lab);
    }
    pre_optimize(&mut stages);
    Some(stages)
}

/// Lab in its natural range through a float pipeline over the Lab4 encoding.
fn roundtrip_lab(stages: &[Stage], lab: [f64; 3]) -> [f64; 3] {
    let input = [
        (lab[0] / 100.0) as f32,
        ((lab[1] + 128.0) / 255.0) as f32,
        ((lab[2] + 128.0) / 255.0) as f32,
    ];
    let mut out = [0.0f32; MAX_CHANNELS];
    eval_pipeline_float(stages, &input, &mut out);
    lab_from_float(&out)
}

/// `BlackPointUsingPerceptualBlack`: Lab black through the perceptual
/// PCS→device→PCS round trip of the profile.
fn black_point_using_perceptual_black(profile: &Profile) -> [f64; 3] {
    if !profile.supports_intent(Intent::Perceptual, false) {
        return [0.0; 3];
    }
    let Some(stages) = roundtrip_stages(profile, Intent::Perceptual) else {
        return [0.0; 3];
    };
    let mut lab = roundtrip_lab(&stages, [0.0, 0.0, 0.0]);
    if lab[0] > 50.0 {
        lab[0] = 50.0;
    }
    lab[1] = 0.0;
    lab[2] = 0.0;
    lab_to_xyz(lab)
}

/// `cmsDetectBlackPoint`.
fn detect_black_point(profile: &Profile, intent: Intent) -> [f64; 3] {
    if matches!(profile.class, CLASS_LINK | CLASS_ABSTRACT | CLASS_NAMED)
        || intent == Intent::AbsoluteColorimetric
    {
        return [0.0; 3];
    }
    if profile.version >= 0x0400_0000 && matches!(intent, Intent::Perceptual | Intent::Saturation) {
        if profile.is_matrix_shaper() {
            return black_point_as_darker_colorant(profile, Intent::RelativeColorimetric);
        }
        return PERCEPTUAL_BLACK;
    }
    if intent == Intent::RelativeColorimetric
        && profile.class == CLASS_OUTPUT
        && profile.space == SPACE_CMYK
    {
        return black_point_using_perceptual_black(profile);
    }
    black_point_as_darker_colorant(profile, intent)
}

/// `RootOfLeastSquaresFitQuadraticCurve`, including lcms2's clamp that
/// returns 0 for a linear fit.
fn root_of_quadratic_fit(x: &[f64], y: &[f64]) -> f64 {
    let n = x.len();
    if n < 4 {
        return 0.0;
    }
    let (mut sx, mut sx2, mut sx3, mut sx4) = (0.0, 0.0, 0.0, 0.0);
    let (mut sy, mut syx, mut syx2) = (0.0, 0.0, 0.0);
    for (&xn, &yn) in x.iter().zip(y) {
        sx += xn;
        sx2 += xn * xn;
        sx3 += xn * xn * xn;
        sx4 += xn * xn * xn * xn;
        sy += yn;
        syx += yn * xn;
        syx2 += yn * xn * xn;
    }
    let m = [n as f64, sx, sx2, sx, sx2, sx3, sx2, sx3, sx4];
    let Some(inv) = mat3_inverse(&m) else {
        return 0.0;
    };
    let v = [sy, syx, syx2];
    let res = transform_vector(&inv, &v);
    let (a, b, c) = (res[2], res[1], res[0]);
    if a.abs() < 1.0e-10 {
        return 0.0;
    }
    let d = b * b - 4.0 * a * c;
    if d <= 0.0 {
        return 0.0;
    }
    ((-b + d.sqrt()) / (2.0 * a)).clamp(0.0, 50.0)
}

/// `cmsDetectDestinationBlackPoint`: the black an output profile can
/// actually reproduce, found on its Lab round trip; other profiles take the
/// input-side detection.
fn detect_destination_black_point(profile: &Profile, intent: Intent) -> [f64; 3] {
    if matches!(profile.class, CLASS_LINK | CLASS_ABSTRACT | CLASS_NAMED)
        || intent == Intent::AbsoluteColorimetric
    {
        return [0.0; 3];
    }
    if profile.version >= 0x0400_0000 && matches!(intent, Intent::Perceptual | Intent::Saturation) {
        if profile.is_matrix_shaper() {
            return black_point_as_darker_colorant(profile, Intent::RelativeColorimetric);
        }
        return PERCEPTUAL_BLACK;
    }
    if !profile.is_clut(intent, true)
        || !matches!(profile.space, SPACE_GRAY | SPACE_RGB | SPACE_CMYK)
    {
        return detect_black_point(profile, intent);
    }
    let initial = if intent == Intent::RelativeColorimetric {
        xyz_to_lab(detect_black_point(profile, intent))
    } else {
        [0.0; 3]
    };
    let Some(stages) = roundtrip_stages(profile, intent) else {
        return [0.0; 3];
    };
    let a = initial[1].clamp(-50.0, 50.0);
    let b = initial[2].clamp(-50.0, 50.0);
    let mut in_ramp = [0.0f64; 256];
    let mut out_ramp = [0.0f64; 256];
    for l in 0..256 {
        in_ramp[l] = l as f64 * 100.0 / 255.0;
        out_ramp[l] = roundtrip_lab(&stages, [in_ramp[l], a, b])[0];
    }
    for l in (1..255).rev() {
        out_ramp[l] = out_ramp[l].min(out_ramp[l + 1]);
    }
    if out_ramp[0].partial_cmp(&out_ramp[255]) != Some(std::cmp::Ordering::Less) {
        return [0.0; 3];
    }
    let (min_l, max_l) = (out_ramp[0], out_ramp[255]);
    if intent == Intent::RelativeColorimetric {
        let nearly_straight = in_ramp
            .iter()
            .zip(&out_ramp)
            .all(|(&i, &o)| i <= min_l + 0.2 * (max_l - min_l) || (i - o).abs() < 4.0);
        if nearly_straight {
            return lab_to_xyz(initial);
        }
    }
    let (lo, hi) = if intent == Intent::RelativeColorimetric {
        (0.1, 0.5)
    } else {
        (0.03, 0.25)
    };
    let mut x = Vec::with_capacity(256);
    let mut y = Vec::with_capacity(256);
    for (&i, &o) in in_ramp.iter().zip(&out_ramp) {
        let ff = (o - min_l) / (max_l - min_l);
        if ff >= lo && ff < hi {
            x.push(i);
            y.push(ff);
        }
    }
    if x.len() < 3 {
        return [0.0; 3];
    }
    let l = root_of_quadratic_fit(&x, &y).max(0.0);
    lab_to_xyz([l, initial[1], initial[2]])
}

/// `ComputeConversion` + `AddConversion`: the PCS-to-PCS layer between the
/// two profiles (black point compensation or absolute white scaling).
fn add_conversion(
    stages: &mut Vec<Stage>,
    src: &Profile,
    dst: &Profile,
    intent: Intent,
    bpc: bool,
) {
    let mut m = [1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0];
    let mut off = [0.0f64; 3];
    if intent == Intent::AbsoluteColorimetric {
        let wp_in = src.media_white_point();
        let wp_out = dst.media_white_point();
        for i in 0..3 {
            m[i * 4] = wp_in[i] / wp_out[i];
        }
    } else if bpc {
        let bp_in = detect_black_point(src, intent);
        let bp_out = detect_destination_black_point(dst, intent);
        if bp_in != bp_out {
            for i in 0..3 {
                let t = bp_in[i] - D50[i];
                m[i * 4] = (bp_out[i] - D50[i]) / t;
                off[i] = -D50[i] * (bp_out[i] - bp_in[i]) / t;
            }
        }
    }
    for o in &mut off {
        *o /= MAX_ENCODEABLE_XYZ;
    }
    let diff: f64 = (0..9)
        .map(|i| (m[i] - if i % 4 == 0 { 1.0 } else { 0.0 }).abs())
        .sum::<f64>()
        + off.iter().map(|o| o.abs()).sum::<f64>();
    let layer = (diff >= 0.002)
        .then(|| Stage::Matrix(MatrixStage::square(m, Some(off), MatrixKind::Plain)));
    match (src.pcs, dst.pcs) {
        (SPACE_XYZ, SPACE_XYZ) => stages.extend(layer),
        (SPACE_XYZ, SPACE_LAB) => {
            stages.extend(layer);
            stages.push(Stage::Xyz2Lab);
        }
        (SPACE_LAB, SPACE_XYZ) => {
            stages.push(Stage::Lab2Xyz);
            stages.extend(layer);
        }
        _ => {
            if let Some(layer) = layer {
                stages.push(Stage::Lab2Xyz);
                stages.push(layer);
                stages.push(Stage::Xyz2Lab);
            }
        }
    }
}

/// `cmsDetectRGBProfileGamma`: the mean exponent of the profile's Y response,
/// or a negative number when it is not a plain power law.
fn detect_rgb_profile_gamma(profile: &Profile, threshold: f64) -> f64 {
    if profile.space != SPACE_RGB
        || !matches!(
            profile.class,
            CLASS_INPUT | CLASS_DISPLAY | CLASS_OUTPUT | CLASS_COLORSPACE
        )
    {
        return -1.0;
    }
    let Some(mut stages) = profile.input_stages(Intent::RelativeColorimetric) else {
        return -1.0;
    };
    if profile.pcs == SPACE_LAB {
        stages.push(Stage::Lab2Xyz);
    }
    pre_optimize(&mut stages);
    let mut samples = [0.0f32; 256];
    let mut out = [0.0f32; MAX_CHANNELS];
    for (i, s) in samples.iter_mut().enumerate() {
        let v = f32::from(from_8_to_16(i as u8)) / 65535.0;
        eval_pipeline_float(&stages, &[v, v, v], &mut out);
        *s = (f64::from(out[1]) * MAX_ENCODEABLE_XYZ) as f32;
    }
    // cmsEstimateGamma over a float-sampled curve (LinLerp1Dfloat).
    let eval = |x: f32| -> f64 {
        let v = if x < 1.0e-9 { 0.0 } else { x.min(1.0) };
        if v == 1.0 {
            return f64::from(samples[255]);
        }
        let v = v * 255.0;
        let cell0 = v.floor() as usize;
        let cell1 = v.ceil() as usize;
        let rest = v - cell0 as f32;
        let (y0, y1) = (samples[cell0], samples[cell1]);
        f64::from(y0 + (y1 - y0) * rest)
    };
    let (mut sum, mut sum2, mut n) = (0.0f64, 0.0f64, 0.0f64);
    for i in 1..MAX_NODES_IN_CURVE - 1 {
        let x = i as f64 / (MAX_NODES_IN_CURVE - 1) as f64;
        let y = eval(x as f32);
        if y > 0.0 && y < 1.0 && x > 0.07 {
            let g = y.ln() / x.ln();
            sum += g;
            sum2 += g * g;
            n += 1.0;
        }
    }
    if n <= 1.0 {
        return -1.0;
    }
    let std = ((n * sum2 - sum * sum) / (n * (n - 1.0))).sqrt();
    if std > threshold {
        return -1.0;
    }
    sum / n
}

// ---------------------------------------------------------------------------
// Transforms (cmsxform.c, cmsopt.c)
// ---------------------------------------------------------------------------

#[derive(Debug)]
enum Eval {
    Identity,
    Clut(Clut),
    Curves(Vec<Vec<u16>>),
    MatShaper(Box<MatShaper>),
    Float(Vec<Stage>),
}

/// A device link between two profiles: the optimised 16-bit evaluator
/// lcms2 would use for MuPDF's flags.
#[derive(Debug)]
pub(crate) struct Transform {
    n_in: usize,
    n_out: usize,
    lab_input: bool,
    eval: Eval,
}

impl Transform {
    fn identity(src: &Profile) -> Transform {
        Transform {
            n_in: src.components(),
            n_out: src.components(),
            lab_input: src.is_lab(),
            eval: Eval::Identity,
        }
    }

    /// `cmsCreateTransform(src, dst, intent, LOWRESPRECALC [| BLACKPOINTCOMPENSATION])`
    /// for 16-bit samples (`fz_icc_transform_color`) or, with `bits8`, for the
    /// 8-bit pixmap format MuPDF converts images with; `None` when either
    /// profile cannot be used.
    pub(crate) fn new(
        src: &Profile,
        dst: &Profile,
        intent: Intent,
        bpc: bool,
        bits8: bool,
    ) -> Option<Transform> {
        if src.class == CLASS_NAMED || dst.class == CLASS_NAMED {
            return None;
        }
        // _cmsLinkProfiles: no compensation for absolute colorimetric, and
        // always for a v4 destination under perceptual or saturation.
        let bpc = match intent {
            Intent::AbsoluteColorimetric => false,
            Intent::Perceptual | Intent::Saturation if dst.version >= 0x0400_0000 => true,
            _ => bpc,
        };
        let mut stages = src.input_stages(intent)?;
        let out = dst.output_stages(intent)?;
        add_conversion(&mut stages, src, dst, intent, bpc);
        stages.extend(out);
        pre_optimize(&mut stages);
        let n_in = src.components();
        let n_out = dst.components();
        let transform = |eval| Transform {
            n_in,
            n_out,
            lab_input: src.is_lab(),
            eval,
        };
        if stages.is_empty() {
            return Some(transform(Eval::Identity));
        }
        if src.space == SPACE_RGB {
            let gamma = detect_rgb_profile_gamma(src, 0.1);
            if gamma > 0.0 && gamma < 1.6 {
                return Some(transform(Eval::Float(stages)));
            }
        }
        if n_in == n_out && stages.iter().all(|s| matches!(s, Stage::Curves(_))) {
            return Some(transform(join_curves(&stages, n_in)));
        }
        if bits8
            && n_in == 3
            && n_out == 3
            && let Some(eval) = mat_shaper(&stages)
        {
            return Some(transform(eval));
        }
        // OptimizeByResampling with cmsFLAGS_LOWRESPRECALC grid sizes.
        let grid: usize = match n_in {
            1 => 33,
            n if n > 4 => 6,
            _ => 17,
        };
        let total = grid.pow(n_in as u32);
        let mut table = vec![0u16; total * n_out];
        let sample = |first: usize, chunk: &mut [u16]| {
            let mut input = [0u16; MAX_CHANNELS];
            let mut out = [0u16; MAX_CHANNELS];
            for (i, slot) in chunk.chunks_exact_mut(n_out).enumerate() {
                let mut rest = first + i;
                for t in (0..n_in).rev() {
                    input[t] = quantize_val(rest % grid, grid);
                    rest /= grid;
                }
                eval_pipeline_16(&stages, &input[..n_in], &mut out);
                slot.copy_from_slice(&out[..n_out]);
            }
        };
        // The CMYK grid is 83 521 nodes; sampling it once per process is
        // the only noticeable cost of a link, so spread it over the cores.
        let threads = if total >= 4096 {
            std::thread::available_parallelism().map_or(1, |n| n.get().min(8))
        } else {
            1
        };
        let nodes_per_thread = total.div_ceil(threads);
        std::thread::scope(|scope| {
            for (t, chunk) in table.chunks_mut(nodes_per_thread * n_out).enumerate() {
                let sample = &sample;
                scope.spawn(move || sample(t * nodes_per_thread, chunk));
            }
        });
        let mut clut = Clut::new(&[grid; 4], n_in, n_out, table)?;
        if intent != Intent::AbsoluteColorimetric {
            fix_white_misalignment(&mut clut, src.space, dst.space);
        }
        Some(transform(Eval::Clut(clut)))
    }

    pub(crate) fn is_identity(&self) -> bool {
        matches!(self.eval, Eval::Identity)
    }

    /// Convert one 16-bit colour (device encoding, Lab in v4 encoding) into
    /// the first `n_out` slots of `out`.
    pub(crate) fn eval16(&self, input: &[u16], out: &mut [u16]) {
        #[cfg(test)]
        EVALUATIONS.with(|count| count.set(count.get() + 1));
        match &self.eval {
            Eval::Identity => out[..self.n_out].copy_from_slice(&input[..self.n_out]),
            Eval::Clut(c) => c.lerp16(input, out),
            Eval::Curves(t) => {
                for (i, table) in t.iter().enumerate().take(self.n_out) {
                    out[i] = lin_lerp_1d(table, input[i]);
                }
            }
            Eval::MatShaper(m) => m.eval(input, out),
            Eval::Float(stages) => {
                let mut tmp = [0u16; MAX_CHANNELS];
                eval_pipeline_16(stages, input, &mut tmp);
                out[..self.n_out].copy_from_slice(&tmp[..self.n_out]);
            }
        }
    }

    /// `cmsDoTransform` on 8-bit samples (`fz_icc_transform_pixmap`):
    /// lcms2's `FROM_8_TO_16` in, `FROM_16_TO_8` rounding out.
    pub(crate) fn eval8(&self, input: &[u8], out: &mut [u8]) {
        let mut in16 = [0u16; MAX_CHANNELS];
        for (slot, &v) in in16.iter_mut().zip(input).take(self.n_in) {
            *slot = from_8_to_16(v);
        }
        let mut out16 = [0u16; MAX_CHANNELS];
        self.eval16(&in16[..self.n_in], &mut out16);
        for (slot, &v) in out.iter_mut().zip(&out16).take(self.n_out) {
            *slot = from_16_to_8(v);
        }
    }

    /// `fz_icc_transform_color`: PDF components (0..1, Lab in its natural
    /// range) to destination components in 0..1, quantised the way MuPDF's
    /// draw device paints a fill (truncation to 8 bits) so the rasteriser's
    /// rounding lands on the same byte. Returns the output count.
    pub(crate) fn convert_into(&self, components: &[f32], out: &mut [f32]) -> usize {
        let mut input = [0u16; MAX_CHANNELS];
        let clamp16 = |v: f32| (v as i32).clamp(0, 65535) as u16;
        if self.lab_input {
            input[0] = clamp16(components[0] * 655.35);
            input[1] = clamp16((components[1] + 128.0) * 257.0);
            input[2] = clamp16((components[2] + 128.0) * 257.0);
        } else {
            for (i, v) in components.iter().take(self.n_in).enumerate() {
                input[i] = clamp16(v * 65535.0);
            }
        }
        let mut out16 = [0u16; MAX_CHANNELS];
        self.eval16(&input[..self.n_in], &mut out16);
        for (slot, &v) in out.iter_mut().zip(&out16).take(self.n_out) {
            let unit = f32::from(v) / 65535.0;
            *slot = f32::from((unit * 255.0) as u8) / 255.0;
        }
        self.n_out
    }

    /// [`Transform::convert_into`] for a link into device RGB.
    pub(crate) fn convert(&self, components: &[f32]) -> [f32; 3] {
        let mut out = [0.0f32; MAX_CHANNELS];
        self.convert_into(components, &mut out);
        [out[0], out[1], out[2]]
    }
}

/// lcms2's `FROM_16_TO_8`.
#[inline]
fn from_16_to_8(v: u16) -> u8 {
    ((u32::from(v) * 65281 + 8_388_608) >> 24) as u8
}

/// `OptimizeByJoiningCurves`: a pipeline made only of curve stages becomes
/// one 4096-point table per channel, or the identity when they are linear.
fn join_curves(stages: &[Stage], n: usize) -> Eval {
    let mut tables = vec![vec![0u16; PRELINEARIZATION_POINTS]; n];
    let mut out = [0.0f32; MAX_CHANNELS];
    for i in 0..PRELINEARIZATION_POINTS {
        let v = (i as f64 / (PRELINEARIZATION_POINTS - 1) as f64) as f32;
        let input = vec![v; n];
        eval_pipeline_float(stages, &input, &mut out);
        for (c, table) in tables.iter_mut().enumerate() {
            table[i] = quick_saturate_word(f64::from(out[c]) * 65535.0);
        }
    }
    if tables.iter().all(|t| table_is_linear(t)) {
        Eval::Identity
    } else {
        Eval::Curves(tables)
    }
}

/// `OptimizeMatrixShaper`: an 8-bit RGB→RGB link whose pipeline is
/// shaper–matrix[–matrix]–shaper runs as lcms2's 1.14 fixed-point evaluator.
fn mat_shaper(stages: &[Stage]) -> Option<Eval> {
    let is_3x3 = |m: &MatrixStage| m.rows == 3 && m.cols == 3;
    let (c1, m, off, c2) = match stages {
        [
            Stage::Curves(c1),
            Stage::Matrix(m1),
            Stage::Matrix(m2),
            Stage::Curves(c2),
        ] => {
            if m1.offset.is_some() || !is_3x3(m1) || !is_3x3(m2) {
                return None;
            }
            (c1, mat3_mul(&m2.m, &m1.m), m2.offset.as_deref(), c2)
        }
        [Stage::Curves(c1), Stage::Matrix(m1), Stage::Curves(c2)] => {
            if !is_3x3(m1) {
                return None;
            }
            let mut m = [0.0; 9];
            m.copy_from_slice(&m1.m);
            (c1, m, m1.offset.as_deref(), c2)
        }
        _ => return None,
    };
    if c1.len() != 3 || c2.len() != 3 {
        return None;
    }
    if matrix_is_identity(&m) && off.is_none() {
        let joined = [Stage::Curves(c1.clone()), Stage::Curves(c2.clone())];
        return Some(join_curves(&joined, 3));
    }
    Some(Eval::MatShaper(Box::new(MatShaper::new(c1, &m, off, c2))))
}

const SHAPER2_POINTS: usize = 16385;

/// lcms2's `DOUBLE_TO_1FIXED14`.
fn double_to_1fixed14(x: f64) -> i32 {
    (x * 16384.0 + 0.5).floor() as i32
}

/// `MatShaper8Data` with `MatShaperEval16`: 8-bit input through the first
/// shaper into 1.14 fixed point, the matrix, then a second shaper already
/// quantised to the output byte.
#[derive(Debug)]
struct MatShaper {
    shaper1: [[i32; 256]; 3],
    mat: [[i32; 3]; 3],
    off: [i32; 3],
    shaper2: Vec<u16>,
}

impl MatShaper {
    fn new(curve1: &[Curve], m: &[f64; 9], offset: Option<&[f64]>, curve2: &[Curve]) -> MatShaper {
        let mut shaper1 = [[0i32; 256]; 3];
        for (table, curve) in shaper1.iter_mut().zip(curve1) {
            for (i, slot) in table.iter_mut().enumerate() {
                let y = curve.eval_f32((i as f64 / 255.0) as f32);
                *slot = if y < 131072.0 {
                    double_to_1fixed14(f64::from(y))
                } else {
                    0x7fff_ffff
                };
            }
        }
        let mut shaper2 = vec![0u16; 3 * SHAPER2_POINTS];
        for (table, curve) in shaper2
            .as_chunks_mut::<SHAPER2_POINTS>()
            .0
            .iter_mut()
            .zip(curve2)
        {
            for (i, slot) in table.iter_mut().enumerate() {
                let v = curve.eval_f32((i as f64 / 16384.0) as f32).clamp(0.0, 1.0);
                let w = quick_saturate_word(f64::from(v) * 65535.0);
                *slot = from_8_to_16(from_16_to_8(w));
            }
        }
        let mut mat = [[0i32; 3]; 3];
        for (i, row) in mat.iter_mut().enumerate() {
            for (j, cell) in row.iter_mut().enumerate() {
                *cell = double_to_1fixed14(m[i * 3 + j]);
            }
        }
        let mut off = [0i32; 3];
        if let Some(offset) = offset {
            for (slot, o) in off.iter_mut().zip(offset) {
                *slot = double_to_1fixed14(*o);
            }
        }
        MatShaper {
            shaper1,
            mat,
            off,
            shaper2,
        }
    }

    fn eval(&self, input: &[u16], out: &mut [u16]) {
        // The input came from a byte: its low byte is the sample.
        let r = self.shaper1[0][usize::from(input[0] & 0xff)];
        let g = self.shaper1[1][usize::from(input[1] & 0xff)];
        let b = self.shaper1[2][usize::from(input[2] & 0xff)];
        for (c, (row, slot)) in self.mat.iter().zip(out.iter_mut()).enumerate() {
            let l = row[0]
                .wrapping_mul(r)
                .wrapping_add(row[1].wrapping_mul(g))
                .wrapping_add(row[2].wrapping_mul(b))
                .wrapping_add(self.off[c])
                .wrapping_add(0x2000)
                >> 14;
            *slot = self.shaper2[c * SHAPER2_POINTS + l.clamp(0, 16384) as usize];
        }
    }
}

/// `FixWhiteMisalignment`: patch the grid node for white so white maps to
/// the destination's white, as lcms2 does after resampling.
fn fix_white_misalignment(clut: &mut Clut, src_space: u32, dst_space: u32) {
    let (Some(white_in), Some(white_out)) = (white_colorant(src_space), white_colorant(dst_space))
    else {
        return;
    };
    if white_in.len() != clut.n_in || white_out.len() != clut.n_out {
        return;
    }
    let mut got = [0u16; MAX_CHANNELS];
    clut.lerp16(&white_in, &mut got);
    let wildly_different = got
        .iter()
        .zip(&white_out)
        .any(|(&g, &w)| (i32::from(g) - i32::from(w)).abs() > 0xf000);
    if wildly_different || got[..clut.n_out] == white_out[..] {
        return;
    }
    let mut index = 0usize;
    for (i, &w) in white_in.iter().enumerate() {
        let p = f64::from(w) * f64::from(clut.domain[i]) / 65535.0;
        if p.fract() != 0.0 {
            return;
        }
        index += clut.opta[clut.n_in - 1 - i] as usize * p as usize;
    }
    clut.table[index..index + clut.n_out].copy_from_slice(&white_out);
}

// ---------------------------------------------------------------------------
// Device profiles, links and caching
// ---------------------------------------------------------------------------

fn bundled(bytes: &[u8]) -> Arc<Profile> {
    Profile::parse(bytes).expect("bundled MuPDF ICC profile parses")
}

impl Profile {
    /// Unique for the life of the process: two loads never share one.
    pub(crate) fn id(&self) -> u64 {
        self.id
    }
}

pub(crate) static DEVICE_GRAY: LazyLock<Arc<Profile>> = LazyLock::new(|| bundled(GRAY_ICC));
pub(crate) static DEVICE_RGB: LazyLock<Arc<Profile>> = LazyLock::new(|| bundled(RGB_ICC));
pub(crate) static DEVICE_CMYK: LazyLock<Arc<Profile>> = LazyLock::new(|| bundled(CMYK_ICC));
pub(crate) static DEVICE_LAB: LazyLock<Arc<Profile>> = LazyLock::new(|| bundled(LAB_ICC));

/// MuPDF's PostScript profiles (`ps_gray`, `ps_rgb`, `ps_cmyk`): what gray,
/// RGB and CMYK colours go through inside a luminosity soft mask.
pub(crate) static PS_GRAY: LazyLock<Arc<Profile>> =
    LazyLock::new(|| bundled(include_bytes!("icc/ps_gray.icc")));
pub(crate) static PS_RGB: LazyLock<Arc<Profile>> =
    LazyLock::new(|| bundled(include_bytes!("icc/ps_rgb.icc")));
pub(crate) static PS_CMYK: LazyLock<Arc<Profile>> =
    LazyLock::new(|| bundled(include_bytes!("icc/ps_cmyk.icc")));

#[cfg(test)]
thread_local! {
    /// Link evaluations on this thread, so tests bound conversion work by
    /// a count rather than a clock.
    static EVALUATIONS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
pub(crate) fn evaluations() -> usize {
    EVALUATIONS.with(std::cell::Cell::get)
}

type LinkKey = (u64, u64, Intent, bool, bool);

static LINKS: LazyLock<Mutex<HashMap<LinkKey, Arc<Transform>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// The cached link from `src` to `dst` (`fz_find_icc_link`), for 16-bit
/// colours or 8-bit pixmaps. Byte-identical profiles link as identity, as
/// MuPDF's md5 check does.
pub(crate) fn link(
    src: &Arc<Profile>,
    dst: &Arc<Profile>,
    intent: Intent,
    bpc: bool,
    bits8: bool,
) -> Option<Arc<Transform>> {
    let key = (src.id, dst.id, intent, bpc, bits8);
    if let Some(link) = LINKS.lock().ok()?.get(&key) {
        return Some(Arc::clone(link));
    }
    let link = if src.same_profile(dst) {
        Transform::identity(src)
    } else if Arc::ptr_eq(src, &DEVICE_CMYK)
        && Arc::ptr_eq(dst, &DEVICE_RGB)
        && intent == Intent::RelativeColorimetric
        && bpc
        && let Some(link) = Transform::from_sampled_table(CMYK_LINK_TABLE, 4, 17)
    {
        link
    } else {
        Transform::new(src, dst, intent, bpc, bits8)?
    };
    let link = Arc::new(link);
    LINKS.lock().ok()?.insert(key, Arc::clone(&link));
    Some(link)
}

/// [`link`] into MuPDF's device RGB profile.
pub(crate) fn link_to_rgb(
    profile: &Arc<Profile>,
    intent: Intent,
    bpc: bool,
    bits8: bool,
) -> Option<Arc<Transform>> {
    link(profile, &DEVICE_RGB, intent, bpc, bits8)
}

/// The DeviceCMYK → device RGB link, sampled once by `Transform::new` and
/// bundled so a process does not spend ~10 ms rebuilding its 83 521-node
/// grid. Regenerate after any change to the engine or the profiles with
/// `PDF_GOAT_CMYK_LINK_OUT=crates/interp/src/icc/cmyk_link.bin cargo test
/// -p pdf-interp -- --ignored write_cmyk_link_table`; the test
/// `bundled_cmyk_link_matches_engine` fails until it is regenerated.
static CMYK_LINK_TABLE: &[u8] = include_bytes!("icc/cmyk_link.bin");

impl Transform {
    /// A link from a bundled big-endian 16-bit grid.
    fn from_sampled_table(bytes: &[u8], n_in: usize, grid: usize) -> Option<Transform> {
        let table: Vec<u16> = bytes
            .as_chunks::<2>()
            .0
            .iter()
            .map(|b| u16::from_be_bytes(*b))
            .collect();
        let clut = Clut::new(&[grid; 4], n_in, 3, table)?;
        Some(Transform {
            n_in,
            n_out: 3,
            lab_input: false,
            eval: Eval::Clut(clut),
        })
    }

    /// Components the link writes.
    pub(crate) fn outputs(&self) -> usize {
        self.n_out
    }

    #[cfg(test)]
    fn sampled_table(&self) -> Option<&[u16]> {
        match &self.eval {
            Eval::Clut(c) => Some(&c.table),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// CalGray / CalRGB profiles (MuPDF source/fitz/color.c fz_new_icc_data_from_cal)
// ---------------------------------------------------------------------------

fn transform_vector(m: &[f64; 9], v: &[f64; 3]) -> [f64; 3] {
    [
        m[0] * v[0] + m[1] * v[1] + m[2] * v[2],
        m[3] * v[0] + m[4] * v[1] + m[5] * v[2],
        m[6] * v[0] + m[7] * v[1] + m[8] * v[2],
    ]
}

/// Bradford adaptation from one white to another (color.c `adaptation_matrix`).
fn adaptation_matrix(from: &[f64; 3], to: &[f64; 3]) -> Option<[f64; 9]> {
    const LAM_RIGG: [f64; 9] = [
        0.8951, 0.2664, -0.1614, -0.7502, 1.7135, 0.0367, 0.0389, -0.0685, 1.0296,
    ];
    let chad_inv = mat3_inverse(&LAM_RIGG)?;
    let f = transform_vector(&LAM_RIGG, from);
    let t = transform_vector(&LAM_RIGG, to);
    let cone = [
        t[0] / f[0],
        0.0,
        0.0,
        0.0,
        t[1] / f[1],
        0.0,
        0.0,
        0.0,
        t[2] / f[2],
    ];
    Some(mat3_mul(&chad_inv, &mat3_mul(&cone, &LAM_RIGG)))
}

/// color.c `build_rgb2XYZ_transfer_matrix`: colorants from xyY primaries and
/// white, adapted to D50.
fn rgb_to_xyz_matrix(white_xy: [f64; 2], primaries_xy: [[f64; 2]; 3]) -> Option<[f64; 9]> {
    let [[xr, yr], [xg, yg], [xb, yb]] = primaries_xy;
    let primaries = [
        xr,
        xg,
        xb,
        yr,
        yg,
        yb,
        1.0 - xr - yr,
        1.0 - xg - yg,
        1.0 - xb - yb,
    ];
    let inverse = mat3_inverse(&primaries)?;
    let [xn, yn] = white_xy;
    let whitepoint = [xn / yn, 1.0, (1.0 - xn - yn) / yn];
    let coef = transform_vector(&inverse, &whitepoint);
    let mat = [
        coef[0] * xr,
        coef[1] * xg,
        coef[2] * xb,
        coef[0] * yr,
        coef[1] * yg,
        coef[2] * yb,
        coef[0] * (1.0 - xr - yr),
        coef[1] * (1.0 - xg - yg),
        coef[2] * (1.0 - xb - yb),
    ];
    let bradford = adaptation_matrix(&whitepoint, &D50)?;
    Some(mat3_mul(&bradford, &mat))
}

/// color.c `double2XYZtype`: truncating float → s15Fixed16, negatives to 0.
fn xyz_fixed(v: f32) -> u32 {
    let v = v.max(0.0);
    let s = v as i16;
    let m = ((v - f32::from(s)) * 65536.0) as u16;
    ((i32::from(s) << 16) as u32) | u32::from(m)
}

/// The profile MuPDF synthesises for a CalRGB (`matrix` given) or CalGray
/// colour space: v2.2, XYZ PCS, gamma TRCs, colorants adapted to D50.
pub(crate) fn cal_profile(
    white: [f32; 3],
    gamma: &[f32],
    matrix: Option<[f32; 9]>,
) -> Option<Arc<Profile>> {
    let n = gamma.len();
    let mut colorants = Vec::new();
    if let Some(matrix) = matrix {
        let xyz = f64::from(white[0] + white[1] + white[2]);
        let white_xy = [f64::from(white[0]) / xyz, f64::from(white[1]) / xyz];
        let mut primaries_xy = [[0.0f64; 2]; 3];
        for (k, p) in primaries_xy.iter_mut().enumerate() {
            let sum = f64::from(matrix[3 * k] + matrix[3 * k + 1] + matrix[3 * k + 2]);
            *p = [
                f64::from(matrix[3 * k]) / sum,
                f64::from(matrix[3 * k + 1]) / sum,
            ];
        }
        let m = rgb_to_xyz_matrix(white_xy, primaries_xy)?;
        for k in 0..3 {
            colorants.push([m[k] as f32, m[k + 3] as f32, m[k + 6] as f32]);
        }
    }
    let tag_count = colorants.len() + 1 + n;
    let mut out =
        Vec::with_capacity(128 + 4 + 12 * tag_count + 20 * (colorants.len() + 1) + 16 * n);
    out.extend_from_slice(&[0u8; 128]);
    out[8..12].copy_from_slice(&0x0220_0000u32.to_be_bytes());
    out[12..16].copy_from_slice(&CLASS_INPUT.to_be_bytes());
    out[16..20].copy_from_slice(&(if n == 3 { SPACE_RGB } else { SPACE_GRAY }).to_be_bytes());
    out[20..24].copy_from_slice(&SPACE_XYZ.to_be_bytes());
    out[36..40].copy_from_slice(b"acsp");
    out[40..44].copy_from_slice(b"APPL");
    for (i, v) in [0.9642f32, 1.0, 0.8249].iter().enumerate() {
        out[68 + 4 * i..72 + 4 * i].copy_from_slice(&xyz_fixed(*v).to_be_bytes());
    }
    out.extend_from_slice(&(tag_count as u32).to_be_bytes());
    let mut offset = 128 + 4 + 12 * tag_count;
    let mut directory = Vec::new();
    let mut body = Vec::new();
    let mut xyz_tag = |sig: u32, v: [f32; 3], directory: &mut Vec<u8>, body: &mut Vec<u8>| {
        directory.extend_from_slice(&sig.to_be_bytes());
        directory.extend_from_slice(&(offset as u32).to_be_bytes());
        directory.extend_from_slice(&20u32.to_be_bytes());
        body.extend_from_slice(&sig_xyz_type());
        for c in v {
            body.extend_from_slice(&xyz_fixed(c).to_be_bytes());
        }
        offset += 20;
    };
    for (sig, c) in [SIG_RXYZ, SIG_GXYZ, SIG_BXYZ].iter().zip(&colorants) {
        xyz_tag(*sig, *c, &mut directory, &mut body);
    }
    xyz_tag(SIG_WTPT, [0.9642, 1.0, 0.8249], &mut directory, &mut body);
    let trc_sigs: [u32; 3] = if n == 3 {
        [SIG_RTRC, SIG_GTRC, SIG_BTRC]
    } else {
        [SIG_KTRC, 0, 0]
    };
    for (k, g) in gamma.iter().enumerate() {
        directory.extend_from_slice(&trc_sigs[k].to_be_bytes());
        directory.extend_from_slice(&(offset as u32).to_be_bytes());
        directory.extend_from_slice(&16u32.to_be_bytes());
        body.extend_from_slice(&TYPE_CURV.to_be_bytes());
        body.extend_from_slice(&[0u8; 4]);
        body.extend_from_slice(&1u32.to_be_bytes());
        body.extend_from_slice(&((g * 256.0) as u16).to_be_bytes());
        body.extend_from_slice(&[0u8; 2]);
        offset += 16;
    }
    out.extend_from_slice(&directory);
    out.extend_from_slice(&body);
    let size = out.len() as u32;
    out[0..4].copy_from_slice(&size.to_be_bytes());
    Profile::parse(&out)
}

fn sig_xyz_type() -> [u8; 8] {
    let mut h = [0u8; 8];
    h[..4].copy_from_slice(&SPACE_XYZ.to_be_bytes());
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bytes(rgb: [f32; 3]) -> [u8; 3] {
        rgb.map(|v| (v * 255.0 + 0.5) as u8)
    }

    fn device_link(profile: &Arc<Profile>) -> Arc<Transform> {
        link_to_rgb(profile, Intent::RelativeColorimetric, true, false).expect("device link builds")
    }

    fn engine_cmyk_link() -> Transform {
        Transform::new(
            &DEVICE_CMYK,
            &DEVICE_RGB,
            Intent::RelativeColorimetric,
            true,
            false,
        )
        .expect("CMYK link builds")
    }

    #[test]
    fn bundled_cmyk_link_matches_engine() {
        let fresh = engine_cmyk_link();
        let bundled =
            Transform::from_sampled_table(CMYK_LINK_TABLE, 4, 17).expect("bundled table loads");
        assert_eq!(bundled.sampled_table(), fresh.sampled_table());
    }

    #[test]
    #[ignore = "writes the bundled table; see CMYK_LINK_TABLE"]
    fn write_cmyk_link_table() {
        let Some(path) = std::env::var_os("PDF_GOAT_CMYK_LINK_OUT") else {
            return;
        };
        let link = engine_cmyk_link();
        let table = link.sampled_table().expect("sampled link");
        let bytes: Vec<u8> = table.iter().flat_map(|v| v.to_be_bytes()).collect();
        std::fs::write(path, bytes).expect("table written");
    }

    #[test]
    fn device_cmyk_fills_match_pymupdf() {
        // PyMuPDF 1.27 (MuPDF + lcms2) at 72 dpi, solid fills.
        let link = device_link(&DEVICE_CMYK);
        let cases: [([f32; 4], [u8; 3]); 10] = [
            ([0.0, 0.0, 0.0, 0.0], [255, 255, 255]),
            ([1.0, 0.0, 0.0, 0.0], [0, 173, 239]),
            ([0.0, 0.0, 0.0, 1.0], [34, 31, 31]),
            ([0.0, 1.0, 1.0, 0.0], [237, 28, 36]),
            ([0.5, 0.0, 0.0, 0.0], [109, 207, 246]),
            ([0.3, 0.6, 0.1, 0.2], [150, 101, 140]),
            ([7.0 / 255.0, 0.0, 0.0, 0.0], [246, 251, 254]),
            ([100.0 / 255.0, 0.0, 0.0, 0.0], [143, 216, 247]),
            ([0.0, 0.0, 0.0, 128.0 / 255.0], [146, 148, 151]),
            ([0.0, 0.0, 0.0, 250.0 / 255.0], [40, 37, 39]),
        ];
        for (cmyk, want) in cases {
            let got = bytes(link.convert(&cmyk));
            for c in 0..3 {
                assert!(
                    (i32::from(got[c]) - i32::from(want[c])).abs() <= 1,
                    "cmyk {cmyk:?}: got {got:?}, want {want:?}"
                );
            }
        }
    }

    #[test]
    fn device_gray_fills_match_pymupdf() {
        let link = device_link(&DEVICE_GRAY);
        let cases: [(f32, [u8; 3]); 5] = [
            (0.2, [51, 51, 50]),
            (100.0 / 255.0, [99, 100, 99]),
            (128.0 / 255.0, [127, 128, 127]),
            (200.0 / 255.0, [199, 200, 199]),
            (1.0, [255, 255, 255]),
        ];
        for (g, want) in cases {
            assert_eq!(bytes(link.convert(&[g])), want, "gray {g}");
        }
    }

    #[test]
    fn lab_fills_match_pymupdf() {
        let link = device_link(&DEVICE_LAB);
        let got = bytes(link.convert(&[50.0, 20.0, -30.0]));
        let want = [131u8, 107, 170];
        for c in 0..3 {
            assert!(
                (i32::from(got[c]) - i32::from(want[c])).abs() <= 1,
                "got {got:?}, want {want:?}"
            );
        }
        // Lab white is not a grid node of the resampled link, so like
        // lcms2 it lands a hair under pure white.
        let white = bytes(link.convert(&[100.0, 0.0, 0.0]));
        assert!(white.iter().all(|&v| v >= 253), "{white:?}");
    }

    #[test]
    fn device_rgb_links_as_identity() {
        let link = device_link(&DEVICE_RGB);
        assert!(link.is_identity());
        let again = Profile::parse(RGB_ICC).expect("parses");
        assert!(device_link(&again).is_identity());
    }

    #[test]
    fn cal_rgb_with_srgb_primaries_stays_close_to_device_rgb() {
        let profile = cal_profile(
            [0.9505, 1.0, 1.089],
            &[2.2, 2.2, 2.2],
            Some([
                0.4124, 0.2126, 0.0193, 0.3576, 0.7152, 0.1192, 0.1805, 0.0722, 0.9505,
            ]),
        )
        .expect("cal profile builds");
        let link = device_link(&profile);
        // Gamma 2.2 and the sRGB curve differ most in the darks: PyMuPDF
        // paints DeviceRGB (0.5, 0.2, 0.9) as (127, 51, 229).
        let got = bytes(link.convert(&[0.5, 0.2, 0.9]));
        for (g, w) in got.iter().zip([127u8, 51, 229]) {
            assert!((i32::from(*g) - i32::from(w)).abs() <= 6, "got {got:?}");
        }
        assert_eq!(bytes(link.convert(&[1.0, 1.0, 1.0])), [255, 255, 255]);
        assert_eq!(bytes(link.convert(&[0.0, 0.0, 0.0])), [0, 0, 0]);
    }

    #[test]
    fn broken_profiles_are_rejected() {
        assert!(Profile::parse(&RGB_ICC[..100]).is_none());
        let mut bad = RGB_ICC.to_vec();
        bad[36..40].copy_from_slice(b"nope");
        assert!(Profile::parse(&bad).is_none());
    }
}
