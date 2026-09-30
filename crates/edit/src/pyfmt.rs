//! Text the Python tools write byte for byte: PyMuPDF's `%g` numbers
//! (MuPDF `fz_format_double`), `JM_TUPLE` rounding, and `csv.writer` rows.

use std::fmt::Write as _;

/// MuPDF's `fmtfloat`: the shortest digits that round-trip the value as a
/// single-precision float, never in exponent form, with no leading zero
/// before the point (`.5`, `-.25`, `12.5`, `0`).
pub fn format_g(value: f64) -> String {
    let mut f = value as f32;
    if f.is_nan() {
        f = 0.0;
    }
    if f.is_infinite() {
        f = if f < 0.0 { f32::MIN } else { f32::MAX };
    }
    let mut out = String::new();
    if f.is_sign_negative() && f != 0.0 {
        out.push('-');
    }
    if f == 0.0 {
        if f.is_sign_negative() {
            out.push('-');
        }
        out.push('0');
        return out;
    }
    // Rust prints the shortest round-trip decimal without an exponent.
    let text = format!("{}", f.abs());
    let (int_part, frac_part) = text.split_once('.').unwrap_or((text.as_str(), ""));
    let frac_part = frac_part.trim_end_matches('0');
    if int_part == "0" {
        out.push('.');
        out.push_str(frac_part);
    } else {
        out.push_str(int_part);
        if !frac_part.is_empty() {
            out.push('.');
            out.push_str(frac_part);
        }
    }
    out
}

/// `_format_g` over a sequence: values joined by single spaces.
pub fn format_g_seq(values: &[f64]) -> String {
    let mut out = String::new();
    for (i, value) in values.iter().enumerate() {
        if i > 0 {
            out.push(' ');
        }
        out.push_str(&format_g(*value));
    }
    out
}

/// Python `round(x, ndigits)`: correctly rounded on the exact value.
pub fn py_round(value: f64, ndigits: usize) -> f64 {
    if !value.is_finite() {
        return value;
    }
    format!("{value:.ndigits$}").parse().unwrap_or(value)
}

/// PyMuPDF `JM_TUPLE`: five decimals, and zero below `1e-4` in magnitude.
pub fn jm_tuple(values: &[f64]) -> Vec<f64> {
    values
        .iter()
        .map(|x| {
            if x.abs() >= 1e-4 {
                py_round(*x, 5)
            } else {
                0.0
            }
        })
        .collect()
}

/// One `csv.writer` row with the default dialect: comma separated, fields
/// quoted when they hold a comma, quote, CR or LF (quotes doubled), `\r\n`
/// line terminator.
pub fn csv_row(fields: &[Option<&str>]) -> String {
    let mut out = String::new();
    for (i, field) in fields.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        let text = field.unwrap_or("");
        let quote = text.contains([',', '"', '\r', '\n']) || (fields.len() == 1 && text.is_empty());
        if quote {
            out.push('"');
            for ch in text.chars() {
                if ch == '"' {
                    out.push('"');
                }
                out.push(ch);
            }
            out.push('"');
        } else {
            let _ = write!(out, "{text}");
        }
    }
    out.push_str("\r\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_g_matches_mupdf_layout() {
        assert_eq!(format_g(0.0), "0");
        assert_eq!(format_g(0.5), ".5");
        assert_eq!(format_g(-0.25), "-.25");
        assert_eq!(format_g(72.0), "72");
        assert_eq!(format_g(100.123456), "100.12346");
        assert_eq!(format_g(1e-7), ".0000001");
        assert_eq!(format_g(1e10), "10000000000");
    }

    #[test]
    fn csv_quotes_like_python() {
        assert_eq!(csv_row(&[Some("a"), Some("b,c"), None]), "a,\"b,c\",\r\n");
        assert_eq!(
            csv_row(&[Some("say \"hi\""), Some("x\ny")]),
            "\"say \"\"hi\"\"\",\"x\ny\"\r\n"
        );
        assert_eq!(csv_row(&[Some("")]), "\"\"\r\n");
    }
}
