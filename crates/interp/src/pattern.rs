//! Pattern and soft-mask paints handed to devices.

use std::fmt;
use std::sync::Arc;

use pdf_core::{Matrix, ObjRef, Rect};

use crate::InterpError;
use crate::device::{Device, Rgb};
use crate::shading::Shading;

/// Content the interpreter can replay into another device: a tiling
/// pattern's cell or a soft mask's group.
pub(crate) trait Replay {
    fn replay(&self, device: &mut dyn Device) -> Result<(), InterpError>;
}

/// A shading pattern (`/PatternType 2`).
#[derive(Clone, Debug)]
pub struct ShadingPaint {
    pub shading: Arc<Shading>,
    /// Shading space → device.
    pub matrix: Matrix,
}

/// A tiling pattern (`/PatternType 1`).
pub struct TilingPaint<'a> {
    pub pattern: ObjRef,
    /// Pattern space → device.
    pub matrix: Matrix,
    /// /BBox in pattern space.
    pub bbox: Rect,
    pub x_step: f64,
    pub y_step: f64,
    /// /PaintType 1.
    pub colored: bool,
    /// For /PaintType 2, the colour the cell paints with.
    pub color: Option<Rgb>,
    /// Equal keys render the same cell.
    pub key: u64,
    pub(crate) cell: &'a dyn Replay,
}

impl TilingPaint<'_> {
    /// Runs one cell in device space (pattern space through `matrix`),
    /// clipped to `bbox`.
    pub fn run_cell(&self, device: &mut dyn Device) -> Result<(), InterpError> {
        self.cell.replay(device)
    }
}

impl fmt::Debug for TilingPaint<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TilingPaint")
            .field("pattern", &self.pattern)
            .field("matrix", &self.matrix)
            .field("bbox", &self.bbox)
            .field("x_step", &self.x_step)
            .field("y_step", &self.y_step)
            .field("colored", &self.colored)
            .field("color", &self.color)
            .field("key", &self.key)
            .finish_non_exhaustive()
    }
}

/// An ExtGState soft mask (`/SMask` dictionary).
pub struct SoftMask<'a> {
    /// `/S /Luminosity` (else `/Alpha`).
    pub luminosity: bool,
    /// /BC as RGB.
    pub backdrop: Rgb,
    /// /TR sampled at 256 points; `None` is the identity.
    pub transfer: Option<Arc<[u8; 256]>>,
    /// Device-space bounds of the mask group.
    pub bbox: Rect,
    /// Equal keys are the same mask.
    pub key: u64,
    pub(crate) group: &'a dyn Replay,
}

impl SoftMask<'_> {
    /// Replays the /G group in device space.
    pub fn run(&self, device: &mut dyn Device) -> Result<(), InterpError> {
        self.group.replay(device)
    }
}

impl fmt::Debug for SoftMask<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SoftMask")
            .field("luminosity", &self.luminosity)
            .field("backdrop", &self.backdrop)
            .field("transfer", &self.transfer.is_some())
            .field("bbox", &self.bbox)
            .field("key", &self.key)
            .finish_non_exhaustive()
    }
}
