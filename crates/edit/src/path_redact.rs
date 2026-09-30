use pdf_core::{Matrix, Object, Operation, Point, Rect};

use crate::redact::covered;

/// MuPDF's covered-line-art rule applies to each moveto/rectangle segment,
/// with conservative control-point bounds and stroke/miter expansion.
pub(crate) fn filter(
    path: Vec<Operation>,
    ctm: Matrix,
    stroke: Option<(f64, i64, f64)>,
    rects: &[Rect],
) -> Vec<Operation> {
    let mut segments: Vec<Vec<Operation>> = Vec::new();
    let mut clip = None;
    for operation in path {
        if matches!(operation.operator.as_slice(), b"W" | b"W*") {
            clip = Some(operation);
        } else if matches!(operation.operator.as_slice(), b"m" | b"re") {
            if segments
                .last()
                .is_some_and(|s| s.len() == 1 && s[0].operator == b"m")
            {
                segments.pop();
            }
            segments.push(vec![operation]);
        } else if let Some(segment) = segments.last_mut() {
            segment.push(operation);
        }
    }
    let mut kept = Vec::new();
    for segment in segments {
        if !bounds(&segment, ctm, stroke).is_some_and(|area| covered(area, rects)) {
            kept.extend(segment);
        }
    }
    if !kept.is_empty()
        && let Some(clip) = clip
    {
        kept.push(clip);
    }
    kept
}

fn bounds(path: &[Operation], ctm: Matrix, stroke: Option<(f64, i64, f64)>) -> Option<Rect> {
    let mut bounds: Option<Rect> = None;
    let mut current = Point::default();
    let mut start = current;
    let mut trailing_move = None;
    let mut right_angles = true;
    for operation in path {
        let point = |offset| {
            Point::new(number(operation, offset), number(operation, offset + 1)).transform(&ctm)
        };
        let mut points = [Point::default(); 5];
        let mut count = 0;
        match operation.operator.as_slice() {
            b"m" => {
                current = point(0);
                start = current;
                trailing_move = Some(current);
            }
            b"re" => {
                let x = number(operation, 0);
                let y = number(operation, 1);
                let w = number(operation, 2);
                let h = number(operation, 3);
                let corners = [
                    Point::new(x, y),
                    Point::new(x + w, y),
                    Point::new(x + w, y + h),
                    Point::new(x, y + h),
                ]
                .map(|p| p.transform(&ctm));
                current = corners[0];
                start = current;
                points[..4].copy_from_slice(&corners);
                count = 4;
                right_angles &= corners
                    .windows(2)
                    .all(|pair| axis_aligned(pair[0], pair[1]));
            }
            b"l" => {
                let end = point(0);
                right_angles &= axis_aligned(current, end);
                points[0] = end;
                count = 1;
                current = end;
            }
            b"c" => {
                points[..3].copy_from_slice(&[point(0), point(2), point(4)]);
                count = 3;
                current = point(4);
                right_angles = false;
            }
            b"v" | b"y" => {
                points[..3].copy_from_slice(&[current, point(0), point(2)]);
                count = 3;
                current = point(2);
                right_angles = false;
            }
            b"h" => current = start,
            _ => {}
        }
        if count > 0 {
            if let Some(start) = trailing_move.take() {
                points[count] = start;
                count += 1;
            }
            for p in &points[..count] {
                bounds = Some(match bounds {
                    Some(r) => {
                        Rect::new(r.x0.min(p.x), r.y0.min(p.y), r.x1.max(p.x), r.y1.max(p.y))
                    }
                    None => Rect::new(p.x, p.y, p.x, p.y),
                });
            }
        }
    }
    let mut bounds = bounds?;
    if let Some((width, join, miter)) = stroke {
        let mut expand = if width == 0.0 { 0.5 } else { width / 2.0 };
        if bounds.width() != 0.0
            && bounds.height() != 0.0
            && !right_angles
            && join == 0
            && miter > 0.5
        {
            expand *= miter * 2.0;
        }
        expand *= ctm
            .a
            .abs()
            .max(ctm.b.abs())
            .max(ctm.c.abs())
            .max(ctm.d.abs());
        bounds.x0 -= expand;
        bounds.y0 -= expand;
        bounds.x1 += expand;
        bounds.y1 += expand;
    }
    Some(bounds)
}

fn number(operation: &Operation, index: usize) -> f64 {
    operation
        .operands
        .get(index)
        .and_then(Object::as_f64)
        .unwrap_or(0.0)
}
fn axis_aligned(a: Point, b: Point) -> bool {
    (a.x - b.x).abs() <= 0.001 || (a.y - b.y).abs() <= 0.001
}
