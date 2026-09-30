use pdf_core::{Dict, Matrix, Object, Point, Rect, Stream};
use pdf_interp::PdfImage;

use crate::EditError;

pub(crate) fn redact(
    image: &PdfImage,
    ctm: Matrix,
    rects: &[Rect],
) -> Result<Option<Stream>, EditError> {
    let corners = [
        Point::new(0.0, 0.0),
        Point::new(1.0, 0.0),
        Point::new(0.0, 1.0),
        Point::new(1.0, 1.0),
    ]
    .map(|p| p.transform(&ctm));
    if rects.iter().any(|r| {
        corners
            .iter()
            .all(|p| p.x >= r.x0 && p.x <= r.x1 && p.y >= r.y0 && p.y <= r.y1)
    }) {
        return Ok(None);
    }
    let inverse = ctm
        .invert()
        .ok_or_else(|| EditError::Message("cannot redact a singular image transform".into()))?;
    let intersecting = rects
        .iter()
        .copied()
        .filter(|rect| intersects(ctm, *rect))
        .collect::<Vec<_>>();
    let rects = intersecting.as_slice();
    if image.is_mask() {
        let mut pixels = image.decode_stencil()?;
        blank(
            &mut pixels.data,
            pixels.width,
            pixels.height,
            1,
            0,
            inverse,
            rects,
        )?;
        let stride = (pixels.width as usize).div_ceil(8);
        let mut packed = vec![0xff; stride * pixels.height as usize];
        for (i, coverage) in pixels.data.iter().enumerate() {
            if *coverage >= 128 {
                let x = i % pixels.width as usize;
                let y = i / pixels.width as usize;
                packed[y * stride + x / 8] &= !(0x80 >> (x % 8));
            }
        }
        let mut dict = dictionary(pixels.width, pixels.height);
        dict.insert("BitsPerComponent", 1);
        dict.insert("ImageMask", true);
        if image.interpolate() {
            dict.insert("Interpolate", true);
        }
        return Ok(Some(Stream::new(dict, packed)));
    }
    let mut pixels = image.decode_rgb()?;
    blank(
        &mut pixels.rgb,
        pixels.width,
        pixels.height,
        3,
        255,
        inverse,
        rects,
    )?;
    let mut dict = dictionary(pixels.width, pixels.height);
    dict.insert("ColorSpace", Object::name("DeviceRGB"));
    if image.interpolate() {
        dict.insert("Interpolate", true);
    }
    if let Some(mut alpha) = pixels.alpha {
        blank(
            &mut alpha.data,
            alpha.width,
            alpha.height,
            1,
            255,
            inverse,
            rects,
        )?;
        let mut mask = dictionary(alpha.width, alpha.height);
        mask.insert("ColorSpace", Object::name("DeviceGray"));
        dict.insert("SMask", Stream::new(mask, alpha.data));
    }
    Ok(Some(Stream::new(dict, pixels.rgb)))
}

/// Separating-axis test of a transformed image square and an axis-aligned area.
pub(crate) fn intersects(ctm: Matrix, rect: Rect) -> bool {
    let image = [
        Point::new(0.0, 0.0),
        Point::new(1.0, 0.0),
        Point::new(0.0, 1.0),
        Point::new(1.0, 1.0),
    ]
    .map(|p| p.transform(&ctm));
    let area = [
        Point::new(rect.x0, rect.y0),
        Point::new(rect.x1, rect.y0),
        Point::new(rect.x0, rect.y1),
        Point::new(rect.x1, rect.y1),
    ];
    let axes = [
        Point::new(1.0, 0.0),
        Point::new(0.0, 1.0),
        Point::new(-ctm.b, ctm.a),
        Point::new(-ctm.d, ctm.c),
    ];
    axes.iter().all(|axis| {
        let interval = |points: &[Point; 4]| {
            points
                .iter()
                .map(|p| p.x * axis.x + p.y * axis.y)
                .fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), v| {
                    (lo.min(v), hi.max(v))
                })
        };
        let (a0, a1) = interval(&image);
        let (b0, b1) = interval(&area);
        a0 <= b1 && b0 <= a1
    })
}

fn dictionary(width: u32, height: u32) -> Dict {
    let mut dict = Dict::new();
    dict.insert("Type", Object::name("XObject"));
    dict.insert("Subtype", Object::name("Image"));
    dict.insert("Width", i64::from(width));
    dict.insert("Height", i64::from(height));
    dict.insert("BitsPerComponent", 8);
    dict
}

fn blank(
    data: &mut [u8],
    width: u32,
    height: u32,
    components: usize,
    white: u8,
    inverse: Matrix,
    rects: &[Rect],
) -> Result<(), EditError> {
    let expected = (width as usize)
        .checked_mul(height as usize)
        .and_then(|pixels| pixels.checked_mul(components));
    if width == 0 || height == 0 || expected != Some(data.len()) {
        return Err(EditError::Message(
            "decoded image samples have invalid dimensions".into(),
        ));
    }
    let transform = inverse.concat(&Matrix::scale(f64::from(width), f64::from(height)));
    for rect in rects {
        let r = rect.transform(&transform);
        let x0 = (r.x0 + 0.001).floor().clamp(0.0, f64::from(width)) as usize;
        let x1 = (r.x1 - 0.001).ceil().clamp(0.0, f64::from(width)) as usize;
        // ImageEvent.ctm already maps top-down image rows, unlike a raw Do CTM.
        let y0 = (r.y0 + 0.001).floor().clamp(0.0, f64::from(height)) as usize;
        let y1 = (r.y1 - 0.001).ceil().clamp(0.0, f64::from(height)) as usize;
        if x0 >= x1 || y0 >= y1 {
            continue;
        }
        for y in y0..y1 {
            let begin = (y * width as usize + x0) * components;
            let end = (y * width as usize + x1) * components;
            data[begin..end].fill(white);
        }
    }
    Ok(())
}
