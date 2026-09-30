use goat_common::GoatError;
use pdf_core::{Dict, Document, Matrix, ObjRef, Object, Rect, Stream};
use pdf_font::{Standard14, cp1252_from_unicode};

use crate::{WidgetType, error, field_value_text, inherited, on_state, text, widget_type};

pub(crate) fn dict(doc: &Document, object: &Object) -> Result<Dict, GoatError> {
    Ok(doc.resolve_dict(object).map_err(error)?.unwrap_or_default())
}

pub(crate) fn child_dict(doc: &Document, parent: &Dict, key: &[u8]) -> Result<Dict, GoatError> {
    dict(doc, parent.get(key).unwrap_or(&Object::Null))
}

pub(crate) fn store_child(doc: &mut Document, parent: &mut Dict, key: &str, value: Object) {
    if let Some(id) = parent.get_ref(key.as_bytes()) {
        doc.set(id, value);
    } else {
        parent.insert(key, value);
    }
}

pub(crate) fn numbers(values: &[f64]) -> Object {
    Object::Array(values.iter().map(|n| Object::Real(*n)).collect())
}

pub(crate) fn mu(n: f64) -> String {
    let value = (n as f32).to_string();
    if value.starts_with("0.") {
        value[1..].to_owned()
    } else if value.starts_with("-0.") {
        format!("-{}", &value[2..])
    } else {
        value
    }
}

pub(crate) fn literal(value: &str) -> Vec<u8> {
    let mut output = vec![b'('];
    for character in value.chars() {
        let byte = cp1252_from_unicode(character)
            .or_else(|| u8::try_from(u32::from(character)).ok())
            .unwrap_or(b'?');
        match byte {
            b'(' | b')' | b'\\' => {
                output.push(b'\\');
                output.push(byte);
            }
            b'\n' => output.extend_from_slice(b"\\n"),
            b'\r' => output.extend_from_slice(b"\\r"),
            _ => output.push(byte),
        }
    }
    output.push(b')');
    output
}

pub(crate) fn color(values: &[f64], stroke: bool) -> String {
    let operator = match (values.len(), stroke) {
        (1, false) => "g",
        (1, true) => "G",
        (4, false) => "k",
        (4, true) => "K",
        (_, false) => "rg",
        (_, true) => "RG",
    };
    format!(
        "{} {operator}\n",
        values.iter().map(|n| mu(*n)).collect::<Vec<_>>().join(" ")
    )
}

pub(crate) fn font_resource(doc: &mut Document, font: Standard14) -> ObjRef {
    let mut dictionary = Dict::new();
    dictionary.insert("Type", Object::name("Font"));
    dictionary.insert("Subtype", Object::name("Type1"));
    dictionary.insert("BaseFont", Object::name(font.name()));
    dictionary.insert("Encoding", Object::name("WinAnsiEncoding"));
    doc.add(dictionary)
}

pub(crate) fn stream(doc: &mut Document, bbox: Rect, content: Vec<u8>, resources: Dict) -> ObjRef {
    let mut dictionary = Dict::new();
    dictionary.insert("Type", Object::name("XObject"));
    dictionary.insert("Subtype", Object::name("Form"));
    dictionary.insert("BBox", bbox.to_object());
    dictionary.insert("Matrix", Matrix::IDENTITY.to_object());
    if !resources.is_empty() {
        dictionary.insert("Resources", resources);
    }
    doc.add(Stream::new(dictionary, content))
}

pub(crate) fn font_resources(name: &str, id: ObjRef) -> Dict {
    let mut fonts = Dict::new();
    fonts.insert(name, Object::Reference(id));
    let mut resources = Dict::new();
    resources.insert("Font", fonts);
    resources
}

pub(crate) fn install(doc: &mut Document, widget: ObjRef, normal: Object) -> Result<(), GoatError> {
    let mut field = dict(doc, &Object::Reference(widget))?;
    let mut ap = child_dict(doc, &field, b"AP")?;
    ap.insert("N", normal);
    store_child(doc, &mut field, "AP", Object::Dict(ap));
    doc.set(widget, field);
    Ok(())
}

fn parse_da(da: &str) -> (&str, f64, Vec<f64>) {
    let tokens: Vec<_> = da.split_whitespace().collect();
    let mut name = "Helv";
    let mut size = 0.0;
    let mut ink = vec![0.0];
    for (i, token) in tokens.iter().enumerate() {
        if *token == "Tf" && i >= 2 {
            name = tokens[i - 2].trim_start_matches('/');
            size = tokens[i - 1].parse().unwrap_or(0.0);
        }
        let count = match *token {
            "g" => 1,
            "rg" => 3,
            "k" => 4,
            _ => 0,
        };
        if count > 0
            && i >= count
            && let Ok(values) = tokens[i - count..i]
                .iter()
                .map(|s| s.parse::<f64>())
                .collect()
        {
            ink = values;
        }
    }
    (name, size, ink)
}

pub(crate) fn wrap(text: &str, font: Standard14, size: f64, width: f64) -> Vec<String> {
    let mut lines = Vec::new();
    for paragraph in text.replace("\r\n", "\n").replace('\r', "\n").split('\n') {
        let mut line = String::new();
        for word in paragraph.split(' ') {
            let trial = if line.is_empty() {
                word.to_owned()
            } else {
                format!("{line} {word}")
            };
            if !line.is_empty() && f64::from(font.text_width(&trial, size as f32)) > width {
                lines.push(std::mem::take(&mut line));
                line.push_str(word);
            } else {
                line = trial;
            }
        }
        lines.push(line);
    }
    lines
}

/// MuPDF's variable-text layout used by widgets and free-text annotations.
pub(crate) struct VariableText<'a> {
    pub text: &'a str,
    pub font: Standard14,
    pub name: &'a str,
    pub size: f64,
    pub width: f64,
    pub height: f64,
    pub padding: f64,
    pub multiline: bool,
    pub align: i64,
    pub ink: &'a [f64],
}

pub(crate) fn variable_text(layout: VariableText<'_>) -> Vec<u8> {
    let VariableText {
        text,
        font,
        name,
        mut size,
        width,
        height,
        padding,
        multiline,
        align,
        ink,
    } = layout;
    let width = (width - 2.0 * padding).max(0.0);
    let height = (height - 2.0 * padding).max(0.0);
    if size == 0.0 {
        let measure = f64::from(font.text_width(text, 1.0));
        size = if multiline {
            12.0
        } else if measure > 0.0 {
            (width / measure).min(height)
        } else {
            height
        };
    }
    let baseline = 0.8 * size;
    let leading = f64::from(1.2_f32 * size as f32);
    let mut output = format!("BT\n{}", color(ink, false)).into_bytes();
    if multiline {
        output.extend_from_slice(
            format!(
                "{} {} Td\n",
                mu(padding),
                mu(padding + height - baseline + leading)
            )
            .as_bytes(),
        );
        let mut previous = 0.0;
        for line in wrap(text, font, size, width) {
            let line_width = f64::from(font.text_width(&line, size as f32));
            let x = match align {
                1 => (width - line_width) / 2.0,
                2 => width - line_width,
                _ => 0.0,
            };
            output.extend_from_slice(
                format!(
                    "{} {} Td\n/{name} {} Tf\n",
                    mu(x - previous),
                    mu(-leading),
                    mu(size)
                )
                .as_bytes(),
            );
            output.extend_from_slice(&literal(&line));
            output.extend_from_slice(b" Tj\n");
            previous = x;
        }
    } else {
        let line_width = f64::from(font.text_width(text, size as f32));
        let x = match align {
            1 => (width - line_width) / 2.0,
            2 => width - line_width,
            _ => 0.0,
        };
        let y = padding + height - baseline - (height - size) / 2.0;
        output.extend_from_slice(format!("{} {} Td\n", mu(padding + x), mu(y)).as_bytes());
        if !text.is_empty() {
            output.extend_from_slice(format!("/{name} {} Tf\n", mu(size)).as_bytes());
            output.extend_from_slice(&literal(text));
            output.extend_from_slice(b" Tj\n");
        }
    }
    output.extend_from_slice(b"ET\n");
    output
}

pub(crate) fn circle_path(rect: Rect) -> String {
    let (cx, cy) = (
        (rect.x0 + rect.x1) as f32 / 2.0,
        (rect.y0 + rect.y1) as f32 / 2.0,
    );
    let (rx, ry) = (
        (rect.x1 - rect.x0) as f32 / 2.0,
        (rect.y1 - rect.y0) as f32 / 2.0,
    );
    let (mx, my) = (rx * 0.551915, ry * 0.551915);
    let p = |x: f32, y: f32| format!("{} {}", mu(f64::from(x)), mu(f64::from(y)));
    format!(
        "{} m\n{} {} {} c\n{} {} {} c\n{} {} {} c\n{} {} {} c\n",
        p(cx, cy + ry),
        p(cx + mx, cy + ry),
        p(cx + rx, cy + my),
        p(cx + rx, cy),
        p(cx + rx, cy - my),
        p(cx + mx, cy - ry),
        p(cx, cy - ry),
        p(cx - mx, cy - ry),
        p(cx - rx, cy - my),
        p(cx - rx, cy),
        p(cx - rx, cy + my),
        p(cx - mx, cy + ry),
        p(cx, cy + ry)
    )
}

pub fn set_field_value(doc: &mut Document, widget: ObjRef, value: &str) -> Result<(), GoatError> {
    let mut field = dict(doc, &Object::Reference(widget))?;
    let kind = widget_type(doc, &field)?;
    if matches!(kind, WidgetType::CheckBox | WidgetType::RadioButton) {
        let on = on_state(doc, &field)?.as_str().unwrap_or("Yes").to_owned();
        let state = if value == on || value == "Yes" {
            on.as_str()
        } else {
            "Off"
        };
        field.insert("V", Object::name(state));
        field.insert("AS", Object::name(state));
    } else if !value.is_empty() {
        field.insert("V", Object::text(value));
        if matches!(kind, WidgetType::ComboBox | WidgetType::ListBox) {
            field.remove(b"I");
        }
    }
    doc.set(widget, field);
    update_widget_appearance(doc, widget)
}

pub fn update_widget_appearance(doc: &mut Document, widget: ObjRef) -> Result<(), GoatError> {
    let mut field = dict(doc, &Object::Reference(widget))?;
    let kind = widget_type(doc, &field)?;
    let rect = field
        .get_array(b"Rect")
        .and_then(Rect::from_array)
        .ok_or_else(|| GoatError::message("widget has no rectangle"))?
        .normalized();
    let (w, h) = (rect.width(), rect.height());
    let bbox = Rect::new(0.0, 0.0, w, h);
    let bs = child_dict(doc, &field, b"BS")?;
    let mut border = bs.get_f64(b"W").unwrap_or(1.0);
    if border == 0.0 {
        border = 1.0;
    }
    let mk = child_dict(doc, &field, b"MK")?;
    let bg: Vec<f64> = mk
        .get_array(b"BG")
        .unwrap_or(&[])
        .iter()
        .filter_map(Object::as_f64)
        .collect();
    let bc: Vec<f64> = mk
        .get_array(b"BC")
        .unwrap_or(&[])
        .iter()
        .filter_map(Object::as_f64)
        .collect();
    let flags = inherited(doc, &field, b"Ff")?.as_i64().unwrap_or(0);
    let da = inherited(doc, &field, b"DA")?;
    let da = if da.is_null() {
        let acro = child_dict(doc, &doc.catalog().map_err(error)?, b"AcroForm")?;
        text(&doc.resolve_key(&acro, b"DA").map_err(error)?)
    } else {
        text(&da)
    };
    let (font_name, size, ink) = parse_da(&da);
    let font = match font_name {
        "Cour" => Standard14::Courier,
        "TiRo" => Standard14::TimesRoman,
        "ZaDb" => Standard14::ZapfDingbats,
        _ => Standard14::from_base_font(font_name).unwrap_or(Standard14::Helvetica),
    };
    let mut body = b"q\n".to_vec();
    if !bg.is_empty() {
        body.extend_from_slice(color(&bg, false).as_bytes());
        body.extend_from_slice(
            if kind == WidgetType::RadioButton {
                circle_path(bbox)
            } else {
                format!("0 0 {} {} re\n", mu(w), mu(h))
            }
            .as_bytes(),
        );
        body.extend_from_slice(b"f\n");
    }
    body.extend_from_slice(format!("{} w\n", mu(border)).as_bytes());
    if !bc.is_empty() {
        body.extend_from_slice(color(&bc, true).as_bytes());
        if kind == WidgetType::RadioButton {
            body.extend_from_slice(
                circle_path(Rect::new(
                    border / 2.0,
                    border / 2.0,
                    w - border / 2.0,
                    h - border / 2.0,
                ))
                .as_bytes(),
            );
            body.extend_from_slice(b"S\n");
        } else {
            body.extend_from_slice(
                format!(
                    "{} {} {} {} re\n{}\n",
                    mu(border / 2.0),
                    mu(border / 2.0),
                    mu(w - border),
                    mu(h - border),
                    if kind == WidgetType::CheckBox {
                        "S"
                    } else {
                        "s"
                    }
                )
                .as_bytes(),
            );
        }
    }
    if matches!(kind, WidgetType::CheckBox | WidgetType::RadioButton) {
        let mut off = body.clone();
        off.extend_from_slice(b"Q\n");
        let off = stream(doc, bbox, off, Dict::new());
        let mut resources = Dict::new();
        if kind == WidgetType::RadioButton {
            let r = (w.min(h) / 2.0 - border - 2.0).max(0.0);
            body.extend_from_slice(
                format!(
                    "0 g\n{}f\n",
                    circle_path(Rect::new(
                        w / 2.0 - r,
                        h / 2.0 - r,
                        w / 2.0 + r,
                        h / 2.0 + r
                    ))
                )
                .as_bytes(),
            );
        } else {
            let font = font_resource(doc, Standard14::ZapfDingbats);
            resources = font_resources("ZaDb", font);
            body.extend(variable_text(VariableText {
                text: "3",
                font: Standard14::ZapfDingbats,
                name: "ZaDb",
                size: h,
                width: w,
                height: h,
                padding: border + h / 10.0,
                multiline: false,
                align: 0,
                ink: &[0.0],
            }));
        }
        body.extend_from_slice(b"Q\n");
        let yes = stream(doc, bbox, body, resources);
        let state = on_state(doc, &field)?.as_str().unwrap_or("Yes").to_owned();
        let mut states = Dict::new();
        states.insert("Off", Object::Reference(off));
        states.insert(state.as_str(), Object::Reference(yes));
        install(doc, widget, Object::Dict(states))?;
    } else if matches!(
        kind,
        WidgetType::Text | WidgetType::ComboBox | WidgetType::ListBox | WidgetType::Button
    ) {
        let value = if kind == WidgetType::Button {
            text(&doc.resolve_key(&mk, b"CA").map_err(error)?)
        } else {
            field_value_text(doc, &field)?
        };
        body.extend_from_slice(
            format!(
                "{} {} {} {} re\nW\nn\n",
                mu(border),
                mu(border),
                mu(w - 2.0 * border),
                mu(h - 2.0 * border)
            )
            .as_bytes(),
        );
        if kind == WidgetType::ListBox {
            let options = inherited(doc, &field, b"Opt")?;
            let size = if size == 0.0 { 12.0 } else { size };
            let leading = 1.2 * size;
            let top = inherited(doc, &field, b"TI")?.as_i64().unwrap_or(0).max(0) as usize;
            let selection = inherited(doc, &field, b"V")?;
            for (row, option) in options
                .as_array()
                .unwrap_or(&[])
                .iter()
                .skip(top)
                .enumerate()
            {
                let (export, display) = match option.as_array() {
                    Some(pair) if pair.len() == 2 => (text(&pair[0]), text(&pair[1])),
                    _ => (text(option), text(option)),
                };
                let y = h - 2.0 * border - (row + 1) as f64 * leading;
                if y + leading < border {
                    break;
                }
                let selected = selection.as_array().map_or(value == export, |values| {
                    values.iter().any(|v| text(v) == export)
                });
                if selected {
                    body.extend_from_slice(
                        format!(
                            "q\n.6 .75 .85 rg\n{} {} {} {} re\nf\nQ\n",
                            mu(border),
                            mu(y),
                            mu(w - 2.0 * border),
                            mu(leading)
                        )
                        .as_bytes(),
                    );
                }
                body.extend_from_slice(
                    format!(
                        "BT\n{}/{font_name} {} Tf\n{} {} Td\n",
                        color(&ink, false),
                        mu(size),
                        mu(2.0 * border),
                        mu(y + 0.2 * size)
                    )
                    .as_bytes(),
                );
                body.extend(literal(&display));
                body.extend_from_slice(b" Tj\nET\n");
            }
        } else {
            body.extend(variable_text(VariableText {
                text: &value,
                font,
                name: font_name,
                size,
                width: w,
                height: h,
                padding: 2.0 * border,
                multiline: flags & 4096 != 0,
                align: inherited(doc, &field, b"Q")?.as_i64().unwrap_or(0),
                ink: &ink,
            }));
        }
        body.extend_from_slice(b"Q\nEMC\n");
        let mut content = b"/Tx BMC\n".to_vec();
        content.extend(body);
        let font = font_resource(doc, font);
        let normal = stream(doc, bbox, content, font_resources(font_name, font));
        install(doc, widget, Object::Reference(normal))?;
    }
    field = dict(doc, &Object::Reference(widget))?;
    let mut bs = child_dict(doc, &field, b"BS")?;
    bs.insert("S", Object::name("S"));
    bs.insert("W", border);
    field.insert("BS", bs);
    field.insert("Ff", flags);
    field.insert(
        "DA",
        Object::text(&format!(
            "{} /{font_name} {} Tf",
            color(&ink, false).trim_end(),
            mu(size)
        )),
    );
    doc.set(widget, field);
    Ok(())
}

pub(crate) fn pypdf_text(
    doc: &mut Document,
    widget: ObjRef,
    parent: &Dict,
    acro_id: ObjRef,
) -> Result<(), GoatError> {
    let mut field = dict(doc, &Object::Reference(widget))?;
    let rect = field
        .get_array(b"Rect")
        .and_then(Rect::from_array)
        .ok_or_else(|| GoatError::exception("KeyError", "'/Rect'"))?
        .normalized();
    let (w, h) = (rect.width(), rect.height());
    let mut acro = dict(doc, &Object::Reference(acro_id))?;
    let da = inherited(doc, &field, b"DA")?;
    let da = if da.is_null() {
        text(&doc.resolve_key(&acro, b"DA").map_err(error)?)
    } else {
        text(&da)
    };
    let da = if da.is_empty() {
        "/Helv 0 Tf 0 g"
    } else {
        da.as_str()
    };
    let (name, mut size, ink) = parse_da(da);
    let mut name = name.to_owned();
    let dr = inherited(doc, &field, b"DR")?;
    let mut dr = if dr.is_null() {
        child_dict(doc, &acro, b"DR")?
    } else {
        dict(doc, &dr)?
    };
    let mut fonts = child_dict(doc, &dr, b"Font")?;
    let mut resource = fonts.get(name.as_bytes()).cloned().unwrap_or_default();
    let mut font_dict = dict(doc, &resource)?;
    let base = font_dict
        .get_name(b"BaseFont")
        .map(|n| String::from_utf8_lossy(n).into_owned())
        .unwrap_or_else(|| name.clone());
    let standard = Standard14::from_base_font(&base).unwrap_or(Standard14::Helvetica);
    if resource.is_null() {
        if Standard14::from_base_font(&name).is_none() {
            name = "Helvetica".into();
            field.insert(
                "DA",
                Object::text(&da.replace(&format!("/{}", parse_da(da).0), &format!("/{name}"))),
            );
        }
        let id = font_resource(doc, standard);
        resource = Object::Reference(id);
        font_dict = dict(doc, &resource)?;
        fonts.insert(name.as_str(), resource.clone());
        store_child(doc, &mut dr, "Font", Object::Dict(fonts));
        store_child(doc, &mut acro, "DR", Object::Dict(dr));
        doc.set(acro_id, acro);
        doc.set(widget, field.clone());
    }
    let descriptor = child_dict(doc, &font_dict, b"FontDescriptor")?;
    let ascent = descriptor
        .get_f64(b"Ascent")
        .unwrap_or(f64::from(standard.metrics().ascent.unwrap_or(0.0)));
    let bbox = descriptor
        .get_array(b"FontBBox")
        .and_then(Rect::from_array)
        .unwrap_or_else(|| {
            let b = standard.metrics().bbox;
            Rect::new(
                f64::from(b[0]),
                f64::from(b[1]),
                f64::from(b[2]),
                f64::from(b[3]),
            )
        });
    let factor = (bbox.y1 - bbox.y0) / 1000.0;
    if factor <= 0.0 {
        return Err(GoatError::value_error("invalid font bounding box"));
    }
    let widths = doc.resolve_key(&font_dict, b"Widths").map_err(error)?;
    let first = font_dict.get_i64(b"FirstChar").unwrap_or(0);
    let default_width = descriptor.get_f64(b"MissingWidth").unwrap_or_else(|| {
        if base.starts_with("Courier") {
            600.0
        } else {
            2.0 * f64::from(standard.code_width(b' '))
        }
    });
    let text_width = |text: &str| -> f64 {
        text.chars()
            .map(|c| {
                let code = cp1252_from_unicode(c).or_else(|| u8::try_from(u32::from(c)).ok());
                if let Some(widths) = widths.as_array() {
                    return code
                        .and_then(|b| usize::try_from(i64::from(b) - first).ok())
                        .and_then(|i| widths.get(i))
                        .and_then(Object::as_f64)
                        .unwrap_or(default_width);
                }
                code.map(|b| standard.code_width(b))
                    .filter(|w| *w != 0)
                    .map(f64::from)
                    .unwrap_or(default_width)
            })
            .sum()
    };
    let flags = parent.get_i64(b"Ff").unwrap_or(0);
    let multiline = flags & 4096 != 0;
    let listbox = parent.get_name(b"FT") == Some(b"Ch") && flags & 131072 == 0;
    let value = doc.resolve_key(parent, b"V").map_err(error)?;
    let selected = value
        .as_array()
        .map(|a| a.iter().map(text).collect::<Vec<_>>())
        .unwrap_or_else(|| vec![text(&value)])
        .join("");
    let value = if listbox {
        inherited(doc, &field, b"Opt")?
            .as_array()
            .unwrap_or(&[])
            .iter()
            .map(|v| match v.as_array() {
                Some(pair) => pair.last().map(text).unwrap_or_default(),
                _ => text(v),
            })
            .collect::<Vec<_>>()
            .join("\n")
    } else {
        text(&value)
    };
    let escaped = value
        .replace('\\', "\\\\")
        .replace('(', "\\(")
        .replace(')', "\\)");
    let bs = child_dict(doc, parent, b"BS")?;
    let border = bs.get_f64(b"W").unwrap_or(1.0);
    let margin = (border
        * if matches!(bs.get_name(b"S"), Some(b"B" | b"I")) {
            2.0
        } else {
            1.0
        })
    .max(1.0);
    let fh = h - 2.0 * margin;
    let fw = w - 4.0 * margin;
    let mut lines: Vec<String> = escaped.lines().map(str::to_owned).collect();
    if size == 0.0 {
        if multiline {
            size = 12.0;
            loop {
                lines = wrap(&escaped, standard, size, fw);
                if size + lines.len().saturating_sub(1) as f64 * factor * size <= fh || size <= 4.0
                {
                    break;
                }
                size -= 0.2;
            }
        } else {
            let measure = text_width(&escaped) / 1000.0;
            size = (fh / factor)
                .min(fw / if measure == 0.0 { 1.0 } else { measure })
                .max(4.0);
            lines = vec![escaped.clone()];
        }
        size = crate::rounded(size, 1);
    }
    let maxlen = field.get_i64(b"MaxLen").unwrap_or(0);
    let comb = flags & (1 << 24) != 0 && maxlen > 0;
    if comb {
        lines = escaped.chars().map(|c| c.to_string()).collect();
    }
    let y = if multiline {
        h + margin - bbox.y1 * size / 1000.0
    } else {
        margin + (fh - ascent * size / 1000.0) / 2.0
    };
    let py = goat_common::py::float_repr;
    let margin_repr = |value: f64| {
        if bs.get(b"W").is_none_or(|v| matches!(v, Object::Integer(_))) || margin == 1.0 {
            format!("{value:.0}")
        } else {
            py(value)
        }
    };
    let da = format!("/{name} {} Tf {}", py(size), color(&ink, false).trim_end());
    let mut content = format!(
        "q\n/Tx BMC \nq\n{} {} {} {} re\nW\nBT\n{da}\n",
        margin_repr(2.0 * margin),
        margin_repr(margin),
        py(fw),
        py(fh)
    )
    .into_bytes();
    let mut current = 0.0;
    for (i, line) in lines.iter().enumerate() {
        if listbox && selected.contains(line) {
            content.extend_from_slice(
                format!(
                    "1 {} {} {} re\n0.5 0.5 0.5 rg s\n{da}\n",
                    py(y - i as f64 * size * factor - 1.0),
                    py(w - 2.0),
                    py(size + 2.0)
                )
                .as_bytes(),
            );
        }
        let lw = text_width(line) * size / 1000.0;
        let x = if comb {
            i as f64 * w / maxlen as f64 + (w / maxlen as f64 - lw) / 2.0
        } else {
            match parent.get_i64(b"Q").unwrap_or(0) {
                1 => (w - lw) / 2.0,
                2 => w - 2.0 * margin - lw,
                _ => 2.0 * margin,
            }
        };
        let dx = if !comb && parent.get_i64(b"Q").unwrap_or(0) == 0 {
            margin_repr(x - current)
        } else {
            py(x - current)
        };
        let dy = if i == 0 {
            y
        } else if comb {
            0.0
        } else {
            -size * factor
        };
        content.extend_from_slice(format!("{dx} {} Td\n(", py(dy)).as_bytes());
        for c in line.chars() {
            content.push(
                cp1252_from_unicode(c)
                    .or_else(|| u8::try_from(u32::from(c)).ok())
                    .unwrap_or(b'?'),
            );
        }
        content.extend_from_slice(b") Tj\n");
        current = x;
    }
    content.extend_from_slice(b"ET\nQ\nEMC\nQ\n");
    let ap = child_dict(doc, &field, b"AP")?;
    let old = ap.get(b"N").cloned().unwrap_or_default();
    let old_object = doc.resolve(&old).map_err(error)?;
    let mut sd = old_object
        .as_stream()
        .map(|s| s.dict.clone())
        .unwrap_or_default();
    sd.remove(b"Filter");
    sd.remove(b"DecodeParms");
    sd.remove(b"Length");
    sd.insert("Type", Object::name("XObject"));
    sd.insert("Subtype", Object::name("Form"));
    sd.insert("BBox", Rect::new(0.0, 0.0, w, h).to_object());
    let mut resources = child_dict(doc, &sd, b"Resources")?;
    let mut fonts = child_dict(doc, &resources, b"Font")?;
    fonts.insert(name.as_str(), resource);
    store_child(doc, &mut resources, "Font", Object::Dict(fonts));
    store_child(doc, &mut sd, "Resources", Object::Dict(resources));
    let stream = Stream::new(sd, content);
    let normal = if let Some(id) = old.as_reference() {
        doc.set(id, stream);
        id
    } else {
        doc.add(stream)
    };
    install(doc, widget, Object::Reference(normal))
}
