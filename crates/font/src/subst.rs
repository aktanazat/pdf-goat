//! System font substitution for fonts a PDF names but does not embed.
//!
//! Font directories are walked directly (no fontconfig): `/System/Library/Fonts` and
//! `/Library/Fonts` on macOS, `/usr/share/fonts` and `/usr/local/share/fonts` on Linux.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use crate::cmap::CidCollection;
use crate::error::{FontError, Result};
use crate::font::Font;
use crate::reader::{read_u16, read_u32};
use crate::sfnt::{self, TAG_TTCF};
use crate::std14::strip_subset_tag;
use crate::type1::Type1Header;

const SYSTEM_DIRS: [&str; 4] = [
    "/System/Library/Fonts",
    "/Library/Fonts",
    "/usr/share/fonts",
    "/usr/local/share/fonts",
];
const MAX_DEPTH: usize = 8;
const MAX_ENTRIES: usize = 100_000;
const MAX_FACES_PER_FILE: u32 = 256;
/// Largest `name` table read while indexing.
const MAX_NAME_TABLE: usize = 1 << 20;
/// Bytes of a Type 1 file read for its cleartext header.
const TYPE1_PREFIX: usize = 64 * 1024;

const FLAG_FIXED_PITCH: u32 = 1;
const FLAG_SERIF: u32 = 1 << 1;
const FLAG_ITALIC: u32 = 1 << 6;
const FLAG_FORCE_BOLD: u32 = 1 << 18;

/// Writing systems that need a CJK-capable substitute; everything else is `Latin`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Script {
    Latin,
    Japanese,
    SimplifiedChinese,
    TraditionalChinese,
    Korean,
}

impl Script {
    pub fn from_collection(collection: CidCollection) -> Script {
        match collection {
            CidCollection::Japan1 => Script::Japanese,
            CidCollection::Gb1 => Script::SimplifiedChinese,
            CidCollection::Cns1 => Script::TraditionalChinese,
            CidCollection::Korea1 => Script::Korean,
        }
    }

    /// Script for a `/CIDSystemInfo` ordering; `Latin` for `Identity` and unknown orderings.
    pub fn from_ordering(ordering: &str) -> Script {
        CidCollection::from_ordering(ordering).map_or(Script::Latin, Script::from_collection)
    }
}

/// What a PDF says about a font it does not embed.
#[derive(Debug, Clone, Copy)]
pub struct FontRequest<'a> {
    /// `/BaseFont`, with or without a subset tag.
    pub base_font: &'a str,
    /// `/Flags` of the font descriptor, 0 when absent.
    pub flags: u32,
    /// `/FontWeight` of the font descriptor.
    pub weight: Option<u16>,
    pub script: Script,
}

/// How closely a substitute matches the request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatchQuality {
    /// A face whose PostScript name equals the base font name.
    Exact,
    /// A face of the requested family, chosen by weight and slant.
    Family,
    /// A stand-in chosen by class (sans, serif, monospace, symbol, dingbats) and script.
    Fallback,
}

/// An installed font face chosen for a request.
#[derive(Debug, Clone, PartialEq)]
pub struct Substitute {
    pub path: PathBuf,
    pub face_index: u32,
    pub postscript_name: String,
    pub quality: MatchQuality,
}

impl Substitute {
    /// Reads the file and parses the chosen face.
    pub fn load(&self) -> Result<Font> {
        let data = std::fs::read(&self.path)
            .map_err(|e| FontError::Io(format!("{}: {e}", self.path.display())))?;
        Font::parse_face(data, self.face_index)
    }
}

#[derive(Debug, Clone)]
struct FaceEntry {
    file: usize,
    index: u32,
    postscript_name: String,
    ps_key: String,
    family_key: String,
    weight: u16,
    italic: bool,
    /// OS/2 width class; 5 is normal.
    width: u16,
    /// Faces whose family starts with '.' are private to the system.
    hidden: bool,
}

/// An index of installed font files and their faces.
#[derive(Debug, Clone, Default)]
pub struct FontLocator {
    files: Vec<PathBuf>,
    /// Lowercased file names, parallel to `files`.
    file_names: Vec<String>,
    faces: Vec<FaceEntry>,
}

static SYSTEM: LazyLock<FontLocator> = LazyLock::new(|| {
    let dirs: Vec<&Path> = SYSTEM_DIRS.iter().map(Path::new).collect();
    FontLocator::scan(&dirs)
});

fn key(s: &str) -> String {
    s.chars()
        .filter(char::is_ascii_alphanumeric)
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

fn is_font_file(path: &Path) -> bool {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase);
    matches!(
        ext.as_deref(),
        Some("ttf" | "otf" | "ttc" | "otc" | "pfb" | "pfa" | "t1")
    )
}

impl FontLocator {
    /// The index of the system font directories, built on first use.
    pub fn system() -> &'static FontLocator {
        &SYSTEM
    }

    /// Indexes the font files under `dirs` (recursively, symlinks followed, depth-limited).
    /// Missing directories and unreadable files are skipped.
    pub fn scan(dirs: &[&Path]) -> FontLocator {
        let mut files = Vec::new();
        let mut stack: Vec<(PathBuf, usize)> = dirs.iter().map(|d| (d.to_path_buf(), 0)).collect();
        let mut entries = 0usize;
        while let Some((dir, depth)) = stack.pop() {
            let Ok(read) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in read.flatten() {
                entries += 1;
                if entries > MAX_ENTRIES {
                    break;
                }
                let path = entry.path();
                let Ok(meta) = std::fs::metadata(&path) else {
                    continue;
                };
                if meta.is_dir() {
                    if depth + 1 < MAX_DEPTH {
                        stack.push((path, depth + 1));
                    }
                } else if meta.is_file() && is_font_file(&path) {
                    files.push(path);
                }
            }
        }
        files.sort();
        files.dedup();
        let mut locator = FontLocator {
            file_names: Vec::with_capacity(files.len()),
            ..FontLocator::default()
        };
        for path in files {
            let file = locator.files.len();
            let name = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("")
                .to_lowercase();
            if let Ok(mut handle) = File::open(&path) {
                read_faces(&mut handle, file, &mut locator.faces);
            }
            locator.files.push(path);
            locator.file_names.push(name);
        }
        locator
    }

    /// The indexed font files, sorted by path.
    pub fn files(&self) -> &[PathBuf] {
        &self.files
    }

    /// Number of indexed faces across all files.
    pub fn face_count(&self) -> usize {
        self.faces.len()
    }

    /// Picks an installed face for `request`: the face with the same PostScript name, else the
    /// closest face of the same family, else a class and script stand-in.
    pub fn find(&self, request: &FontRequest<'_>) -> Option<Substitute> {
        let want = Wanted::from_request(request);
        let visible = |f: &&FaceEntry| !f.hidden;
        if let Some(face) = self
            .faces
            .iter()
            .filter(visible)
            .find(|f| f.ps_key == want.ps_key)
        {
            return Some(self.substitute(face, MatchQuality::Exact));
        }
        if let Some(face) = self.best(
            self.faces
                .iter()
                .filter(|f| f.family_key == want.family_key),
            &want,
        ) {
            return Some(self.substitute(face, MatchQuality::Family));
        }
        for group in fallback_groups(want.class, want.script) {
            let hint = group.family.map(key);
            let candidates = self.faces.iter().filter(|f| {
                group
                    .files
                    .iter()
                    .any(|name| self.file_names[f.file] == name.to_lowercase())
                    && hint.as_ref().is_none_or(|h| &f.family_key == h)
            });
            if let Some(face) = self.best(candidates, &want) {
                return Some(self.substitute(face, MatchQuality::Fallback));
            }
        }
        None
    }

    fn best<'a>(
        &self,
        faces: impl Iterator<Item = &'a FaceEntry>,
        want: &Wanted,
    ) -> Option<&'a FaceEntry> {
        faces.min_by_key(|f| {
            let mut cost = u32::from(f.weight.abs_diff(want.weight));
            if f.italic != want.italic {
                cost += 1000;
            }
            cost += 300 * u32::from(f.width.abs_diff(5));
            if f.hidden {
                cost += 10_000;
            }
            cost
        })
    }

    fn substitute(&self, face: &FaceEntry, quality: MatchQuality) -> Substitute {
        Substitute {
            path: self.files[face.file].clone(),
            face_index: face.index,
            postscript_name: face.postscript_name.clone(),
            quality,
        }
    }
}

/// Reads up to `len` bytes at `offset`; fewer at end of file.
fn read_at(file: &mut File, offset: u64, len: usize) -> Option<Vec<u8>> {
    file.seek(SeekFrom::Start(offset)).ok()?;
    let mut buf = Vec::new();
    file.by_ref().take(len as u64).read_to_end(&mut buf).ok()?;
    Some(buf)
}

fn read_faces(file: &mut File, file_index: usize, out: &mut Vec<FaceEntry>) {
    let Some(head) = read_at(file, 0, 12) else {
        return;
    };
    if head.get(0..4) == Some(&TAG_TTCF[..]) {
        let count = read_u32(&head, 8).unwrap_or(0).min(MAX_FACES_PER_FILE);
        let Some(offsets) = read_at(file, 12, count as usize * 4) else {
            return;
        };
        for i in 0..count {
            if let Some(off) = read_u32(&offsets, i as usize * 4)
                && let Some(face) = read_sfnt_face(file, u64::from(off), file_index, i)
            {
                out.push(face);
            }
        }
    } else if read_u32(&head, 0).is_some_and(sfnt::is_sfnt_version) {
        if let Some(face) = read_sfnt_face(file, 0, file_index, 0) {
            out.push(face);
        }
    } else if head.first() == Some(&0x80) || head.starts_with(b"%!") {
        let Some(prefix) = read_at(file, 0, TYPE1_PREFIX) else {
            return;
        };
        let header = Type1Header::parse(&prefix);
        let Some(ps) = header.font_name else { return };
        let family = header
            .family_name
            .unwrap_or_else(|| ps.split('-').next().unwrap_or(&ps).to_owned());
        let style = format!(
            "{} {}",
            header.weight.as_deref().unwrap_or(""),
            header.full_name.as_deref().unwrap_or("")
        );
        out.push(FaceEntry {
            file: file_index,
            index: 0,
            ps_key: key(&ps),
            family_key: key(&family),
            postscript_name: ps,
            weight: weight_from_words(&key(&style)).unwrap_or(400),
            italic: header.italic_angle != 0.0,
            width: 5,
            hidden: false,
        });
    }
}

fn read_sfnt_face(
    file: &mut File,
    offset: u64,
    file_index: usize,
    index: u32,
) -> Option<FaceEntry> {
    let header = read_at(file, offset, 12)?;
    let num_tables = read_u16(&header, 4)?.min(512);
    let dir = read_at(file, offset + 12, usize::from(num_tables) * 16)?;
    let table = |tag: &[u8; 4]| -> Option<(u64, usize)> {
        (0..usize::from(num_tables)).find_map(|i| {
            let rec = dir.get(i * 16..i * 16 + 16)?;
            (&rec[0..4] == tag).then(|| {
                (
                    u64::from(read_u32(rec, 8).unwrap_or(0)),
                    read_u32(rec, 12).unwrap_or(0) as usize,
                )
            })
        })
    };
    let (name_off, name_len) = table(b"name")?;
    let name = read_at(file, name_off, name_len.min(MAX_NAME_TABLE))?;
    let os2 = table(b"OS/2").and_then(|(o, l)| read_at(file, o, l.min(96)));
    let mac_style = table(b"head")
        .and_then(|(o, _)| read_at(file, o + 44, 2))
        .and_then(|b| read_u16(&b, 0))
        .unwrap_or(0);
    let family = sfnt::name_string(&name, 16).or_else(|| sfnt::name_string(&name, 1))?;
    let style = sfnt::name_string(&name, 17)
        .or_else(|| sfnt::name_string(&name, 2))
        .unwrap_or_default();
    let postscript_name =
        sfnt::name_string(&name, 6).unwrap_or_else(|| format!("{family}-{style}"));
    let fs_selection = os2.as_deref().and_then(|t| read_u16(t, 62)).unwrap_or(0);
    let style_key = key(&style);
    let italic = fs_selection & 0x201 != 0
        || mac_style & 2 != 0
        || style_key.contains("italic")
        || style_key.contains("oblique");
    let weight = match os2.as_deref().and_then(|t| read_u16(t, 4)) {
        Some(w @ 1..=9) => w * 100,
        Some(w @ 10..=1000) => w,
        _ => weight_from_words(&style_key).unwrap_or(if mac_style & 1 != 0 { 700 } else { 400 }),
    };
    let width = os2
        .as_deref()
        .and_then(|t| read_u16(t, 6))
        .filter(|w| (1..=9).contains(w))
        .unwrap_or(5);
    Some(FaceEntry {
        file: file_index,
        index,
        ps_key: key(&postscript_name),
        family_key: key(&family),
        hidden: family.starts_with('.'),
        postscript_name,
        weight,
        italic,
        width,
    })
}

/// Weight named by style words in a normalized (lowercase alphanumeric) name.
fn weight_from_words(k: &str) -> Option<u16> {
    const WORDS: [(&str, u16); 13] = [
        ("extrabold", 800),
        ("ultrabold", 800),
        ("semibold", 600),
        ("demibold", 600),
        ("extralight", 200),
        ("ultralight", 200),
        ("black", 900),
        ("heavy", 900),
        ("bold", 700),
        ("demi", 600),
        ("medium", 500),
        ("light", 300),
        ("thin", 100),
    ];
    WORDS.iter().find(|(w, _)| k.contains(w)).map(|(_, v)| *v)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Class {
    Sans,
    Serif,
    Mono,
    Symbol,
    Dingbats,
}

struct Wanted {
    ps_key: String,
    family_key: String,
    weight: u16,
    italic: bool,
    class: Class,
    script: Script,
}

impl Wanted {
    fn from_request(req: &FontRequest<'_>) -> Wanted {
        let name = strip_subset_tag(req.base_font.trim());
        let (family, style) = match name.split_once(',').or_else(|| name.split_once('-')) {
            Some((f, s)) => (f, s),
            None => (name, ""),
        };
        let mut family_key = key(family);
        for _ in 0..3 {
            let stripped = [
                "psmt",
                "mt",
                "ps",
                "bolditalic",
                "boldoblique",
                "bold",
                "italic",
                "oblique",
                "regular",
            ]
            .iter()
            .find_map(|s| family_key.strip_suffix(s).filter(|rest| rest.len() >= 3));
            match stripped {
                Some(rest) => family_key = rest.to_owned(),
                None => break,
            }
        }
        let style_key = key(style);
        let full_key = key(name);
        let style_words = if style_key.is_empty() {
            &full_key[family_key.len().min(full_key.len())..]
        } else {
            &style_key
        };
        let weight = weight_from_words(style_words)
            .or(req.weight.filter(|w| (100..=900).contains(w)))
            .unwrap_or(if req.flags & FLAG_FORCE_BOLD != 0 {
                700
            } else {
                400
            });
        let italic = style_words.contains("italic")
            || style_words.contains("oblique")
            || style_words.ends_with("it")
            || req.flags & FLAG_ITALIC != 0;
        let script = match req.script {
            Script::Latin => script_from_name(&family_key),
            s => s,
        };
        Wanted {
            ps_key: full_key,
            class: classify(&family_key, req.flags),
            family_key,
            weight,
            italic,
            script,
        }
    }
}

fn script_from_name(k: &str) -> Script {
    const JAPANESE: [&str; 9] = [
        "mincho",
        "msgothic",
        "mspgothic",
        "meiryo",
        "hiragino",
        "hirakaku",
        "hiramin",
        "kozmin",
        "kozgo",
    ];
    const SIMPLIFIED: [&str; 8] = [
        "simsun", "simhei", "stsong", "stheiti", "songti", "fangsong", "kaiti", "yahei",
    ];
    const TRADITIONAL: [&str; 4] = ["mingliu", "msung", "mhei", "dfkai"];
    const KOREAN: [&str; 6] = ["batang", "gulim", "dotum", "malgun", "hygothic", "myeongjo"];
    let has = |words: &[&str]| words.iter().any(|w| k.contains(w));
    if has(&JAPANESE) {
        Script::Japanese
    } else if has(&SIMPLIFIED) {
        Script::SimplifiedChinese
    } else if has(&TRADITIONAL) {
        Script::TraditionalChinese
    } else if has(&KOREAN) {
        Script::Korean
    } else {
        Script::Latin
    }
}

fn classify(family_key: &str, flags: u32) -> Class {
    const MONO: [&str; 9] = [
        "courier",
        "mono",
        "consol",
        "typewriter",
        "menlo",
        "monaco",
        "fixed",
        "andale",
        "lucidaconsole",
    ];
    const SANS: [&str; 12] = [
        "sans",
        "helvetica",
        "arial",
        "helv",
        "verdana",
        "tahoma",
        "calibri",
        "gothic",
        "grotesk",
        "frutiger",
        "univers",
        "futura",
    ];
    const SERIF: [&str; 20] = [
        "times",
        "serif",
        "roman",
        "georgia",
        "garamond",
        "cambria",
        "palatino",
        "bookman",
        "bookantiqua",
        "century",
        "minion",
        "bodoni",
        "baskerville",
        "caslon",
        "didot",
        "charter",
        "mincho",
        "ming",
        "song",
        "batang",
    ];
    let has = |words: &[&str]| words.iter().any(|w| family_key.contains(w));
    if family_key.contains("dingbat") {
        Class::Dingbats
    } else if family_key.starts_with("symbol") {
        Class::Symbol
    } else if has(&MONO) {
        Class::Mono
    } else if has(&SANS) {
        Class::Sans
    } else if has(&SERIF) {
        Class::Serif
    } else if flags & FLAG_FIXED_PITCH != 0 {
        Class::Mono
    } else if flags & FLAG_SERIF != 0 {
        Class::Serif
    } else {
        Class::Sans
    }
}

/// Files that make up one family, and the face family to take from them (for collections
/// that mix families).
struct Group {
    files: &'static [&'static str],
    family: Option<&'static str>,
}

const fn group(files: &'static [&'static str], family: Option<&'static str>) -> Group {
    Group { files, family }
}

const LATIN_SANS: &[Group] = &[
    group(&["Helvetica.ttc"], None),
    group(
        &[
            "Arial.ttf",
            "Arial Bold.ttf",
            "Arial Italic.ttf",
            "Arial Bold Italic.ttf",
        ],
        None,
    ),
    group(
        &[
            "LiberationSans-Regular.ttf",
            "LiberationSans-Bold.ttf",
            "LiberationSans-Italic.ttf",
            "LiberationSans-BoldItalic.ttf",
        ],
        None,
    ),
    group(
        &[
            "NimbusSans-Regular.otf",
            "NimbusSans-Bold.otf",
            "NimbusSans-Italic.otf",
            "NimbusSans-BoldItalic.otf",
        ],
        None,
    ),
    group(
        &[
            "DejaVuSans.ttf",
            "DejaVuSans-Bold.ttf",
            "DejaVuSans-Oblique.ttf",
            "DejaVuSans-BoldOblique.ttf",
        ],
        None,
    ),
    group(
        &[
            "NotoSans-Regular.ttf",
            "NotoSans-Bold.ttf",
            "NotoSans-Italic.ttf",
            "NotoSans-BoldItalic.ttf",
        ],
        None,
    ),
];

const LATIN_SERIF: &[Group] = &[
    group(&["Times.ttc"], None),
    group(
        &[
            "Times New Roman.ttf",
            "Times New Roman Bold.ttf",
            "Times New Roman Italic.ttf",
            "Times New Roman Bold Italic.ttf",
        ],
        None,
    ),
    group(
        &[
            "LiberationSerif-Regular.ttf",
            "LiberationSerif-Bold.ttf",
            "LiberationSerif-Italic.ttf",
            "LiberationSerif-BoldItalic.ttf",
        ],
        None,
    ),
    group(
        &[
            "NimbusRoman-Regular.otf",
            "NimbusRoman-Bold.otf",
            "NimbusRoman-Italic.otf",
            "NimbusRoman-BoldItalic.otf",
        ],
        None,
    ),
    group(
        &[
            "DejaVuSerif.ttf",
            "DejaVuSerif-Bold.ttf",
            "DejaVuSerif-Italic.ttf",
            "DejaVuSerif-BoldItalic.ttf",
        ],
        None,
    ),
    group(
        &[
            "NotoSerif-Regular.ttf",
            "NotoSerif-Bold.ttf",
            "NotoSerif-Italic.ttf",
            "NotoSerif-BoldItalic.ttf",
        ],
        None,
    ),
];

const LATIN_MONO: &[Group] = &[
    group(&["Courier.ttc"], None),
    group(
        &[
            "Courier New.ttf",
            "Courier New Bold.ttf",
            "Courier New Italic.ttf",
            "Courier New Bold Italic.ttf",
        ],
        None,
    ),
    group(
        &[
            "LiberationMono-Regular.ttf",
            "LiberationMono-Bold.ttf",
            "LiberationMono-Italic.ttf",
            "LiberationMono-BoldItalic.ttf",
        ],
        None,
    ),
    group(
        &[
            "NimbusMonoPS-Regular.otf",
            "NimbusMonoPS-Bold.otf",
            "NimbusMonoPS-Italic.otf",
            "NimbusMonoPS-BoldItalic.otf",
        ],
        None,
    ),
    group(
        &[
            "DejaVuSansMono.ttf",
            "DejaVuSansMono-Bold.ttf",
            "DejaVuSansMono-Oblique.ttf",
            "DejaVuSansMono-BoldOblique.ttf",
        ],
        None,
    ),
    group(&["NotoSansMono-Regular.ttf", "NotoSansMono-Bold.ttf"], None),
    group(&["Menlo.ttc"], None),
];

const SYMBOL: &[Group] = &[
    group(&["Symbol.ttf"], None),
    group(
        &[
            "StandardSymbolsPS.otf",
            "StandardSymbolsPS.t1",
            "s050000l.pfb",
        ],
        None,
    ),
];

const DINGBATS: &[Group] = &[
    group(&["ZapfDingbats.ttf"], None),
    group(&["D050000L.otf", "D050000L.t1", "d050000l.pfb"], None),
];

const JAPANESE_SANS: &[Group] = &[
    group(
        &["ヒラギノ角ゴシック W3.ttc", "ヒラギノ角ゴシック W6.ttc"],
        Some("Hiragino Kaku Gothic ProN"),
    ),
    group(
        &["NotoSansCJK-Regular.ttc", "NotoSansCJK-Bold.ttc"],
        Some("Noto Sans CJK JP"),
    ),
    group(
        &["NotoSansCJKjp-Regular.otf", "NotoSansCJKjp-Bold.otf"],
        None,
    ),
    group(
        &[
            "NotoSansJP-Regular.otf",
            "NotoSansJP-Bold.otf",
            "NotoSansJP-Regular.ttf",
            "NotoSansJP-Bold.ttf",
        ],
        None,
    ),
    group(
        &["ipagp.ttf", "ipag.ttf", "fonts-japanese-gothic.ttf"],
        None,
    ),
    group(
        &["DroidSansFallbackFull.ttf", "DroidSansFallback.ttf"],
        None,
    ),
    group(&["Arial Unicode.ttf"], None),
];

const JAPANESE_SERIF: &[Group] = &[
    group(&["ヒラギノ明朝 ProN.ttc"], Some("Hiragino Mincho ProN")),
    group(
        &["NotoSerifCJK-Regular.ttc", "NotoSerifCJK-Bold.ttc"],
        Some("Noto Serif CJK JP"),
    ),
    group(
        &["NotoSerifCJKjp-Regular.otf", "NotoSerifCJKjp-Bold.otf"],
        None,
    ),
    group(
        &["ipamp.ttf", "ipam.ttf", "fonts-japanese-mincho.ttf"],
        None,
    ),
];

const SIMPLIFIED_SANS: &[Group] = &[
    group(&["Hiragino Sans GB.ttc"], Some("Hiragino Sans GB")),
    group(
        &["STHeiti Light.ttc", "STHeiti Medium.ttc"],
        Some("Heiti SC"),
    ),
    group(
        &["NotoSansCJK-Regular.ttc", "NotoSansCJK-Bold.ttc"],
        Some("Noto Sans CJK SC"),
    ),
    group(
        &["NotoSansCJKsc-Regular.otf", "NotoSansCJKsc-Bold.otf"],
        None,
    ),
    group(
        &[
            "NotoSansSC-Regular.otf",
            "NotoSansSC-Bold.otf",
            "NotoSansSC-Regular.ttf",
            "NotoSansSC-Bold.ttf",
        ],
        None,
    ),
    group(&["wqy-microhei.ttc", "wqy-zenhei.ttc"], None),
    group(
        &["DroidSansFallbackFull.ttf", "DroidSansFallback.ttf"],
        None,
    ),
    group(&["Arial Unicode.ttf"], None),
];

const SIMPLIFIED_SERIF: &[Group] = &[
    group(&["Songti.ttc"], Some("Songti SC")),
    group(
        &["NotoSerifCJK-Regular.ttc", "NotoSerifCJK-Bold.ttc"],
        Some("Noto Serif CJK SC"),
    ),
    group(
        &["NotoSerifCJKsc-Regular.otf", "NotoSerifCJKsc-Bold.otf"],
        None,
    ),
];

const TRADITIONAL_SANS: &[Group] = &[
    group(
        &["STHeiti Light.ttc", "STHeiti Medium.ttc"],
        Some("Heiti TC"),
    ),
    group(
        &["NotoSansCJK-Regular.ttc", "NotoSansCJK-Bold.ttc"],
        Some("Noto Sans CJK TC"),
    ),
    group(
        &["NotoSansCJKtc-Regular.otf", "NotoSansCJKtc-Bold.otf"],
        None,
    ),
    group(
        &[
            "NotoSansTC-Regular.otf",
            "NotoSansTC-Bold.otf",
            "NotoSansTC-Regular.ttf",
            "NotoSansTC-Bold.ttf",
        ],
        None,
    ),
    group(&["wqy-microhei.ttc", "wqy-zenhei.ttc"], None),
    group(
        &["DroidSansFallbackFull.ttf", "DroidSansFallback.ttf"],
        None,
    ),
    group(&["Arial Unicode.ttf"], None),
];

const TRADITIONAL_SERIF: &[Group] = &[
    group(&["Songti.ttc"], Some("Songti TC")),
    group(
        &["NotoSerifCJK-Regular.ttc", "NotoSerifCJK-Bold.ttc"],
        Some("Noto Serif CJK TC"),
    ),
    group(
        &["NotoSerifCJKtc-Regular.otf", "NotoSerifCJKtc-Bold.otf"],
        None,
    ),
];

const KOREAN_SANS: &[Group] = &[
    group(&["AppleSDGothicNeo.ttc"], Some("Apple SD Gothic Neo")),
    group(&["AppleGothic.ttf"], None),
    group(
        &["NotoSansCJK-Regular.ttc", "NotoSansCJK-Bold.ttc"],
        Some("Noto Sans CJK KR"),
    ),
    group(
        &["NotoSansCJKkr-Regular.otf", "NotoSansCJKkr-Bold.otf"],
        None,
    ),
    group(
        &[
            "NotoSansKR-Regular.otf",
            "NotoSansKR-Bold.otf",
            "NotoSansKR-Regular.ttf",
            "NotoSansKR-Bold.ttf",
        ],
        None,
    ),
    group(
        &["NanumGothic.ttf", "NanumGothicBold.ttf", "UnDotum.ttf"],
        None,
    ),
    group(
        &["DroidSansFallbackFull.ttf", "DroidSansFallback.ttf"],
        None,
    ),
    group(&["Arial Unicode.ttf"], None),
];

const KOREAN_SERIF: &[Group] = &[
    group(&["AppleMyungjo.ttf"], None),
    group(
        &["NotoSerifCJK-Regular.ttc", "NotoSerifCJK-Bold.ttc"],
        Some("Noto Serif CJK KR"),
    ),
    group(
        &["NotoSerifCJKkr-Regular.otf", "NotoSerifCJKkr-Bold.otf"],
        None,
    ),
    group(
        &["NanumMyeongjo.ttf", "NanumMyeongjoBold.ttf", "UnBatang.ttf"],
        None,
    ),
];

/// Stand-in families in order of preference. Serif CJK requests fall back to sans faces.
fn fallback_groups(class: Class, script: Script) -> impl Iterator<Item = &'static Group> {
    let serif = class == Class::Serif;
    let lists: [&'static [Group]; 2] = match script {
        Script::Japanese => [if serif { JAPANESE_SERIF } else { &[] }, JAPANESE_SANS],
        Script::SimplifiedChinese => [if serif { SIMPLIFIED_SERIF } else { &[] }, SIMPLIFIED_SANS],
        Script::TraditionalChinese => [
            if serif { TRADITIONAL_SERIF } else { &[] },
            TRADITIONAL_SANS,
        ],
        Script::Korean => [if serif { KOREAN_SERIF } else { &[] }, KOREAN_SANS],
        Script::Latin => match class {
            Class::Sans => [LATIN_SANS, &[]],
            Class::Serif => [LATIN_SERIF, LATIN_SANS],
            Class::Mono => [LATIN_MONO, LATIN_SANS],
            Class::Symbol => [SYMBOL, &[]],
            Class::Dingbats => [DINGBATS, &[]],
        },
    };
    lists.into_iter().flatten()
}
