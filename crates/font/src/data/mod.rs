//! Static tables converted at development time from pdfminer.six, fontTools, ghostscript
//! and the Adobe Core14 AFM files. Nothing here reads those sources at runtime.

pub(crate) mod agl;
pub(crate) mod cff_tables;
pub(crate) mod cmap_index;
pub(crate) mod encodings;
pub(crate) mod mac_glyphs;
pub(crate) mod std14;
