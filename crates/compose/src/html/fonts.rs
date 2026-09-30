//! Font selection: CSS family names mapped to macOS system fonts, faces matched by weight
//! and style with the CSS Fonts matching rules, and per-character fallback.
//!
//! Only `glyf` TrueType faces are used, since those are the faces the PDF writer can
//! subset and embed.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock};

use pdf_font::{Font, FontKind};

pub(super) fn system_fonts() -> Arc<fontdb::Database> {
    static FONTS: LazyLock<Arc<fontdb::Database>> = LazyLock::new(|| {
        let mut fonts = fontdb::Database::new();
        fonts.load_system_fonts();
        Arc::new(fonts)
    });
    Arc::clone(&FONTS)
}

pub type FaceId = usize;
pub type FamilyId = u32;

pub struct Face {
    pub font: Font,
    /// Ascent and descent in em; descent is positive below the baseline.
    pub ascent: f32,
    pub descent: f32,
    pub units_per_em: f32,
}

impl Face {
    /// Advance of `gid` in em.
    pub fn advance_em(&self, gid: u16) -> f32 {
        self.font.advance(gid).unwrap_or(0.0) / self.units_per_em
    }
}

const SUPPLEMENTAL: &str = "/System/Library/Fonts/Supplemental/";

/// Font files of a lowercased CSS family name, relative to the supplemental directory
/// unless absolute.
fn family_files(family: &str) -> &'static [&'static str] {
    match family {
        "helvetica neue" | "helveticaneue" | "-apple-system" | "system-ui"
        | "blinkmacsystemfont" | "ui-sans-serif" => &["/System/Library/Fonts/HelveticaNeue.ttc"],
        "helvetica" => &["/System/Library/Fonts/Helvetica.ttc"],
        "arial" => &[
            "Arial.ttf",
            "Arial Bold.ttf",
            "Arial Italic.ttf",
            "Arial Bold Italic.ttf",
        ],
        "verdana" | "sans-serif" => &[
            "Verdana.ttf",
            "Verdana Bold.ttf",
            "Verdana Italic.ttf",
            "Verdana Bold Italic.ttf",
        ],
        "times new roman" | "times" | "serif" => &[
            "Times New Roman.ttf",
            "Times New Roman Bold.ttf",
            "Times New Roman Italic.ttf",
            "Times New Roman Bold Italic.ttf",
        ],
        "georgia" => &[
            "Georgia.ttf",
            "Georgia Bold.ttf",
            "Georgia Italic.ttf",
            "Georgia Bold Italic.ttf",
        ],
        "courier new" | "courier" => &[
            "Courier New.ttf",
            "Courier New Bold.ttf",
            "Courier New Italic.ttf",
            "Courier New Bold Italic.ttf",
        ],
        "andale mono" | "monospace" => &["Andale Mono.ttf"],
        "menlo" | "sf mono" | "ui-monospace" => &["/System/Library/Fonts/Menlo.ttc"],
        "monaco" => &["/System/Library/Fonts/Monaco.ttf"],
        "tahoma" => &["Tahoma.ttf", "Tahoma Bold.ttf"],
        "trebuchet ms" => &[
            "Trebuchet MS.ttf",
            "Trebuchet MS Bold.ttf",
            "Trebuchet MS Italic.ttf",
            "Trebuchet MS Bold Italic.ttf",
        ],
        "comic sans ms" | "cursive" => &["Comic Sans MS.ttf", "Comic Sans MS Bold.ttf"],
        "impact" | "fantasy" => &["Impact.ttf"],
        "arial unicode ms" => &["Arial Unicode.ttf"],
        _ => &[],
    }
}

fn full_path(file: &str) -> String {
    if file.starts_with('/') {
        file.to_owned()
    } else {
        format!("{SUPPLEMENTAL}{file}")
    }
}

/// Faces of the system collections as (face index, weight, italic), so choosing a face
/// does not parse, and copy, every face of the file.
fn collection_faces(path: &str) -> Option<&'static [(u32, u16, bool)]> {
    match path {
        "/System/Library/Fonts/HelveticaNeue.ttc" => Some(&[
            (0, 400, false),
            (1, 700, false),
            (2, 400, true),
            (3, 700, true),
            (5, 100, false),
            (6, 100, true),
            (7, 300, false),
            (8, 300, true),
            (10, 500, false),
            (11, 500, true),
            (12, 200, false),
            (13, 200, true),
        ]),
        "/System/Library/Fonts/Menlo.ttc" => Some(&[
            (0, 400, false),
            (1, 700, false),
            (2, 400, true),
            (3, 700, true),
        ]),
        "/System/Library/Fonts/Helvetica.ttc" => Some(&[
            (0, 400, false),
            (1, 700, false),
            (2, 400, true),
            (3, 700, true),
            (4, 300, false),
            (5, 300, true),
        ]),
        _ => None,
    }
}

/// The last-resort chains for characters the requested families lack: (file, face).
const HAN_SERIF: (&str, u32) = ("Songti.ttc", 6);
const HAN_SANS: (&str, u32) = ("/System/Library/Fonts/STHeiti Light.ttc", 1);
const HANGUL_SERIF: (&str, u32) = ("AppleMyungjo.ttf", 0);
const HANGUL_SANS: (&str, u32) = ("AppleGothic.ttf", 0);
const BROAD: (&str, u32) = ("Arial Unicode.ttf", 0);

/// Metadata of one usable face of a file.
struct FaceInfo {
    index: u32,
    weight: u16,
    italic: bool,
}

fn is_condensed(name: &str) -> bool {
    ["Condensed", "Compressed", "Narrow", "Extended"]
        .iter()
        .any(|word| name.contains(word))
}

/// Reads the usable faces of a font file: TrueType outlines, normal width.
fn scan_faces(path: &str) -> Vec<FaceInfo> {
    let Ok(data) = std::fs::read(path) else {
        return Vec::new();
    };
    let count = Font::face_count(&data).unwrap_or(0).min(64);
    let fonts: Vec<(u32, Font)> = if count == 1 {
        Font::parse(data)
            .map(|font| vec![(0, font)])
            .unwrap_or_default()
    } else {
        (0..count)
            .filter_map(|index| {
                Font::parse_face(data.clone(), index)
                    .ok()
                    .map(|font| (index, font))
            })
            .collect()
    };
    fonts
        .into_iter()
        .filter(|(_, font)| {
            font.kind() == FontKind::TrueType
                && !is_condensed(&font.postscript_name().unwrap_or_default())
        })
        .map(|(index, font)| {
            let metrics = font.metrics();
            FaceInfo {
                index,
                weight: metrics.weight,
                italic: metrics.is_italic,
            }
        })
        .collect()
}

/// CSS Fonts 4 weight matching, as a sort key: lower ranks are preferred.
fn weight_rank(desired: u16, candidate: u16) -> (u8, u16) {
    let distance = desired.abs_diff(candidate);
    let group = if (400..=500).contains(&desired) {
        if candidate >= desired && candidate <= 500 {
            0
        } else if candidate < desired {
            1
        } else {
            2
        }
    } else if desired < 400 {
        u8::from(candidate > desired)
    } else {
        u8::from(candidate < desired)
    };
    (group, distance)
}

pub struct Fonts {
    pub faces: Vec<Face>,
    families: Vec<Vec<String>>,
    family_ids: HashMap<Vec<String>, FamilyId>,
    /// Usable faces of each file; empty when the file is missing or not TrueType.
    file_faces: HashMap<String, Vec<FaceInfo>>,
    loaded: HashMap<(String, u32), Option<FaceId>>,
    database_loaded: HashMap<fontdb::ID, Option<FaceId>>,
    /// (family, weight, italic) to the matched face of each listed family, in order.
    matched: HashMap<(FamilyId, u16, bool), Vec<FaceId>>,
    by_char: HashMap<(FamilyId, u16, bool, char), FaceId>,
}

impl Fonts {
    pub fn new() -> Fonts {
        Fonts {
            faces: Vec::new(),
            families: Vec::new(),
            family_ids: HashMap::new(),
            file_faces: HashMap::new(),
            loaded: HashMap::new(),
            database_loaded: HashMap::new(),
            matched: HashMap::new(),
            by_char: HashMap::new(),
        }
    }

    /// Interns a lowercased family list.
    pub fn intern(&mut self, families: Vec<String>) -> FamilyId {
        if let Some(&id) = self.family_ids.get(&families) {
            return id;
        }
        let id = FamilyId::try_from(self.families.len()).unwrap_or(FamilyId::MAX);
        self.families.push(families.clone());
        self.family_ids.insert(families, id);
        id
    }

    fn file_info(&mut self, path: &str) -> &[FaceInfo] {
        if !self.file_faces.contains_key(path) {
            let infos = match collection_faces(path) {
                Some(known) if std::path::Path::new(path).is_file() => known
                    .iter()
                    .map(|&(index, weight, italic)| FaceInfo {
                        index,
                        weight,
                        italic,
                    })
                    .collect(),
                Some(_) => Vec::new(),
                None => scan_faces(path),
            };
            self.file_faces.insert(path.to_owned(), infos);
        }
        self.file_faces.get(path).map_or(&[], Vec::as_slice)
    }

    fn load(&mut self, path: &str, index: u32) -> Option<FaceId> {
        let key = (path.to_owned(), index);
        if let Some(&id) = self.loaded.get(&key) {
            return id;
        }
        let font = std::fs::read(path)
            .ok()
            .and_then(|data| Font::parse_face(data, index).ok());
        let id = font.and_then(|font| self.append_font(font));
        self.loaded.insert(key, id);
        id
    }

    fn append_font(&mut self, font: Font) -> Option<FaceId> {
        if font.kind() != FontKind::TrueType {
            return None;
        }
        let metrics = font.metrics();
        let units_per_em = f32::from(font.units_per_em().max(1));
        self.faces.push(Face {
            ascent: metrics.ascent / units_per_em,
            descent: -metrics.descent / units_per_em,
            units_per_em,
            font,
        });
        Some(self.faces.len() - 1)
    }

    pub(super) fn load_database(
        &mut self,
        database: &fontdb::Database,
        id: fontdb::ID,
    ) -> Option<FaceId> {
        if let Some(&face) = self.database_loaded.get(&id) {
            return face;
        }
        let font = database
            .with_face_data(id, |bytes, index| {
                Font::parse_face(bytes.to_vec(), index).ok()
            })
            .flatten();
        let face = font.and_then(|font| self.append_font(font));
        self.database_loaded.insert(id, face);
        face
    }

    /// The best face of one family for a weight and style.
    fn match_family(&mut self, family: &str, weight: u16, italic: bool) -> Option<FaceId> {
        if family_files(family).is_empty() {
            let database = system_fonts();
            let name = database
                .faces()
                .flat_map(|face| &face.families)
                .map(|(name, _)| name.as_str())
                .find(|name| name.eq_ignore_ascii_case(family))?;
            let id = database.query(&fontdb::Query {
                families: &[fontdb::Family::Name(name)],
                weight: fontdb::Weight(weight),
                style: if italic {
                    fontdb::Style::Italic
                } else {
                    fontdb::Style::Normal
                },
                ..fontdb::Query::default()
            })?;
            return self.load_database(&database, id);
        }
        let mut candidates: Vec<(String, u32, u16, bool)> = Vec::new();
        for file in family_files(family) {
            let path = full_path(file);
            let infos: Vec<(u32, u16, bool)> = self
                .file_info(&path)
                .iter()
                .map(|info| (info.index, info.weight, info.italic))
                .collect();
            candidates.extend(
                infos
                    .into_iter()
                    .map(|(index, w, it)| (path.clone(), index, w, it)),
            );
        }
        let style_available = candidates.iter().any(|candidate| candidate.3 == italic);
        let best = candidates
            .into_iter()
            .filter(|candidate| !style_available || candidate.3 == italic)
            .min_by_key(|candidate| (weight_rank(weight, candidate.2), candidate.1))?;
        self.load(&best.0, best.1)
    }

    fn family_faces(&mut self, family: FamilyId, weight: u16, italic: bool) -> Vec<FaceId> {
        if let Some(faces) = self.matched.get(&(family, weight, italic)) {
            return faces.clone();
        }
        let names = self
            .families
            .get(family as usize)
            .cloned()
            .unwrap_or_default();
        let mut faces = Vec::new();
        for name in names.iter().map(String::as_str).chain(["serif"]) {
            if let Some(face) = self.match_family(name, weight, italic)
                && !faces.contains(&face)
            {
                faces.push(face);
            }
        }
        self.matched.insert((family, weight, italic), faces.clone());
        faces
    }

    /// The face for text of this family where no character decides: the first listed
    /// family that resolves.
    pub fn primary(&mut self, family: FamilyId, weight: u16, italic: bool) -> Option<FaceId> {
        self.family_faces(family, weight, italic).first().copied()
    }

    fn sans(&self, family: FamilyId) -> bool {
        self.families.get(family as usize).is_some_and(|names| {
            names.iter().any(|name| {
                matches!(
                    name.as_str(),
                    "sans-serif"
                        | "helvetica neue"
                        | "helvetica"
                        | "arial"
                        | "verdana"
                        | "system-ui"
                        | "-apple-system"
                )
            })
        })
    }

    /// The face that draws `ch`: the first listed family whose face has the glyph, then
    /// the script fallbacks, then the primary face.
    pub fn face_for(
        &mut self,
        family: FamilyId,
        weight: u16,
        italic: bool,
        ch: char,
    ) -> Option<FaceId> {
        let key = (family, weight, italic, ch);
        if let Some(&face) = self.by_char.get(&key) {
            return Some(face);
        }
        let faces = self.family_faces(family, weight, italic);
        let has = |fonts: &Fonts, face: FaceId| {
            fonts.faces[face]
                .font
                .glyph_for_char(ch)
                .is_some_and(|gid| gid != 0)
        };
        let mut found = faces.iter().copied().find(|&face| has(self, face));
        if found.is_none() && !ch.is_whitespace() && !ch.is_control() {
            let sans = self.sans(family);
            let chain: [(&str, u32); 3] = match (is_hangul(ch), sans) {
                (true, true) => [HANGUL_SANS, HANGUL_SERIF, BROAD],
                (true, false) => [HANGUL_SERIF, HANGUL_SANS, BROAD],
                (false, true) => [HAN_SANS, HAN_SERIF, BROAD],
                (false, false) => [HAN_SERIF, HAN_SANS, BROAD],
            };
            for (file, index) in chain {
                if let Some(face) = self.load(&full_path(file), index)
                    && has(self, face)
                {
                    found = Some(face);
                    break;
                }
            }
        }
        if found.is_none() && !ch.is_whitespace() && !ch.is_control() {
            let database = system_fonts();
            let candidate = database
                .faces()
                .find(|face| {
                    database
                        .with_face_data(face.id, |data, index| {
                            rustybuzz::ttf_parser::Face::parse(data, index)
                                .ok()
                                .is_some_and(|face| {
                                    face.tables().glyf.is_some() && face.glyph_index(ch).is_some()
                                })
                        })
                        .unwrap_or(false)
                })
                .map(|face| face.id);
            if let Some(id) = candidate {
                found = self.load_database(&database, id);
            }
        }
        let face = found.or_else(|| faces.first().copied())?;
        self.by_char.insert(key, face);
        Some(face)
    }
}

fn is_hangul(ch: char) -> bool {
    matches!(u32::from(ch), 0x1100..=0x11FF | 0x3130..=0x318F | 0xA960..=0xA97F | 0xAC00..=0xD7FF)
}
