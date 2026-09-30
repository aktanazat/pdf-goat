//! Fixed-size word operations used by the grayscale bitplane decoder.

use core::ops::{BitAnd, BitOr, BitXor, BitXorAssign};

pub(crate) const SIMD_WIDTH: usize = 8;

#[derive(Copy, Clone)]
pub(crate) struct Mask32x8([bool; SIMD_WIDTH]);

impl Mask32x8 {
    #[inline(always)]
    pub(crate) fn select(self, if_true: U32x8, if_false: U32x8) -> U32x8 {
        let mut result = [0; SIMD_WIDTH];
        for (i, value) in result.iter_mut().enumerate() {
            *value = if self.0[i] {
                if_true.0[i]
            } else {
                if_false.0[i]
            };
        }
        U32x8(result)
    }
}

#[derive(Copy, Clone)]
#[repr(C, align(32))]
pub(crate) struct U32x8([u32; SIMD_WIDTH]);

impl U32x8 {
    #[inline(always)]
    pub(crate) fn from_slice(slice: &[u32]) -> Self {
        let mut values = [0; SIMD_WIDTH];
        values.copy_from_slice(&slice[..SIMD_WIDTH]);
        Self(values)
    }

    #[inline(always)]
    pub(crate) fn splat(value: u32) -> Self {
        Self([value; SIMD_WIDTH])
    }

    #[inline(always)]
    pub(crate) fn store(self, slice: &mut [u32]) {
        slice[..SIMD_WIDTH].copy_from_slice(&self.0);
    }

    #[inline(always)]
    pub(crate) fn simd_gt(self, other: Self) -> Mask32x8 {
        let mut result = [false; SIMD_WIDTH];
        for (i, value) in result.iter_mut().enumerate() {
            *value = self.0[i] > other.0[i];
        }
        Mask32x8(result)
    }
}

impl BitAnd for U32x8 {
    type Output = Self;

    #[inline(always)]
    fn bitand(self, rhs: Self) -> Self {
        let mut result = [0; SIMD_WIDTH];
        for (i, value) in result.iter_mut().enumerate() {
            *value = self.0[i] & rhs.0[i];
        }
        Self(result)
    }
}

impl BitOr for U32x8 {
    type Output = Self;

    #[inline(always)]
    fn bitor(self, rhs: Self) -> Self {
        let mut result = [0; SIMD_WIDTH];
        for (i, value) in result.iter_mut().enumerate() {
            *value = self.0[i] | rhs.0[i];
        }
        Self(result)
    }
}

impl BitXor for U32x8 {
    type Output = Self;

    #[inline(always)]
    fn bitxor(self, rhs: Self) -> Self {
        let mut result = [0; SIMD_WIDTH];
        for (i, value) in result.iter_mut().enumerate() {
            *value = self.0[i] ^ rhs.0[i];
        }
        Self(result)
    }
}

impl BitXorAssign for U32x8 {
    #[inline(always)]
    fn bitxor_assign(&mut self, rhs: Self) {
        for (value, rhs) in self.0.iter_mut().zip(rhs.0) {
            *value ^= rhs;
        }
    }
}
