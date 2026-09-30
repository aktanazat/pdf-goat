//! PDF dates (ISO 32000-2 7.9.4): `D:YYYYMMDDHHmmSSOHH'mm'`.

use std::fmt;
use std::time::{SystemTime, UNIX_EPOCH};

/// A calendar date and time with an optional offset from UTC.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PdfDate {
    pub year: u16,
    /// 1 to 12.
    pub month: u8,
    /// 1 to the length of the month.
    pub day: u8,
    pub hour: u8,
    pub minute: u8,
    pub second: u8,
    /// Minutes east of UTC; `None` when the date gives no offset.
    pub utc_offset_minutes: Option<i16>,
}

impl PdfDate {
    /// Parse a date string. The `D:` prefix and every field after the year
    /// are optional (month and day default to 1, the rest to 0); the offset
    /// may be `Z`, `+HH'mm'`, `+HH'mm`, `+HHmm`, or `+HH`. Text after a valid
    /// date is ignored. `None` when a field is out of range.
    pub fn parse(text: &str) -> Option<PdfDate> {
        let text = text.trim();
        let bytes = text.strip_prefix("D:").unwrap_or(text).as_bytes();
        let mut pos = 0;
        let year = u16::try_from(digits(bytes, &mut pos, 4)?).ok()?;
        let month = digits(bytes, &mut pos, 2).unwrap_or(1);
        let day = digits(bytes, &mut pos, 2).unwrap_or(1);
        let hour = digits(bytes, &mut pos, 2).unwrap_or(0);
        let minute = digits(bytes, &mut pos, 2).unwrap_or(0);
        let second = digits(bytes, &mut pos, 2).unwrap_or(0);
        if !(1..=12).contains(&month)
            || day == 0
            || day > days_in_month(year, month)
            || hour > 23
            || minute > 59
            || second > 59
        {
            return None;
        }
        let utc_offset_minutes = match bytes.get(pos) {
            Some(b'Z' | b'z') => Some(0),
            Some(&sign @ (b'+' | b'-')) => {
                pos += 1;
                let hours = digits(bytes, &mut pos, 2)?;
                if bytes.get(pos) == Some(&b'\'') {
                    pos += 1;
                }
                let minutes = digits(bytes, &mut pos, 2).unwrap_or(0);
                if hours > 23 || minutes > 59 {
                    return None;
                }
                let total = i16::try_from(hours * 60 + minutes).ok()?;
                Some(if sign == b'-' { -total } else { total })
            }
            _ => None,
        };
        Some(PdfDate {
            year,
            month: u8::try_from(month).ok()?,
            day: u8::try_from(day).ok()?,
            hour: u8::try_from(hour).ok()?,
            minute: u8::try_from(minute).ok()?,
            second: u8::try_from(second).ok()?,
            utc_offset_minutes,
        })
    }

    /// `D:YYYYMMDDHHmmSS` followed by `Z` for UTC, `+HH'mm'` or `-HH'mm'`
    /// for other offsets, and nothing when the offset is unknown.
    pub fn format(&self) -> String {
        let mut out = format!(
            "D:{:04}{:02}{:02}{:02}{:02}{:02}",
            self.year, self.month, self.day, self.hour, self.minute, self.second
        );
        match self.utc_offset_minutes {
            None => {}
            Some(0) => out.push('Z'),
            Some(offset) => {
                let sign = if offset < 0 { '-' } else { '+' };
                let abs = offset.unsigned_abs();
                out.push_str(&format!("{sign}{:02}'{:02}'", abs / 60, abs % 60));
            }
        }
        out
    }

    /// The date at `seconds` since the Unix epoch, shown at
    /// `utc_offset_minutes` from UTC. `None` outside years 0 to 9999.
    pub fn from_unix(seconds: i64, utc_offset_minutes: i16) -> Option<PdfDate> {
        let local = seconds.checked_add(i64::from(utc_offset_minutes) * 60)?;
        let days = local.div_euclid(86_400);
        let secs = local.rem_euclid(86_400);
        let (year, month, day) = civil_from_days(days);
        Some(PdfDate {
            year: u16::try_from(year).ok().filter(|y| *y <= 9999)?,
            month: u8::try_from(month).ok()?,
            day: u8::try_from(day).ok()?,
            hour: u8::try_from(secs / 3600).ok()?,
            minute: u8::try_from(secs % 3600 / 60).ok()?,
            second: u8::try_from(secs % 60).ok()?,
            utc_offset_minutes: Some(utc_offset_minutes),
        })
    }

    /// Seconds since the Unix epoch; an unknown offset counts as UTC.
    pub fn to_unix(&self) -> i64 {
        let days = days_from_civil(
            i64::from(self.year),
            i64::from(self.month),
            i64::from(self.day),
        );
        let local = days * 86_400
            + i64::from(self.hour) * 3600
            + i64::from(self.minute) * 60
            + i64::from(self.second);
        local - i64::from(self.utc_offset_minutes.unwrap_or(0)) * 60
    }

    /// The current time in UTC.
    pub fn now() -> PdfDate {
        let seconds = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(0));
        PdfDate::from_unix(seconds, 0).unwrap_or(PdfDate {
            year: 1970,
            month: 1,
            day: 1,
            hour: 0,
            minute: 0,
            second: 0,
            utc_offset_minutes: Some(0),
        })
    }
}

impl fmt::Display for PdfDate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.format())
    }
}

fn is_leap(year: u16) -> bool {
    (year.is_multiple_of(4) && !year.is_multiple_of(100)) || year.is_multiple_of(400)
}

fn days_in_month(year: u16, month: u32) -> u32 {
    match month {
        2 if is_leap(year) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// Days since 1970-01-01 for a proleptic Gregorian date.
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The proleptic Gregorian date `days` after 1970-01-01.
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

/// `width` ASCII digits at `*pos` as a number, advancing past them.
fn digits(bytes: &[u8], pos: &mut usize, width: usize) -> Option<u32> {
    let run = bytes.get(*pos..*pos + width)?;
    if !run.iter().all(u8::is_ascii_digit) {
        return None;
    }
    *pos += width;
    Some(run.iter().fold(0, |acc, d| acc * 10 + u32::from(d - b'0')))
}
