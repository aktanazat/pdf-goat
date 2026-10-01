//! Transparency-group contracts. Byte expectations use MuPDF's draw-paint.c
//! and draw-blend.c integer equations (AGPL), checked against PyMuPDF 1.27.2.3.
//! E(a)=a+(a>>7), C(a,b)=(a*b)>>8, M(a,b)=round(a*b/255).
//! Opaque page destinations retain alpha 255 rather than MuPDF RGBA's 254.

use pdf_raster::{
    BlendMode, Canvas, Color, Composite, Filter, Image, ImageFormat, ImageOptions, IntRect,
    MAX_GROUP_DEPTH, Mask, MaskImage, Paint, Pixmap, RasterError, Rect, Transform,
};

const ID: Transform = Transform::IDENTITY;
const RED: Color = Color::rgb(1.0, 0.0, 0.0);
const BLUE: Color = Color::rgb(0.0, 0.0, 1.0);

fn fill(canvas: &mut Canvas, x0: f64, x1: f64, paint: &Paint<'_>) {
    canvas.fill_rect(Rect::new(x0, 0.0, x1, 1.0), &ID, paint);
}

fn pixels(pixmap: &Pixmap) -> Vec<[u8; 4]> {
    pixmap.data().as_chunks::<4>().0.to_vec()
}

#[test]
fn nonisolated_blends_see_the_backdrop_but_isolated_blends_do_not() {
    let draw = |isolated| {
        let mut canvas = Canvas::new(1, 1).unwrap();
        canvas.clear(Color::rgb(0.8, 0.4, 0.2));
        canvas.begin_group(isolated, false).unwrap();
        let mut paint = Paint::solid(Color::rgb(0.5, 0.75, 1.0));
        paint.composite.blend_mode = BlendMode::Multiply;
        fill(&mut canvas, 0.0, 1.0, &paint);
        canvas.pop_group(&Composite::default());
        canvas.finish()
    };
    // Byte truncation gives [204,102,51] and [127,191,255].
    // Multiply rounds their products /255 to [102,76,51].
    assert_eq!(pixels(&draw(false)), [[102, 76, 51, 255]]);
    // Multiply against transparency leaves the truncated source unchanged.
    assert_eq!(pixels(&draw(true)), [[127, 191, 255, 255]]);
}

#[test]
fn inherited_translucent_backdrop_is_removed_before_group_opacity() {
    let mut canvas = Canvas::new(2, 1).unwrap();
    canvas.clear(Color::new(1.0, 0.0, 0.0, 0.5));
    canvas.begin_group(false, false).unwrap();
    fill(
        &mut canvas,
        0.0,
        1.0,
        &Paint::solid(Color::new(0.0, 0.0, 1.0, 0.5)),
    );
    canvas.pop_group(&Composite {
        alpha: 0.5,
        ..Composite::default()
    });
    // MuPDF's non-isolated uncompositing: contribution alpha is
    // C(255,E(127))=126; M(126,127)=63 at the outer opacity.
    // The red backdrop retains 96; resulting alpha is 127-31+63=159.
    assert_eq!(
        pixels(&canvas.finish()),
        [[96, 0, 63, 159], [127, 0, 0, 127]]
    );
}

#[test]
fn empty_nonisolated_group_cannot_repaint_its_backdrop() {
    let mut canvas = Canvas::new(1, 1).unwrap();
    canvas.clear(Color::new(1.0, 0.0, 0.0, 0.5));
    canvas.begin_group(false, true).unwrap();
    canvas.pop_group(&Composite {
        alpha: 0.5,
        blend_mode: BlendMode::Multiply,
        ..Composite::default()
    });
    // clear's premultiplied byte is trunc(255*0.5)=127; no drawing may alter it.
    assert_eq!(pixels(&canvas.finish()), [[127, 0, 0, 127]]);
}

#[test]
fn knockout_removes_siblings_by_shape_including_partial_edge_coverage() {
    let draw = |knockout| {
        let mut canvas = Canvas::new(3, 1).unwrap();
        canvas.clear(Color::WHITE);
        canvas.begin_group(true, knockout).unwrap();
        fill(&mut canvas, 0.0, 3.0, &Paint::solid(RED));
        let mut paint = Paint::solid(BLUE);
        paint.composite.alpha = 0.5;
        fill(&mut canvas, 0.0, 1.5, &paint);
        canvas.pop_group(&Composite::default());
        canvas.finish()
    };
    // MuPDF draw-device oracle: half-alpha blue paints [0,0,126,126].
    // At x=1.5, coverage 120 paints blue/alpha 58, shape 119.
    // fz_blend_knockout gives [87,0,76,163]; popping adds 91 white.
    // Without knockout the corresponding group pixels are [128,0,126,255]
    // and [196,0,58,255]. This guards colour as well as shape and opacity.
    assert_eq!(
        pixels(&draw(true)),
        [[129, 129, 255, 255], [178, 91, 167, 255], [255, 0, 0, 255]]
    );
    assert_eq!(
        pixels(&draw(false)),
        [[128, 0, 126, 255], [196, 0, 58, 255], [255, 0, 0, 255]]
    );
}

#[test]
fn nonisolated_child_in_knockout_uses_initial_not_previous_sibling_backdrop() {
    let mut canvas = Canvas::new(1, 1).unwrap();
    canvas.clear(Color::rgb(0.0, 1.0, 0.0));
    canvas.begin_group(false, true).unwrap();
    fill(&mut canvas, 0.0, 1.0, &Paint::solid(RED));
    canvas.begin_group(false, false).unwrap();
    let mut paint = Paint::solid(BLUE);
    paint.composite.alpha = 0.5;
    paint.composite.blend_mode = BlendMode::Multiply;
    fill(&mut canvas, 0.0, 1.0, &paint);
    canvas.pop_group(&Composite::default());
    canvas.pop_group(&Composite::default());
    // Blue Multiply sees initial green, not sibling red. Truncated alpha
    // leaves green 128; non-isolated uncompositing/recompositing gives 129.
    assert_eq!(pixels(&canvas.finish()), [[0, 129, 0, 255]]);
}

#[test]
fn zero_opacity_still_knocks_out_unless_alpha_is_shape() {
    let draw = |alpha_is_shape| {
        let mut canvas = Canvas::new(1, 1).unwrap();
        canvas.clear(Color::WHITE);
        canvas.begin_group(true, true).unwrap();
        fill(&mut canvas, 0.0, 1.0, &Paint::solid(RED));
        let mut paint = Paint::solid(BLUE);
        paint.composite = Composite {
            alpha: 0.0,
            alpha_is_shape,
            ..Composite::default()
        };
        fill(&mut canvas, 0.0, 1.0, &paint);
        canvas.pop_group(&Composite::default());
        canvas.finish()
    };
    assert_eq!(pixels(&draw(false)), [[255, 255, 255, 255]]);
    assert_eq!(pixels(&draw(true)), [[255, 0, 0, 255]]);
}

#[test]
fn soft_mask_changes_knockout_shape_only_when_ais_is_true() {
    let mask = Mask::from_data(IntRect::from_size(2, 1), vec![128, 0], 0).unwrap();
    let draw = |alpha_is_shape| {
        let mut canvas = Canvas::new(2, 1).unwrap();
        canvas.clear(Color::WHITE);
        canvas.begin_group(true, true).unwrap();
        fill(&mut canvas, 0.0, 2.0, &Paint::solid(RED));
        let mut paint = Paint::solid(BLUE);
        paint.composite = Composite {
            soft_mask: Some(&mask),
            alpha_is_shape,
            ..Composite::default()
        };
        fill(&mut canvas, 0.0, 2.0, &paint);
        canvas.pop_group(&Composite::default());
        canvas.finish()
    };
    // Mask byte 128 paints blue/alpha 128. With full shape, popping adds
    // C(255,256-E(128))=126. With AIS, fz_blend_knockout gives
    // [95,0,96,191], and popping adds C(255,64)=63 white.
    assert_eq!(
        pixels(&draw(false)),
        [[126, 126, 254, 255], [255, 255, 255, 255]]
    );
    assert_eq!(pixels(&draw(true)), [[158, 63, 159, 255], [255, 0, 0, 255]]);
}

#[test]
fn nested_group_shape_keeps_holes_and_transparent_painted_regions_distinct() {
    let mut canvas = Canvas::new(3, 1).unwrap();
    canvas.clear(Color::WHITE);
    canvas.begin_group(true, true).unwrap();
    fill(&mut canvas, 0.0, 3.0, &Paint::solid(RED));
    canvas.push_group().unwrap();
    canvas.push_group().unwrap();
    fill(
        &mut canvas,
        0.0,
        1.0,
        &Paint::solid(Color::new(0.0, 0.0, 1.0, 0.5)),
    );
    fill(&mut canvas, 2.0, 3.0, &Paint::solid(Color::TRANSPARENT));
    canvas.pop_group(&Composite::default());
    canvas.pop_group(&Composite::default());
    canvas.pop_group(&Composite::default());
    // The unpainted middle is not part of either nested group's shape.
    // The transparent but painted right-hand pixel is, so it removes red.
    // Half-alpha blue is C(255,127)=126; nested transparent backdrops
    // preserve it, and the final pop adds C(255,130)=129 white.
    assert_eq!(
        pixels(&canvas.finish()),
        [[129, 129, 255, 255], [255, 0, 0, 255], [255, 255, 255, 255]]
    );
}

#[test]
fn group_ais_does_not_convert_internal_opacity_into_external_shape() {
    let draw = |alpha_is_shape| {
        let mut canvas = Canvas::new(1, 1).unwrap();
        canvas.clear(Color::WHITE);
        canvas.begin_group(true, true).unwrap();
        fill(&mut canvas, 0.0, 1.0, &Paint::solid(RED));
        canvas.push_group().unwrap();
        fill(
            &mut canvas,
            0.0,
            1.0,
            &Paint::solid(Color::new(0.0, 0.0, 1.0, 0.5)),
        );
        canvas.pop_group(&Composite {
            alpha: 0.5,
            alpha_is_shape,
            ..Composite::default()
        });
        canvas.pop_group(&Composite::default());
        canvas.finish()
    };
    // Inner blue/alpha is 126; the outer constant alpha gives C(126,127)=62.
    // AIS=false replaces fully, then white contributes C(255,194)=193.
    // AIS=true uses shape 127: knockout gives [80,0,79,159], then +95 white.
    assert_eq!(pixels(&draw(false)), [[193, 193, 255, 255]]);
    assert_eq!(pixels(&draw(true)), [[175, 95, 174, 255]]);
}

#[test]
fn inherited_fractional_clip_is_applied_once_after_nested_groups() {
    let mut canvas = Canvas::new(2, 1).unwrap();
    canvas.push_clip_rect(Rect::new(0.5, 0.0, 2.0, 1.0), &ID);
    let depth = canvas.clip_depth();
    canvas.begin_group(false, false).unwrap();
    canvas.push_group().unwrap();
    assert_eq!(canvas.clip_depth(), depth);
    fill(&mut canvas, 0.0, 2.0, &Paint::solid(RED));
    fill(&mut canvas, 0.0, 2.0, &Paint::solid(BLUE));
    canvas.push_clip_rect(Rect::new(1.0, 0.0, 2.0, 1.0), &ID);
    canvas.pop_group(&Composite::default());
    assert_eq!(canvas.clip_depth(), depth);
    canvas.pop_group(&Composite::default());
    assert_eq!(canvas.clip_depth(), depth);
    assert_eq!(
        pixels(&canvas.finish()),
        [[0, 0, 128, 128], [0, 0, 255, 255]]
    );
}

#[test]
fn group_blend_and_soft_mask_apply_after_backdrop_removal() {
    let mut canvas = Canvas::new(1, 1).unwrap();
    canvas.clear(Color::rgb(0.4, 0.8, 1.0));
    canvas.begin_group(false, false).unwrap();
    fill(
        &mut canvas,
        0.0,
        1.0,
        &Paint::solid(Color::new(1.0, 0.0, 0.0, 0.5)),
    );
    let mask = Mask::new(IntRect::from_size(1, 1), 128, 0).unwrap();
    canvas.pop_group(&Composite {
        alpha: 0.5,
        blend_mode: BlendMode::Multiply,
        soft_mask: Some(&mask),
        alpha_is_shape: false,
    });
    // Effective source alpha = M(126,M(127,128))=32.
    // Multiply red preserves backdrop red; green/blue retain 223/255.
    assert_eq!(pixels(&canvas.finish()), [[102, 178, 223, 255]]);
}

#[test]
fn stencil_holes_are_shape_but_image_alpha_is_opacity_unless_ais() {
    let draw = |image: bool, alpha_is_shape| {
        let mut canvas = Canvas::new(2, 1).unwrap();
        canvas.clear(Color::WHITE);
        canvas.begin_group(true, true).unwrap();
        fill(&mut canvas, 0.0, 2.0, &Paint::solid(RED));
        let place = Transform::scale(2.0, 1.0);
        if image {
            let data = [0, 0, 255, 128, 0, 0, 255, 0];
            let image = Image::new(2, 1, ImageFormat::Rgba8, &data).unwrap();
            canvas.draw_image(
                &image,
                &place,
                &ImageOptions {
                    filter: Filter::Nearest,
                    composite: Composite {
                        alpha_is_shape,
                        ..Composite::default()
                    },
                    ..ImageOptions::default()
                },
            );
        } else {
            let data = [255, 0];
            let stencil = MaskImage::new(2, 1, &data).unwrap();
            let mut paint = Paint::solid(BLUE);
            paint.composite.alpha = 0.5;
            canvas.draw_stencil(&stencil, &place, Filter::Nearest, &paint);
        }
        canvas.pop_group(&Composite::default());
        canvas.finish()
    };
    // Stencil half opacity truncates to 127, painting alpha 126 and adding
    // 129 white on pop. Image alpha is already a byte (128), adding 126.
    // Image AIS uses shape 128: knockout [95,0,96,191] then +63 white.
    assert_eq!(
        pixels(&draw(false, false)),
        [[129, 129, 255, 255], [255, 0, 0, 255]]
    );
    assert_eq!(
        pixels(&draw(true, false)),
        [[126, 126, 254, 255], [255, 255, 255, 255]]
    );
    assert_eq!(
        pixels(&draw(true, true)),
        [[158, 63, 159, 255], [255, 0, 0, 255]]
    );
}

#[test]
fn failed_begin_preserves_pixels_clips_depth_and_next_drawing_target() {
    let mut canvas = Canvas::new(2, 1).unwrap();
    canvas.clear(Color::WHITE);
    canvas.push_clip_rect(Rect::new(0.5, 0.0, 2.0, 1.0), &ID);
    for _ in 0..MAX_GROUP_DEPTH {
        canvas.push_group().unwrap();
    }
    fill(&mut canvas, 1.0, 2.0, &Paint::solid(Color::BLACK));
    let before = canvas.pixmap().clone();
    let clip = canvas.clip_bounds();
    let depth = canvas.clip_depth();
    let error = RasterError::GroupDepthLimit {
        limit: MAX_GROUP_DEPTH,
    };
    assert_eq!(canvas.begin_group(false, true), Err(error.clone()));
    assert_eq!(canvas.push_group(), Err(error));
    assert_eq!(canvas.group_depth(), MAX_GROUP_DEPTH);
    assert_eq!(canvas.clip_depth(), depth);
    assert_eq!(canvas.clip_bounds(), clip);
    assert_eq!(canvas.pixmap(), &before);
    // Group contents cannot accidentally pop or restore past the entry clip.
    assert!(!canvas.pop_clip());
    canvas.restore_clip_depth(0);
    assert_eq!(canvas.clip_depth(), depth);
    fill(&mut canvas, 0.0, 1.0, &Paint::solid(RED));
    for _ in 0..MAX_GROUP_DEPTH {
        assert!(canvas.pop_group(&Composite::default()));
    }
    assert!(!canvas.pop_group(&Composite::default()));
    // Freed layer storage permits another real group, rather than leaving the
    // canvas in a sticky failed or flattened state.
    canvas.begin_group(false, true).unwrap();
    canvas.pop_group(&Composite::default());
    // Analytic half clip is byte 128: source C(255,E(128))=128;
    // the destination retains C(255,E(127))=126, giving red 254.
    assert_eq!(
        pixels(&canvas.finish()),
        [[254, 126, 126, 255], [0, 0, 0, 255]]
    );
}

#[test]
fn offscreen_groups_keep_clip_and_soft_mask_coordinates() {
    let full = IntRect::new(-20, -20, 20, 20);
    let crop = IntRect::new(-5, -7, 7, 8);
    let mask = Mask::new(IntRect::new(-3, -4, 5, 6), 160, 0).unwrap();
    for (isolated, knockout) in [(false, false), (true, false), (false, true), (true, true)] {
        let draw = |bounds| {
            let mut canvas = Canvas::new_at(bounds).unwrap();
            canvas.clear(Color::WHITE);
            canvas.push_clip_rect(Rect::new(-4.5, -6.2, 5.75, 6.5), &ID);
            canvas.begin_group(isolated, knockout).unwrap();
            canvas.fill_rect(Rect::new(-10.0, -10.0, 10.0, 10.0), &ID, &Paint::solid(RED));
            let mut blue = Paint::solid(BLUE);
            blue.composite.alpha = 0.5;
            canvas.fill_rect(Rect::new(-2.5, -3.5, 4.75, 5.75), &ID, &blue);
            canvas.pop_group(&Composite {
                soft_mask: Some(&mask),
                ..Composite::default()
            });
            canvas.finish()
        };
        let page = draw(full);
        let cell = draw(crop);
        for y in crop.y0..crop.y1 {
            for x in crop.x0..crop.x1 {
                assert_eq!(
                    cell.pixel((x - crop.x0) as u32, (y - crop.y0) as u32),
                    page.pixel((x - full.x0) as u32, (y - full.y0) as u32),
                    "isolated={isolated} knockout={knockout} at ({x},{y})",
                );
            }
        }
    }
}
