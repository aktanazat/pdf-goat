use pdf_raster::{Canvas, Color, IntRect, Mask, Point, Rect, Transform};

const RED: [[f32; 3]; 3] = [[1.0, 0.0, 0.0]; 3];

#[test]
fn adjoining_triangles_cover_the_square_without_translucent_seams() {
    let mut canvas = Canvas::new(40, 40).unwrap();
    canvas.clear(Color::WHITE);
    canvas.fill_mesh_triangle(
        [
            Point::new(0.0, 40.0),
            Point::new(40.0, 40.0),
            Point::new(0.0, 0.0),
        ],
        RED,
    );
    canvas.fill_mesh_triangle(
        [
            Point::new(40.0, 40.0),
            Point::new(40.0, 0.0),
            Point::new(0.0, 0.0),
        ],
        RED,
    );
    for pixel in canvas.finish().data().as_chunks::<4>().0 {
        assert_eq!(*pixel, [255, 0, 0, 255]);
    }
}

#[test]
fn subpixel_shared_edges_have_one_owner_in_either_winding_and_order() {
    let a = Point::new(0.25, 0.25);
    let b = Point::new(5.75, 0.25);
    let c = Point::new(5.75, 4.75);
    let d = Point::new(0.25, 4.75);
    let mask = Mask::new(IntRect::from_size(7, 6), 128, 0).unwrap();
    let draw = |reverse: bool| {
        let mut canvas = Canvas::new(7, 6).unwrap();
        canvas.push_clip_mask(&mask);
        let triangles = if reverse {
            [[d, c, b], [d, b, a]]
        } else {
            [[a, b, d], [b, c, d]]
        };
        for points in triangles {
            canvas.fill_mesh_triangle(points, RED);
        }
        canvas.finish()
    };
    let forward = draw(false);
    assert_eq!(forward, draw(true));
    for y in 0..6 {
        for x in 0..7 {
            // Integer samples inside [0.25,5.75) x [0.25,4.75).
            // The half clip exposes double hits as alpha 192, holes as 0.
            let want = if (1..=5).contains(&x) && (1..=4).contains(&y) {
                [128, 0, 0, 128]
            } else {
                [0; 4]
            };
            assert_eq!(forward.pixel(x, y), Some(want), "sample ({x},{y})");
        }
    }
}

#[test]
fn subpixel_triangle_fan_owns_its_shared_vertex_once() {
    let mut canvas = Canvas::new(3, 3).unwrap();
    let mask = Mask::new(IntRect::from_size(3, 3), 128, 0).unwrap();
    canvas.push_clip_mask(&mask);
    let corners = [
        Point::new(0.25, 0.25),
        Point::new(1.75, 0.25),
        Point::new(1.75, 1.75),
        Point::new(0.25, 1.75),
    ];
    for i in 0..4 {
        canvas.fill_mesh_triangle(
            [corners[i], corners[(i + 1) % 4], Point::new(1.0, 1.0)],
            RED,
        );
    }
    let image = canvas.finish();
    for y in 0..3 {
        for x in 0..3 {
            let want = if x == 1 && y == 1 {
                [128, 0, 0, 128]
            } else {
                [0; 4]
            };
            assert_eq!(image.pixel(x, y), Some(want));
        }
    }
}

#[test]
fn gouraud_uses_integer_samples_floor_quantization_and_excluded_diagonal() {
    let points = [
        Point::new(0.0, 64.0),
        Point::new(64.0, 64.0),
        Point::new(0.0, 0.0),
    ];
    let colors = [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]];
    let draw = |reverse| {
        let mut canvas = Canvas::new(64, 64).unwrap();
        canvas.clear(Color::WHITE);
        if reverse {
            canvas.fill_mesh_triangle(
                [points[2], points[1], points[0]],
                [colors[2], colors[1], colors[0]],
            );
        } else {
            canvas.fill_mesh_triangle(points, colors);
        }
        canvas.finish()
    };
    let image = draw(false);
    assert_eq!(image, draw(true));
    // Weights at (5,13): red 8/64, green 5/64, blue 51/64.
    assert_eq!(image.pixel(5, 13), Some([31, 19, 203, 255]));
    assert_eq!(image.pixel(5, 5), Some([255, 255, 255, 255]));
    assert_eq!(image.pixel(0, 63), Some([251, 0, 3, 255]));
}

#[test]
fn mesh_respects_fractional_clip_coverage() {
    let mut canvas = Canvas::new(5, 5).unwrap();
    canvas.push_clip_rect(Rect::new(1.5, 1.5, 3.5, 3.5), &Transform::IDENTITY);
    canvas.fill_mesh_triangle(
        [
            Point::new(-8.0, -8.0),
            Point::new(16.0, -8.0),
            Point::new(-8.0, 16.0),
        ],
        RED,
    );
    let image = canvas.finish();
    for y in 0..5 {
        for x in 0..5 {
            let alpha = match (x, y) {
                (2, 2) => 255,
                (1 | 3, 1 | 3) => 64,
                (1..=3, 1..=3) => 128,
                _ => 0,
            };
            assert_eq!(image.pixel(x, y), Some([alpha, 0, 0, alpha]));
        }
    }
}
