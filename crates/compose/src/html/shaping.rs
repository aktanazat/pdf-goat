//! OpenType shaping and cluster-to-Unicode mappings shared by measurement and painting.

use std::rc::Rc;

use super::fonts::Face;

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Unicode {
    Scalar(char),
    Cluster(Rc<str>),
    Empty,
}

impl Unicode {
    pub(super) fn from_text(text: &str) -> Self {
        let mut chars = text.chars();
        match (chars.next(), chars.next()) {
            (None, _) => Self::Empty,
            (Some(ch), None) => Self::Scalar(ch),
            _ => Self::Cluster(Rc::from(text)),
        }
    }

    pub fn text(&self) -> String {
        match self {
            Self::Scalar(ch) => ch.to_string(),
            Self::Cluster(text) => text.to_string(),
            Self::Empty => String::new(),
        }
    }
}

#[derive(Clone)]
pub struct Glyph {
    pub gid: u16,
    pub unicode: Unicode,
    pub advance: f32,
    pub x_offset: f32,
    pub y_offset: f32,
}

pub struct ShapedGlyph {
    /// UTF-8 byte offset of the logical cluster in the supplied text.
    pub cluster: usize,
    pub glyph: Glyph,
}

pub fn shape(
    face: &Face,
    text: &str,
    rtl: bool,
    size: f32,
    letter_spacing: f32,
    word_spacing: f32,
) -> Vec<ShapedGlyph> {
    let Some(font) = rustybuzz::Face::from_slice(face.font.data(), face.font.face_index()) else {
        return Vec::new();
    };
    let mut buffer = rustybuzz::UnicodeBuffer::new();
    buffer.push_str(text);
    buffer.set_direction(if rtl {
        rustybuzz::Direction::RightToLeft
    } else {
        rustybuzz::Direction::LeftToRight
    });
    buffer.guess_segment_properties();
    let shaped = rustybuzz::shape(&font, &[], buffer);
    let mut starts: Vec<usize> = shaped
        .glyph_infos()
        .iter()
        .map(|info| info.cluster as usize)
        .collect();
    starts.push(text.len());
    starts.sort_unstable();
    starts.dedup();
    let scale = size / face.units_per_em;
    let mut previous = None;
    let mut output = Vec::with_capacity(shaped.len());
    for (info, pos) in shaped.glyph_infos().iter().zip(shaped.glyph_positions()) {
        let cluster = info.cluster as usize;
        let first = previous != Some(cluster);
        previous = Some(cluster);
        let index = starts.partition_point(|&start| start <= cluster);
        let end = starts.get(index).copied().unwrap_or(text.len());
        let source = text.get(cluster..end).unwrap_or("");
        let unicode = if first {
            Unicode::from_text(source)
        } else {
            Unicode::Empty
        };
        let spacing = if first {
            letter_spacing
                + if source == " " || source == "\u{a0}" {
                    word_spacing
                } else {
                    0.0
                }
        } else {
            0.0
        };
        output.push(ShapedGlyph {
            cluster,
            glyph: Glyph {
                gid: info.glyph_id as u16,
                unicode,
                advance: pos.x_advance as f32 * scale + spacing,
                x_offset: pos.x_offset as f32 * scale,
                y_offset: pos.y_offset as f32 * scale,
            },
        });
    }
    output
}
