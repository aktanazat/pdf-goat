use pdf_raster::{
    Canvas, Color, FillRule, GlyphCache, Paint, Path, PathBuilder, Transform, rasterize_outline,
};

fn polygon(points: &[(f64, f64)]) -> Path {
    let mut b = PathBuilder::new();
    let (x, y) = points[0];
    b.move_to(x, y);
    for &(x, y) in &points[1..] {
        b.line_to(x, y);
    }
    b.close();
    b.finish()
}

/// The bitmap covers the outline's pixel box, and a partial pixel gets the
/// exact area of the outline inside it. FreeType's fill rule turns a
/// negative area into `!coverage`, so a contour wound the other way reads
/// one level lower; MuPDF shows the same.
#[test]
fn fractional_rectangle_gets_exact_area_coverage() {
    let path = polygon(&[(1.25, 0.0), (1.25, 2.0), (3.5, 2.0), (3.5, 0.0)]);
    let mask = rasterize_outline(&path, &Transform::IDENTITY, FillRule::NonZero).unwrap();
    assert_eq!(
        (
            mask.rect().x0,
            mask.rect().y0,
            mask.rect().x1,
            mask.rect().y1
        ),
        (1, 0, 4, 2)
    );
    assert_eq!(mask.data(), &[192, 255, 128, 192, 255, 128]);
    let reversed = polygon(&[(1.25, 0.0), (3.5, 0.0), (3.5, 2.0), (1.25, 2.0)]);
    let mask = rasterize_outline(&reversed, &Transform::IDENTITY, FillRule::NonZero).unwrap();
    assert_eq!(mask.data(), &[191, 255, 127, 191, 255, 127]);
}

#[test]
fn diagonal_edge_splits_pixels_by_area() {
    let path = polygon(&[(0.0, 0.0), (0.0, 2.0), (2.0, 0.0)]);
    let mask = rasterize_outline(&path, &Transform::IDENTITY, FillRule::NonZero).unwrap();
    assert_eq!(mask.rect().width(), 2);
    assert_eq!(mask.data(), &[255, 128, 128, 0]);
}

/// A quadratic is walked as a curve, in FreeType's eight chords for this
/// size: the region under the parabola from (0, 0) through control (4, 8)
/// to (8, 0) has area 2/3 · 8 · 4 = 21.33 pixels, and eight inscribed
/// chords keep 63/64 of it.
#[test]
fn quadratic_segment_is_curved_not_chorded() {
    let mut b = PathBuilder::new();
    b.move_to(8.0, 0.0);
    b.quad_to(4.0, 8.0, 0.0, 0.0);
    b.close();
    let mask = rasterize_outline(&b.finish(), &Transform::IDENTITY, FillRule::NonZero).unwrap();
    let area: u32 = mask.data().iter().map(|&v| u32::from(v)).sum();
    let chords = (2.0 / 3.0) * 8.0 * 4.0 * (63.0 / 64.0) * 255.0;
    assert!(
        (f64::from(area) - chords).abs() < 10.0,
        "area {area} vs {chords}"
    );
}

#[test]
fn even_odd_leaves_the_inner_square_empty_but_nonzero_fills_it() {
    let mut b = PathBuilder::new();
    b.rect(0.0, 0.0, 6.0, 6.0);
    b.rect(2.0, 2.0, 2.0, 2.0);
    let path = b.finish();
    let even_odd = rasterize_outline(&path, &Transform::IDENTITY, FillRule::EvenOdd).unwrap();
    assert_eq!(even_odd.value(3, 3), 0);
    assert_eq!(even_odd.value(1, 1), 255);
    let nonzero = rasterize_outline(&path, &Transform::IDENTITY, FillRule::NonZero).unwrap();
    assert_eq!(nonzero.value(3, 3), 255);
}

/// A 1000-unit font, as a CFF or Type 1 program has.
const UNITS: Transform = Transform::new(0.001, 0.0, 0.0, 0.001, 0.0, 0.0);

/// Glyph origins are quantised to quarter pixels along the baseline (text
/// under 24 px), so two placements in the same quarter share one bitmap
/// and a third one further along needs a second.
#[test]
fn glyph_cache_shares_bitmaps_within_a_quarter_pixel() {
    let square = polygon(&[(0.0, 0.0), (0.0, 500.0), (500.0, 500.0), (500.0, 0.0)]);
    let mut cache = GlyphCache::new();
    let at = |x: f64| Transform::new(12.0, 0.0, 0.0, 12.0, x, 20.7);
    let first = cache.glyph(1, 7, &square, &UNITS, &at(10.3)).unwrap();
    assert_eq!((first.x, first.y), (10, 21));
    assert_eq!(first.mask.rect().width(), 7);
    assert_eq!(&first.mask.data()[..7], &[192, 255, 255, 255, 255, 255, 64]);
    let second = cache.glyph(1, 7, &square, &UNITS, &at(10.35)).unwrap();
    assert_eq!(cache.renders(), 1);
    assert_eq!(second.x, 10);
    assert_eq!(second.mask.data(), first.mask.data());
    cache.glyph(1, 7, &square, &UNITS, &at(10.4)).unwrap();
    assert_eq!(cache.renders(), 2);
    cache.glyph(1, 8, &square, &UNITS, &at(10.3)).unwrap();
    assert_eq!(cache.renders(), 3, "another glyph id is another bitmap");
}

#[test]
fn glyphs_above_the_bitmap_size_limit_are_not_cached() {
    let square = polygon(&[(0.0, 0.0), (500.0, 0.0), (500.0, 500.0), (0.0, 500.0)]);
    let mut cache = GlyphCache::new();
    let big = Transform::new(300.0, 0.0, 0.0, 300.0, 0.0, 0.0);
    assert!(cache.glyph(1, 1, &square, &UNITS, &big).is_none());
    assert_eq!(cache.renders(), 0);
}

/// A bitmap painted at its pixel origin lands where the glyph is: a 6 px
/// square at x = 10.5 covers half of columns 10 and 16. MuPDF expands
/// coverage 128 to weight 129: (255 * (256 - 129)) >> 8 = 126 over white.
#[test]
fn bitmap_painted_at_its_origin_lands_on_the_glyph() {
    let square = polygon(&[(0.0, 0.0), (0.0, 500.0), (500.0, 500.0), (500.0, 0.0)]);
    let trm = Transform::new(12.0, 0.0, 0.0, 12.0, 10.5, 4.0);
    let mut cache = GlyphCache::new();
    let glyph = cache.glyph(1, 1, &square, &UNITS, &trm).unwrap();
    let mut canvas = Canvas::new(32, 16).unwrap();
    canvas.clear(Color::WHITE);
    canvas.fill_mask_at(&glyph.mask, glyph.x, glyph.y, &Paint::solid(Color::BLACK));
    let bitmap = canvas.finish();
    assert_eq!(bitmap.pixel(12, 6), Some([0, 0, 0, 255]));
    assert_eq!(bitmap.pixel(10, 6), Some([126, 126, 126, 255]));
    assert_eq!(bitmap.pixel(16, 6), Some([126, 126, 126, 255]));
    assert_eq!(bitmap.pixel(9, 6), Some([255, 255, 255, 255]));
    assert_eq!(bitmap.pixel(17, 6), Some([255, 255, 255, 255]));
    assert_eq!(bitmap.pixel(12, 3), Some([255, 255, 255, 255]));
    assert_eq!(bitmap.pixel(12, 4), Some([0, 0, 0, 255]));
    assert_eq!(bitmap.pixel(12, 9), Some([0, 0, 0, 255]));
    assert_eq!(bitmap.pixel(12, 10), Some([255, 255, 255, 255]));
}

#[test]
fn implied_quadratic_points_are_invariant_under_integer_translation() {
    let mut b = PathBuilder::new();
    b.move_to(0.0, -6.0);
    b.quad_to(0.0, -1.0 / 64.0, 2.0, -1.5 / 64.0);
    b.quad_to(4.0, -2.0 / 64.0, 4.0, -6.0);
    b.close();
    let path = b.finish();
    let negative = rasterize_outline(&path, &Transform::IDENTITY, FillRule::NonZero).unwrap();
    let positive = rasterize_outline(
        &path,
        &Transform::new(1.0, 0.0, 0.0, 1.0, 0.0, 8.0),
        FillRule::NonZero,
    )
    .unwrap();
    assert_eq!(negative.rect().y0 + 8, positive.rect().y0);
    assert_eq!(negative.data(), positive.data());
}
