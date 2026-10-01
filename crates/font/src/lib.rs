//! Font programs, encodings, CMaps, standard-14 metrics, and TrueType, CFF and Type 1
//! subsetting.
//!
//! Outlines are in font units; [`Font::font_matrix`] maps them to text space.
//! [`embed`] writes an encoded subset and its Unicode map into a PDF document.

mod cff;
mod charstring;
mod cmap;
mod data;
pub mod embed;
mod encoding;
mod error;
mod font;
mod glyf;
mod glyphnames;
mod layout;
mod outline;
mod predefined;
mod ps;
mod reader;
mod sfnt;
mod std14;
mod subset;
mod subset_cff;
mod subset_type1;
mod subst;
mod type1;

pub use cmap::{
    CMap, CharCode, CidCollection, CidSystemInfo, CodespaceRange, ToUnicodeMap,
    write_simple_to_unicode_cmap, write_to_unicode_cmap,
};
pub use embed::{EmbedError, EmbedGlyph, EmbeddedFont, GlyphMapping, embed_font, embed_truetype};
pub use encoding::{BaseEncoding, Encoding, cp1252_from_unicode};
pub use error::{FontError, Result};
pub use font::{CidToGid, Font, FontKind, FontMetrics};
pub use glyphnames::{agl_lookup, dingbats_name_to_unicode, glyph_name_to_unicode};
pub use layout::{PositionedGlyph, layout_text, text_advance};
pub use outline::{Outline, PathOp, Rect};
pub use std14::{Standard14, Std14Metrics, strip_subset_tag};
pub use subset::{TrueTypeSubset, subset_truetype};
pub use subst::{FontLocator, FontRequest, MatchQuality, Script, Substitute};
