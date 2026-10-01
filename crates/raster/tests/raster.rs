//! Pixel-exact contract tests through the public drawing API.

use pdf_raster::{
    BlendMode, Canvas, Color, Composite, Dash, FillRule, Filter, Image, ImageFormat, ImageOptions,
    IntRect, LineCap, LineJoin, Paint, Path, PathBuilder, Pixmap, Rect, Stroke, Transform,
};

const ID: Transform = Transform::IDENTITY;

fn alpha_row(p: &Pixmap, y: u32) -> Vec<u8> {
    (0..p.width())
        .map(|x| p.pixel(x, y).map_or(0, |px| px[3]))
        .collect()
}

fn alpha_at(p: &Pixmap, x: u32, y: u32) -> u8 {
    p.pixel(x, y).map_or(0, |px| px[3])
}

fn black() -> Paint<'static> {
    Paint::solid(Color::BLACK)
}

fn polyline(points: &[(f64, f64)], close: bool) -> Path {
    let mut pb = PathBuilder::new();
    for (i, &(x, y)) in points.iter().enumerate() {
        if i == 0 {
            pb.move_to(x, y);
        } else {
            pb.line_to(x, y);
        }
    }
    if close {
        pb.close();
    }
    pb.finish()
}

fn filled(width: u32, height: u32, rect: Rect) -> Pixmap {
    let mut c = Canvas::new(width, height).unwrap();
    c.fill_rect(rect, &ID, &black());
    c.finish()
}

#[test]
fn rect_with_integer_edges_covers_exactly_its_pixels() {
    let p = filled(10, 8, Rect::new(2.0, 3.0, 6.0, 5.0));
    let inside = [0, 0, 255, 255, 255, 255, 0, 0, 0, 0];
    for y in 0..8 {
        let want = if (3..5).contains(&y) {
            inside.to_vec()
        } else {
            vec![0; 10]
        };
        assert_eq!(alpha_row(&p, y), want, "row {y}");
    }
}

#[test]
fn fractional_edges_count_covered_subsamples_of_the_17_by_15_grid() {
    let p = filled(10, 8, Rect::new(2.5, 3.0, 6.0, 5.5));
    // As in MuPDF, coverage is the number of covered subsamples on a 17 × 15
    // grid, 255 for a full pixel. x = 2.5 lands on column ⌊2.5 · 17⌋ = 42,
    // so pixel 2 keeps 51 − 42 = 9 of its 17 columns; y = 5.5 lands on row
    // ⌊5.5 · 15⌋ = 82, so row 5 keeps 82 − 75 = 7 of its 15 rows.
    // MuPDF's solid-span alpha is (255 * E(coverage)) >> 8, where
    // E(v)=v+(v>>7): counts 135,63,119 become alpha 135,62,118.
    assert_eq!(alpha_row(&p, 2), vec![0; 10]);
    assert_eq!(alpha_row(&p, 3), vec![0, 0, 135, 255, 255, 255, 0, 0, 0, 0]);
    assert_eq!(alpha_row(&p, 4), vec![0, 0, 135, 255, 255, 255, 0, 0, 0, 0]);
    assert_eq!(alpha_row(&p, 5), vec![0, 0, 62, 118, 118, 118, 0, 0, 0, 0]);
    assert_eq!(alpha_row(&p, 6), vec![0; 10]);
}

#[test]
fn diagonal_edge_steps_the_subsample_grid_like_mupdf() {
    // The hypotenuse is y = x; everything left of it is inside. On the
    // 17 × 15 grid the edge crosses sub-scanline j of a diagonal pixel at
    // column ⌈17 j / 15⌉, so the pixel holds Σ ⌈17 j / 15⌉ (j < 15) = 126
    // subsamples; the solid-span alpha is (255*126)>>8 = 125.
    let tri = polyline(&[(0.0, 0.0), (8.0, 8.0), (0.0, 8.0)], true);
    let mut c = Canvas::new(8, 8).unwrap();
    c.fill_path(&tri, &ID, FillRule::NonZero, &black());
    assert_eq!(alpha_rows(&c.finish()), diagonal_triangle(8));
}

/// Alpha rows of the `size`-pixel triangle left of `y = x` as the gel
/// paints it: coverage 126 becomes alpha (255*126)>>8 = 125 on the diagonal.
fn diagonal_triangle(size: u32) -> Vec<Vec<u8>> {
    (0..size)
        .map(|y| {
            (0..size)
                .map(|x| match x.cmp(&y) {
                    std::cmp::Ordering::Less => 255,
                    std::cmp::Ordering::Equal => 125,
                    std::cmp::Ordering::Greater => 0,
                })
                .collect()
        })
        .collect()
}

fn alpha_rows(p: &Pixmap) -> Vec<Vec<u8>> {
    (0..p.height()).map(|y| alpha_row(p, y)).collect()
}

fn pentagram(cx: f64, cy: f64, r: f64) -> Path {
    let pts: Vec<(f64, f64)> = (0..5)
        .map(|i| {
            let a = std::f64::consts::PI * (0.5 + 0.8 * f64::from(i));
            (cx + r * a.cos(), cy - r * a.sin())
        })
        .collect();
    polyline(&pts, true)
}

#[test]
fn even_odd_and_nonzero_differ_exactly_in_the_star_center() {
    let star = pentagram(50.0, 50.0, 40.0);
    let draw = |rule| {
        let mut c = Canvas::new(100, 100).unwrap();
        c.fill_path(&star, &ID, rule, &black());
        c.finish()
    };
    let nonzero = draw(FillRule::NonZero);
    let even_odd = draw(FillRule::EvenOdd);
    // The inner pentagon has inradius 0.382 * 40 * cos 36° ≈ 12.4 and
    // circumradius ≈ 15.3.
    for y in 0..100 {
        for x in 0..100 {
            let d = (f64::from(x) + 0.5 - 50.0).hypot(f64::from(y) + 0.5 - 50.0);
            let (nz, eo) = (alpha_at(&nonzero, x, y), alpha_at(&even_odd, x, y));
            if d < 11.0 {
                assert_eq!((nz, eo), (255, 0), "inner pixel ({x}, {y})");
            } else if d > 16.5 {
                assert_eq!(nz, eo, "outer pixel ({x}, {y})");
            }
        }
    }
    // An arm pixel is filled under both rules.
    assert_eq!(
        (alpha_at(&nonzero, 50, 20), alpha_at(&even_odd, 50, 20)),
        (255, 255)
    );
}

#[test]
fn horizontal_stroke_of_width_two_covers_the_two_rows_around_it() {
    let mut c = Canvas::new(60, 40).unwrap();
    let line = polyline(&[(10.0, 20.0), (50.0, 20.0)], false);
    c.stroke_path(
        &line,
        &ID,
        &Stroke {
            width: 2.0,
            ..Stroke::default()
        },
        &black(),
    );
    let p = c.finish();
    let mut covered = vec![0u8; 60];
    covered[10..50].fill(255);
    for y in 0..40 {
        let want = if y == 19 || y == 20 {
            covered.clone()
        } else {
            vec![0; 60]
        };
        assert_eq!(alpha_row(&p, y), want, "row {y}");
    }
}

/// Two width-10 segments meeting at `(50.5, 30)` with angle `phi` between
/// them, opening downwards; returns the alpha 6 to 7 pixels above the apex,
/// inside the miter tip but outside the bevel.
fn apex_alpha(phi_degrees: f64, miter_limit: f64) -> u8 {
    let half = phi_degrees.to_radians() / 2.0;
    let (s, c) = half.sin_cos();
    let apex = (50.5, 30.0);
    let path = polyline(
        &[
            (apex.0 + 40.0 * s, apex.1 + 40.0 * c),
            apex,
            (apex.0 - 40.0 * s, apex.1 + 40.0 * c),
        ],
        false,
    );
    let mut canvas = Canvas::new(100, 100).unwrap();
    let stroke = Stroke {
        width: 10.0,
        join: LineJoin::Miter,
        miter_limit,
        ..Stroke::default()
    };
    canvas.stroke_path(&path, &ID, &stroke, &black());
    alpha_at(&canvas.finish(), 50, 23)
}

#[test]
fn miter_limit_two_keeps_the_miter_above_sixty_degrees() {
    // 1 / sin(60.5° / 2) = 1.985 <= 2: miter tip 9.9 px above the apex.
    assert_eq!(apex_alpha(60.5, 2.0), 255);
}

#[test]
fn miter_limit_two_bevels_below_sixty_degrees() {
    // 1 / sin(59.5° / 2) = 2.015 > 2: the bevel ends 2.5 px above the apex.
    assert_eq!(apex_alpha(59.5, 2.0), 0);
}

#[test]
fn dash_gaps_land_at_the_pattern_positions_after_the_phase() {
    let mut c = Canvas::new(60, 20).unwrap();
    let line = polyline(&[(0.0, 10.0), (60.0, 10.0)], false);
    let stroke = Stroke {
        width: 2.0,
        dash: Some(Dash {
            array: vec![10.0, 5.0],
            phase: 3.0,
        }),
        ..Stroke::default()
    };
    c.stroke_path(&line, &ID, &stroke, &black());
    let p = c.finish();
    // Phase 3 into [10 on, 5 off]: on 0..7, 12..22, 27..37, 42..52, 57..60.
    let on = [(0, 7), (12, 22), (27, 37), (42, 52), (57, 60)];
    let want: Vec<u8> = (0..60)
        .map(|x| {
            if on.iter().any(|&(a, b)| (a..b).contains(&x)) {
                255
            } else {
                0
            }
        })
        .collect();
    assert_eq!(alpha_row(&p, 9), want);
    assert_eq!(alpha_row(&p, 10), want);
    assert_eq!(alpha_row(&p, 11), vec![0; 60]);
}

#[test]
fn axis_aligned_path_clip_is_a_whole_pixel_scissor() {
    let mut c = Canvas::new(40, 40).unwrap();
    c.push_clip_rect(Rect::new(0.0, 0.0, 15.5, 15.5), &ID);
    // Collinear vertices and all: the path scan converts to two vertical
    // edges, which MuPDF turns into a scissor of whole pixels (9..30), not
    // a mask. The analytic rectangle clip keeps its half pixel at 15.5.
    let square = polyline(
        &[
            (9.5, 9.5),
            (20.0, 9.5),
            (30.0, 9.5),
            (30.0, 30.0),
            (9.5, 30.0),
        ],
        true,
    );
    c.push_clip_path(&square, &ID, FillRule::NonZero);
    c.fill_rect(Rect::new(0.0, 0.0, 40.0, 40.0), &ID, &black());
    let p = c.finish();
    let mut middle = vec![0u8; 40];
    middle[9..15].fill(255);
    middle[15] = 128;
    let mut edge = vec![0u8; 40];
    edge[9..15].fill(128);
    edge[15] = 63; // solid-span alpha: (255*64)>>8
    for y in 0..40 {
        let want = match y {
            9..=14 => middle.clone(),
            15 => edge.clone(),
            _ => vec![0; 40],
        };
        assert_eq!(alpha_row(&p, y), want, "row {y}");
    }
}

#[test]
fn fill_scissor_rounds_out_from_the_subsample_grid_only_for_rectangles() {
    let mut c = Canvas::new(10, 10).unwrap();
    // x1 = 6.0 is column 102 = 6 · 17 exactly: the scissor ends at pixel 6,
    // it does not round out to 7.
    let rect = Path::from_rect(Rect::new(2.5, 3.0, 6.0, 5.5));
    assert_eq!(c.fill_scissor(&rect, &ID), Some(IntRect::new(2, 3, 6, 6)));
    let rotated = Transform::rotate(0.3);
    assert_eq!(c.fill_scissor(&rect, &rotated), None);
}

#[test]
fn non_rectangular_clip_masks_with_the_same_subsample_counts_as_a_fill() {
    let tri = polyline(&[(0.0, 0.0), (8.0, 8.0), (0.0, 8.0)], true);
    let mut c = Canvas::new(8, 8).unwrap();
    c.push_clip_path(&tri, &ID, FillRule::NonZero);
    c.fill_rect(Rect::new(0.0, 0.0, 8.0, 8.0), &ID, &black());
    assert_eq!(alpha_rows(&c.finish()), diagonal_triangle(8));
}

#[test]
fn bilinear_upscale_of_two_by_two_image_is_mirror_symmetric() {
    // Black left column, white right column, drawn 10x into 20x20.
    let data = [0u8, 255, 0, 255];
    let image = Image::new(2, 2, ImageFormat::Gray8, &data).unwrap();
    let mut c = Canvas::new(20, 20).unwrap();
    let place = Transform::new(20.0, 0.0, 0.0, -20.0, 0.0, 20.0);
    let options = ImageOptions {
        filter: Filter::Bilinear,
        ..ImageOptions::default()
    };
    c.draw_image(&image, &place, &options);
    let p = c.finish();
    let want = [
        0, 0, 0, 0, 0, 13, 38, 64, 89, 115, 140, 166, 191, 217, 242, 255, 255, 255, 255, 255,
    ];
    for y in 0..20 {
        let red: Vec<u8> = (0..20).map(|x| p.pixel(x, y).unwrap()[0]).collect();
        assert_eq!(red, want, "row {y}");
        for x in 0..20 {
            assert_eq!(red[x] as u16 + red[19 - x] as u16, 255, "mirror of x = {x}");
        }
    }
}

#[test]
fn image_sample_row_zero_lands_at_the_top_of_the_unit_square() {
    // Row 0 white, row 1 black.
    let data = [255u8, 0];
    let image = Image::new(1, 2, ImageFormat::Gray8, &data).unwrap();
    let mut c = Canvas::new(1, 2).unwrap();
    let place = Transform::new(1.0, 0.0, 0.0, -2.0, 0.0, 2.0);
    let options = ImageOptions {
        filter: Filter::Nearest,
        ..ImageOptions::default()
    };
    c.draw_image(&image, &place, &options);
    let p = c.finish();
    assert_eq!(p.pixel(0, 0), Some([255, 255, 255, 255]));
    assert_eq!(p.pixel(0, 1), Some([0, 0, 0, 255]));
}

#[test]
fn multiply_blend_gives_the_product_of_backdrop_and_source() {
    let mut c = Canvas::new(2, 2).unwrap();
    c.clear(Color::rgb(0.4, 0.8, 1.0)); // bytes 102, 204, 255
    let mut paint = Paint::solid(Color::rgb(1.0, 0.5, 0.2)); // bytes 255, 127, 51
    paint.composite.blend_mode = BlendMode::Multiply;
    c.fill_rect(Rect::new(0.0, 0.0, 2.0, 2.0), &ID, &paint);
    // round(cb * cs * 255): 102 * 255 / 255, 204 * 127 / 255, 255 * 51 / 255.
    assert_eq!(c.finish().pixel(1, 1), Some([102, 102, 51, 255]));
}

#[test]
fn group_popped_with_half_alpha_mixes_evenly_with_the_backdrop() {
    let mut c = Canvas::new(4, 4).unwrap();
    c.clear(Color::WHITE);
    c.push_group().unwrap();
    c.fill_rect(
        Rect::new(0.0, 0.0, 4.0, 4.0),
        &ID,
        &Paint::solid(Color::rgb(1.0, 0.0, 0.0)),
    );
    c.pop_group(&Composite {
        alpha: 0.5,
        ..Composite::default()
    });
    // MuPDF truncates alpha to 127: source (255*127)>>8 = 126;
    // white retains (255*E(255-126))>>8 = 129. The page stays opaque.
    assert_eq!(c.finish().pixel(2, 2), Some([255, 129, 129, 255]));
}

#[test]
fn zero_width_stroke_is_a_fifth_of_a_pixel_hairline_under_any_scale() {
    let mut c = Canvas::new(60, 30).unwrap();
    let line = polyline(&[(1.0, 1.055), (5.0, 1.055)], false);
    let scale = Transform::scale(10.0, 10.0);
    c.stroke_path(
        &line,
        &scale,
        &Stroke {
            width: 0.0,
            ..Stroke::default()
        },
        &black(),
    );
    let p = c.finish();
    // MuPDF widens anything thinner than 0.2 device pixels to 0.2: the
    // band 10.45..10.65 spans sub-scanlines ⌊156.75⌋..⌊159.75⌋, three of
    // the fifteen in row 10, so coverage 3*17=51, alpha (255*51)>>8=50.
    let mut covered = vec![0u8; 60];
    covered[10..50].fill(50);
    assert_eq!(alpha_row(&p, 9), vec![0; 60]);
    assert_eq!(alpha_row(&p, 10), covered);
    assert_eq!(alpha_row(&p, 11), vec![0; 60]);
}

#[test]
fn zero_length_dashes_draw_round_dots_and_nothing_with_butt_caps() {
    let line = polyline(&[(5.0, 10.0), (25.0, 10.0)], false);
    let draw = |cap| {
        let mut c = Canvas::new(30, 20).unwrap();
        c.stroke_path(
            &line,
            &ID,
            &Stroke {
                width: 4.0,
                cap,
                dash: Some(Dash {
                    array: vec![0.0, 8.0],
                    phase: 0.0,
                }),
                ..Stroke::default()
            },
            &black(),
        );
        c.finish()
    };
    // Dots of radius 2 at x = 5, 13, 21; pixel 9 is two units from both.
    let round = draw(LineCap::Round);
    assert_eq!(alpha_at(&round, 5, 10), 255);
    assert_eq!(alpha_at(&round, 4, 10), 255);
    assert_eq!(alpha_at(&round, 9, 10), 0);
    assert_eq!(alpha_at(&round, 13, 10), 255);
    let butt = draw(LineCap::Butt);
    assert!(
        butt.data().iter().all(|&v| v == 0),
        "butt caps draw nothing"
    );
}

#[test]
fn zero_length_subpath_with_round_cap_draws_a_dot() {
    let dot = polyline(&[(20.0, 20.0), (20.0, 20.0)], false);
    let draw = |cap| {
        let mut c = Canvas::new(40, 40).unwrap();
        c.stroke_path(
            &dot,
            &ID,
            &Stroke {
                width: 10.0,
                cap,
                ..Stroke::default()
            },
            &black(),
        );
        c.finish()
    };
    let round = draw(LineCap::Round);
    assert_eq!(alpha_at(&round, 20, 20), 255);
    assert_eq!(alpha_at(&round, 16, 20), 255);
    assert_eq!(alpha_at(&round, 26, 20), 0);
    let butt = draw(LineCap::Butt);
    assert!(
        butt.data().iter().all(|&v| v == 0),
        "butt cap draws nothing"
    );
}

#[test]
fn stroke_join_counts_the_inner_corner_once() {
    // Width 4 around (10.5, 30.5) -> (10.5, 10.5) -> (30.5, 10.5): the inner
    // corner is (12.5, 12.5). In pixel (12, 12) the vertical arm covers
    // 8 of 17 columns (⌊12.5 · 17⌋ = 212, from 204) and the horizontal arm
    // 7 of 15 rows (⌊12.5 · 15⌋ = 187, from 180): their union is
    // 8 · 15 + 7 · 17 − 8 · 7 = 183 subsamples, not the sum.
    // MuPDF solid-span alpha preserves 183, but counts 120/119 paint 119/118.
    let corner = polyline(&[(10.5, 30.5), (10.5, 10.5), (30.5, 10.5)], false);
    let mut c = Canvas::new(40, 40).unwrap();
    c.stroke_path(
        &corner,
        &ID,
        &Stroke {
            width: 4.0,
            ..Stroke::default()
        },
        &black(),
    );
    let p = c.finish();
    assert_eq!(alpha_at(&p, 12, 12), 183);
    assert_eq!(alpha_at(&p, 12, 20), 119);
    assert_eq!(alpha_at(&p, 20, 12), 118);
}

#[test]
fn absurd_coordinates_are_clipped_without_losing_axis_aligned_edges() {
    let p = filled(8, 8, Rect::new(2.0, -1e30, 1e300, 5.0));
    for y in 0..8 {
        let want = if y < 5 {
            vec![0, 0, 255, 255, 255, 255, 255, 255]
        } else {
            vec![0; 8]
        };
        assert_eq!(alpha_row(&p, y), want, "row {y}");
    }
}

#[test]
fn straight_alpha_export_divides_out_alpha_with_rounding() {
    let premul = vec![64, 32, 0, 128, 0, 0, 0, 0, 10, 20, 30, 255];
    let p = Pixmap::from_premultiplied(3, 1, premul).unwrap();
    assert_eq!(
        p.to_rgba8(),
        vec![128, 64, 0, 128, 0, 0, 0, 0, 10, 20, 30, 255]
    );
}
