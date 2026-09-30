//! Transparency-group contracts. Expected bytes follow premultiplied source-over
//! and the PDF shape/opacity equations, not another raster implementation.

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
    // Non-isolated: round([204,102,51] * [128,191,255] / 255).
    assert_eq!(pixels(&draw(false)), [[102, 76, 51, 255]]);
    // Isolated: Multiply against transparency leaves the source unchanged.
    assert_eq!(pixels(&draw(true)), [[128, 191, 255, 255]]);
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
    // The source is blue at 128/255 * 128/255 = 64/255, not the inherited
    // red+blue result. Red retains 128 * (1 - 64/255) = 96; alpha is 160.
    assert_eq!(
        pixels(&canvas.finish()),
        [[96, 0, 64, 160], [128, 0, 0, 128]]
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
    assert_eq!(pixels(&canvas.finish()), [[128, 0, 0, 128]]);
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
    // Full shape removes red even at half opacity. At the half-covered edge,
    // red retains 127 and blue contributes 64; group alpha is 191, not 255.
    assert_eq!(
        pixels(&draw(true)),
        [[127, 127, 255, 255], [191, 64, 128, 255], [255, 0, 0, 255]]
    );
    assert_eq!(
        pixels(&draw(false)),
        [[127, 0, 128, 255], [191, 0, 64, 255], [255, 0, 0, 255]]
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
    // Blue Multiply sees the initial green, not the sibling red. Its half
    // opacity leaves half green after it knocks out the red sibling.
    assert_eq!(pixels(&canvas.finish()), [[0, 127, 0, 255]]);
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
    assert_eq!(
        pixels(&draw(false)),
        [[127, 127, 255, 255], [255, 255, 255, 255]]
    );
    assert_eq!(pixels(&draw(true)), [[127, 0, 128, 255], [255, 0, 0, 255]]);
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
    assert_eq!(
        pixels(&canvas.finish()),
        [[127, 127, 255, 255], [255, 0, 0, 255], [255, 255, 255, 255]]
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
    // Inner opacity is 128; outer constant alpha gives source alpha 64.
    // AIS=false knocks out fully. AIS=true retains 127 of the old red, not 191.
    assert_eq!(pixels(&draw(false)), [[191, 191, 255, 255]]);
    assert_eq!(pixels(&draw(true)), [[191, 64, 128, 255]]);
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
    // Effective source alpha = round(128 * round(128*128/255) / 255) = 32.
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
    assert_eq!(
        pixels(&draw(false, false)),
        [[127, 127, 255, 255], [255, 0, 0, 255]]
    );
    assert_eq!(
        pixels(&draw(true, false)),
        [[127, 127, 255, 255], [255, 255, 255, 255]]
    );
    assert_eq!(
        pixels(&draw(true, true)),
        [[127, 0, 128, 255], [255, 0, 0, 255]]
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
    assert_eq!(
        pixels(&canvas.finish()),
        [[255, 127, 127, 255], [0, 0, 0, 255]]
    );
}
