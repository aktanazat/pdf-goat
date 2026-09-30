use pdf_font::{Font, FontError, Rect};

fn header(contours: i16, bounds: [i16; 4]) -> Vec<u8> {
    [contours, bounds[0], bounds[1], bounds[2], bounds[3]]
        .into_iter()
        .flat_map(i16::to_be_bytes)
        .collect()
}

fn simple(points: &[(i16, i16, bool)], bounds: [i16; 4]) -> Vec<u8> {
    let mut glyph = header(1, bounds);
    glyph.extend_from_slice(&((points.len() - 1) as u16).to_be_bytes());
    glyph.extend_from_slice(&[0, 0]);
    glyph.extend(points.iter().map(|point| u8::from(point.2)));
    let mut previous = 0;
    for &(x, _, _) in points {
        glyph.extend_from_slice(&(x - previous).to_be_bytes());
        previous = x;
    }
    previous = 0;
    for &(_, y, _) in points {
        glyph.extend_from_slice(&(y - previous).to_be_bytes());
        previous = y;
    }
    glyph
}

fn rectangle() -> Vec<u8> {
    simple(
        &[
            (100, 200, true),
            (500, 200, true),
            (500, 800, true),
            (100, 800, true),
        ],
        [100, 200, 500, 800],
    )
}

fn composite(child: u16, flags: u16, bounds: [i16; 4]) -> Vec<u8> {
    let mut glyph = header(-1, bounds);
    glyph.extend_from_slice(&(flags | 3).to_be_bytes());
    glyph.extend_from_slice(&child.to_be_bytes());
    glyph.extend_from_slice(&20i16.to_be_bytes());
    glyph.extend_from_slice(&30i16.to_be_bytes());
    glyph
}

fn sfnt(tables: Vec<([u8; 4], Vec<u8>)>) -> Vec<u8> {
    let mut data = vec![0u8; 12 + tables.len() * 16];
    data[..4].copy_from_slice(&0x0001_0000u32.to_be_bytes());
    data[4..6].copy_from_slice(&(tables.len() as u16).to_be_bytes());
    for (i, (tag, table)) in tables.into_iter().enumerate() {
        let offset = data.len() as u32;
        let record = 12 + i * 16;
        data[record..record + 4].copy_from_slice(&tag);
        data[record + 8..record + 12].copy_from_slice(&offset.to_be_bytes());
        data[record + 12..record + 16].copy_from_slice(&(table.len() as u32).to_be_bytes());
        data.extend_from_slice(&table);
        data.resize(data.len().next_multiple_of(4), 0);
    }
    data
}

fn font_tables(glyphs: &[Vec<u8>], bearings: &[i16]) -> Vec<([u8; 4], Vec<u8>)> {
    let count = glyphs.len() as u16;
    let mut head = vec![0u8; 54];
    head[18..20].copy_from_slice(&1000u16.to_be_bytes());
    head[50..52].copy_from_slice(&1i16.to_be_bytes());
    let mut hhea = vec![0u8; 36];
    hhea[34..36].copy_from_slice(&count.to_be_bytes());
    let mut maxp = vec![0, 1, 0, 0];
    maxp.extend_from_slice(&count.to_be_bytes());
    let mut hmtx = Vec::new();
    let mut loca = vec![0u8; 4];
    let mut glyf = Vec::new();
    for (glyph, bearing) in glyphs.iter().zip(bearings) {
        hmtx.extend_from_slice(&600u16.to_be_bytes());
        hmtx.extend_from_slice(&bearing.to_be_bytes());
        glyf.extend_from_slice(glyph);
        loca.extend_from_slice(&(glyf.len() as u32).to_be_bytes());
    }
    // A zero-length record still needs the table directory to point inside the file.
    glyf.push(0);
    vec![
        (*b"head", head),
        (*b"hhea", hhea),
        (*b"maxp", maxp),
        (*b"hmtx", hmtx),
        (*b"loca", loca),
        (*b"glyf", glyf),
    ]
}

fn font(glyphs: &[Vec<u8>], bearings: &[i16]) -> Font {
    Font::parse(sfnt(font_tables(glyphs, bearings))).expect("valid sfnt fixture")
}

macro_rules! empty_glyph {
    ($name:ident, $glyph:expr) => {
        #[test]
        fn $name() {
            let font = font(&[$glyph], &[0]);
            assert_eq!(font.glyph_bounds(0), Ok(None));
        }
    };
}

empty_glyph!(empty_loca_has_no_glyph_bounds, Vec::new());
empty_glyph!(
    zero_contours_ignore_nonzero_header_bounds,
    header(0, [-50, -50, 50, 50])
);
empty_glyph!(
    single_oncurve_hint_point_has_no_glyph_bounds,
    simple(&[(100, 200, true)], [100, 200, 100, 200])
);

#[test]
fn zero_area_offcurve_contour_is_not_an_empty_outline() {
    let font = font(
        &[simple(&[(100, 200, false)], [100, 200, 100, 200])],
        &[100],
    );
    assert_eq!(
        font.glyph_bounds(0),
        Ok(Some(Rect {
            x_min: 100.0,
            y_min: 200.0,
            x_max: 100.0,
            y_max: 200.0
        }))
    );
}

#[test]
fn stored_bounds_receive_the_outline_phantom_origin_shift() {
    let font = font(&[rectangle()], &[50]);
    assert_eq!(
        font.glyph_bounds(0),
        Ok(Some(Rect {
            x_min: 50.0,
            y_min: 200.0,
            x_max: 450.0,
            y_max: 800.0
        }))
    );
}

#[test]
fn composite_bounds_use_the_component_metrics_origin() {
    let font = font(
        &[rectangle(), composite(0, 0x0200, [120, 230, 520, 830])],
        &[50, 5],
    );
    assert_eq!(
        font.glyph_bounds(1),
        Ok(Some(Rect {
            x_min: 70.0,
            y_min: 230.0,
            x_max: 470.0,
            y_max: 830.0
        }))
    );
}

#[test]
fn composite_bounds_without_component_metrics_use_the_parent_origin() {
    let font = font(
        &[rectangle(), composite(0, 0, [120, 230, 520, 830])],
        &[50, 10],
    );
    assert_eq!(
        font.glyph_bounds(1),
        Ok(Some(Rect {
            x_min: 10.0,
            y_min: 230.0,
            x_max: 410.0,
            y_max: 830.0
        }))
    );
}

#[test]
fn composite_of_empty_glyphs_has_no_bounds() {
    let font = font(
        &[Vec::new(), composite(0, 0, [-100, -100, 100, 100])],
        &[0, 0],
    );
    assert_eq!(font.glyph_bounds(1), Ok(None));
}

#[test]
fn missing_glyph_id_is_not_an_empty_outline() {
    let font = font(&[rectangle()], &[100]);
    assert_eq!(
        font.glyph_bounds(1),
        Err(FontError::Missing("glyph id out of range"))
    );
}

#[test]
fn truncated_glyph_coordinates_are_not_an_empty_outline() {
    let mut glyph = rectangle();
    glyph.truncate(18);
    let font = font(&[glyph], &[100]);
    assert!(matches!(font.glyph_bounds(0), Err(FontError::Truncated(_))));
}

#[test]
fn missing_glyf_table_is_not_an_empty_outline() {
    let mut tables = font_tables(&[rectangle()], &[100]);
    tables.retain(|(tag, _)| tag != b"glyf");
    let font = Font::parse(sfnt(tables)).expect("font metrics remain available");
    assert_eq!(font.glyph_bounds(0), Err(FontError::Missing("glyf table")));
}

#[test]
fn composite_cycles_report_a_limit_error() {
    let font = font(&[composite(0, 0x0200, [0, 0, 100, 100])], &[0]);
    assert!(matches!(
        font.glyph_bounds(0),
        Err(FontError::LimitExceeded("composite glyph depth"))
    ));
}

fn cff() -> Font {
    let data = vec![
        1, 0, 4, 4, 0, 1, 1, 1, 2, b'B', 0, 1, 1, 1, 3, 160, 17, 0, 0, 0, 0, 0, 2, 1, 1, 2, 14, 14,
        239, 247, 92, 21, 189, 139, 139, 239, 89, 139, 5, 14,
    ];
    Font::parse(data).expect("valid CFF fixture")
}

#[test]
fn cff_empty_charstring_has_no_bounds() {
    assert_eq!(cff().glyph_bounds(0), Ok(None));
}

#[test]
fn cff_bounds_follow_charstring_geometry() {
    assert_eq!(
        cff().glyph_bounds(1),
        Ok(Some(Rect {
            x_min: 100.0,
            y_min: 200.0,
            x_max: 150.0,
            y_max: 300.0
        }))
    );
}
