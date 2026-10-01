//! Guard: filling many small shapes reuses the canvas scratch buffers, so a
//! warm canvas allocates nothing per fill. The scan converter's edge list,
//! crossing buckets, delta row, coverage mask and row extents are all kept
//! between fills; a per-shape allocation here is the regression that made a
//! dense vector page twice as slow.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

use pdf_raster::{Canvas, Color, FillRule, Paint, PathBuilder, Transform};

struct Counting;

static ALLOCATIONS: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

/// 2000 small curved shapes scattered over the page, like a dense chart.
fn shapes() -> Vec<pdf_raster::Path> {
    let mut seed = 0x2545_f491_4f6c_dd1du64;
    let mut next = move || {
        seed = seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        ((seed >> 33) % 760) as f64 + 20.0
    };
    (0..2000)
        .map(|i| {
            let (x, y) = (next(), next());
            let r = 4.0 + (i % 7) as f64;
            let mut b = PathBuilder::new();
            b.move_to(x - r, y);
            b.cubic_to(x - r, y - r, x + r, y - r, x + r, y);
            b.line_to(x + r * 0.5, y + r);
            b.quad_to(x, y + r * 1.5, x - r * 0.5, y + r);
            b.close();
            b.finish()
        })
        .collect()
}

fn fill_all(canvas: &mut Canvas, shapes: &[pdf_raster::Path]) {
    let paint = Paint::solid(Color::rgb(0.1, 0.2, 0.3));
    for path in shapes {
        canvas.fill_path(path, &Transform::IDENTITY, FillRule::NonZero, &paint);
    }
}

#[test]
fn warm_canvas_fills_without_allocating() {
    let shapes = shapes();
    let mut canvas = Canvas::new(800, 800).expect("canvas");
    fill_all(&mut canvas, &shapes);
    let before = ALLOCATIONS.load(Ordering::Relaxed);
    fill_all(&mut canvas, &shapes);
    let during = ALLOCATIONS.load(Ordering::Relaxed) - before;
    assert_eq!(
        during, 0,
        "{during} allocations while filling 2000 shapes on a warm canvas"
    );
}
