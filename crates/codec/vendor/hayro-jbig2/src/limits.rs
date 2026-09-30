//! Per-decode bounds for intermediate images and entropy-driven loops.

use core::cell::Cell;

use crate::error::{OverflowError, Result};

#[derive(Clone, Copy)]
struct Budget {
    bits: u64,
    steps: u64,
}

thread_local! {
    static CURRENT: Cell<Budget> = const {
        Cell::new(Budget { bits: u64::MAX, steps: u64::MAX })
    };
}

/// Restores the caller's budget on success, error or unwinding.
/// Decoding is synchronous; nesting on the same thread is supported.
pub(crate) struct Guard(Budget);

impl Guard {
    pub(crate) fn enter(width: u32, height: u32, max_pixels: u64) -> Result<Self> {
        let page_pixels = u64::from(width) * u64::from(height);
        if page_pixels > max_pixels {
            return Err(OverflowError::PixelBudget.into());
        }
        let bits = page_pixels
            .saturating_mul(8)
            .saturating_add(1 << 24)
            .min(max_pixels);
        let budget = Budget {
            bits,
            steps: bits.min(1 << 16),
        };
        Ok(Self(CURRENT.with(|cell| cell.replace(budget))))
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        CURRENT.with(|cell| cell.set(self.0));
    }
}

/// Preflights entropy-coded dimensions against the remaining storage budget.
/// Regions can legally extend beyond the page and are clipped when combined.
pub(crate) fn check_dimensions(width: u32, height: u32) -> Result<()> {
    CURRENT.with(|cell| {
        if u64::from(width) * u64::from(height) > cell.get().bits {
            return Err(OverflowError::PixelBudget.into());
        }
        Ok(())
    })
}

/// Charges storage in bits, including padding and auxiliary buffers, before
/// allocating. Charges are cumulative; freeing or reusing storage cannot
/// conceal repeated decoding work.
pub(crate) fn charge_bits(bits: u64) -> Result<()> {
    CURRENT.with(|cell| {
        let mut budget = cell.get();
        budget.bits = budget
            .bits
            .checked_sub(bits)
            .ok_or(OverflowError::PixelBudget)?;
        cell.set(budget);
        Ok(())
    })
}

/// Arithmetic coding permits implicit trailing bits. A corrupt stream can
/// therefore return endless zero-length runs without allocating any pixels.
pub(crate) fn step() -> Result<()> {
    CURRENT.with(|cell| {
        let mut budget = cell.get();
        budget.steps = budget
            .steps
            .checked_sub(1)
            .ok_or(OverflowError::DecodeBudget)?;
        cell.set(budget);
        Ok(())
    })
}
