//! Structured academic transcripts: positioned reading order, records,
//! printed dates, freshness evidence, and source provenance.

use crate::pyre::{IGNORECASE, Pattern, compile};
use chrono::{Datelike, NaiveDate, Weekday};
use goat_common::GoatError;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::Path;
use std::sync::LazyLock;

pub(crate) const TERM: &str = r"\b(?P<season>Fall|Spring|Summer|Winter)\s+(?P<year>20\d{2})\b";
const DATE: &str = concat!(
    r"20\d{2}-\d{1,2}-\d{1,2}|\d{1,2}/\d{1,2}/20\d{2}|",
    r"(?:Jan(?:uary)?|Feb(?:ruary)?|Mar(?:ch)?|Apr(?:il)?|May|Jun(?:e)?|Jul(?:y)?|Aug(?:ust)?|Sep(?:t(?:ember)?)?|Oct(?:ober)?|Nov(?:ember)?|Dec(?:ember)?)\s+\d{1,2},?\s+20\d{2}"
);
const COURSE: &str = concat!(
    r"^(?P<code>[A-Z][A-Z0-9-]{1,8}\s+\d{2,4}[A-Z]?)\s+",
    r"(?P<title>.+?)\s+(?P<grade>A[+-]?|B[+-]?|C[+-]?|D[+-]?|F|P|NP|S|U|CR|NC|I|IP|W)",
    r"(?:\s+(?P<units>\d+(?:\.\d+)?))?(?:\s+(?P<points>\d+(?:\.\d+)?))?$"
);

struct Expressions {
    date: Pattern,
    term: Pattern,
    course: Pattern,
    grade: Pattern,
    student_id: Pattern,
    student_name: Pattern,
    issue: Pattern,
    conferral: Pattern,
    degree: Pattern,
    awarded: Pattern,
    pending: Pattern,
    title: Pattern,
    institution: Pattern,
    header: Pattern,
    known: Pattern,
    transfer: Pattern,
    total: Pattern,
}
impl Expressions {
    fn new() -> Result<Self, GoatError> {
        let p = |s| compile(s, IGNORECASE);
        Ok(Self {
            date: p(DATE)?,
            term: p(TERM)?,
            course: p(COURSE)?,
            grade: p(r"\b(?:A[+-]?|B[+-]?|C[+-]?|D[+-]?|F|P|NP|S|U|CR|NC|I|IP|W)\b")?,
            student_id: p(r"\b(?:student\s+id|student\s+number|id\s*:)\b")?,
            student_name: p(r"\b(?:student|name)\s*:")?,
            issue: p(r"\b(?:issued|issue\s+date|transcript\s+date|date\s+issued)\b")?,
            conferral: p(r"\b(?:conferred|conferral|awarded|degree\s+date)\b")?,
            degree: p(r"\b(?:degree|program)\s*[:\-]\s*(.+)$")?,
            awarded: p(r"\b(?:awarded|conferred|graduated)\b")?,
            pending: p(r"\b(?:pending|candidate|in progress)\b")?,
            title: p(r"\bofficial\b.*\btranscript\b|\bacademic\s+transcript\b")?,
            institution: p(r"\b(?:university|college|institute|school)\b|\buc\s+[a-z]+\b")?,
            header: p(
                r"^(?:official|unofficial|academic)?\s*transcript$|^course\s+(?:id|code|title)|^(?:student|name|address|student\s+id|id|career|level)\s*[:#]",
            )?,
            known: p(
                r"\b(?:issued|issue\s+date|degree|program|awarded|conferred|student|name|address|career|level)\b",
            )?,
            transfer: p(r"^transfer\s+(?:credit|institution)\s*[:\-]?\s*(.+)$")?,
            total: p(r"\b(term|cumulative)\s+gpa\b\s*[:\-]?\s*(\d+(?:\.\d+)?)")?,
        })
    }
}
static EXPRESSIONS: LazyLock<Result<Expressions, GoatError>> = LazyLock::new(Expressions::new);

#[derive(Clone)]
struct Record {
    page: usize,
    column: usize,
    line: usize,
    text: String,
}
impl Record {
    fn value(&self) -> Value {
        json!({"page":self.page,"column":self.column,"line":self.line,"text":self.text})
    }
}

fn clean(value: &str) -> String {
    value
        .trim_matches([' ', ':', '-', '\t'])
        .split(goat_common::py::is_space)
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

pub(crate) fn iso_date(value: &str) -> Option<NaiveDate> {
    let year = value.get(..4)?;
    if !year.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let year = year.parse::<i32>().ok()?;
    if year == 0 {
        return None;
    }
    let bytes = value.as_bytes();
    if bytes.len() == 10
        && bytes[4] == b'-'
        && bytes[7] == b'-'
        && bytes[5..7]
            .iter()
            .chain(&bytes[8..])
            .all(u8::is_ascii_digit)
    {
        return NaiveDate::parse_from_str(value, "%Y-%m-%d").ok();
    }
    if bytes.len() == 8 && bytes.iter().all(u8::is_ascii_digit) {
        return NaiveDate::parse_from_str(value, "%Y%m%d").ok();
    }
    let (tens, ones, day) = match &bytes[4..] {
        [b'W', a, b] | [b'-', b'W', a, b] => (*a, *b, b'1'),
        [b'W', a, b, d] | [b'-', b'W', a, b, b'-', d] => (*a, *b, *d),
        _ => return None,
    };
    if !tens.is_ascii_digit() || !ones.is_ascii_digit() {
        return None;
    }
    let week = u32::from(tens - b'0') * 10 + u32::from(ones - b'0');
    let weekday = match day {
        b'1' => Weekday::Mon,
        b'2' => Weekday::Tue,
        b'3' => Weekday::Wed,
        b'4' => Weekday::Thu,
        b'5' => Weekday::Fri,
        b'6' => Weekday::Sat,
        b'7' => Weekday::Sun,
        _ => return None,
    };
    NaiveDate::from_isoywd_opt(year, week, weekday).filter(|date| (1..=9999).contains(&date.year()))
}

fn find_date(text: &str, re: &Expressions) -> Result<Option<NaiveDate>, GoatError> {
    for found in re.date.finditer(text)? {
        let value = found.text.trim();
        let parsed = if value.contains('-') {
            iso_date(value)
        } else if value.contains('/') {
            let values: Vec<u32> = value.split('/').filter_map(|s| s.parse().ok()).collect();
            if values.len() == 3 {
                NaiveDate::from_ymd_opt(values[2] as i32, values[0], values[1])
            } else {
                None
            }
        } else {
            let plain = value.replace(',', "");
            let parts: Vec<&str> = plain.split_whitespace().collect();
            if parts.len() == 3 {
                let lower = parts[0].to_lowercase();
                let month = [
                    "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov",
                    "dec",
                ]
                .iter()
                .position(|prefix| lower.starts_with(prefix));
                match (
                    month,
                    parts[1].parse::<u32>().ok(),
                    parts[2].parse::<i32>().ok(),
                ) {
                    (Some(month), Some(day), Some(year)) => {
                        NaiveDate::from_ymd_opt(year, month as u32 + 1, day)
                    }
                    _ => None,
                }
            } else {
                None
            }
        };
        if parsed.is_some() {
            return Ok(parsed);
        }
    }
    Ok(None)
}
fn iso(date: Option<NaiveDate>) -> Option<String> {
    date.map(|d| d.format("%Y-%m-%d").to_string())
}

fn term_bounds(term: &str) -> Option<(NaiveDate, NaiveDate)> {
    let (season, year) = term.split_once(' ')?;
    let year = year.parse().ok()?;
    let (start, end) = match season {
        "Winter" => (1, 3),
        "Spring" => (1, 6),
        "Summer" => (6, 8),
        "Fall" => (8, 12),
        _ => return None,
    };
    let last = match end {
        3 | 12 => 31,
        6 => 30,
        8 => 31,
        _ => return None,
    };
    Some((
        NaiveDate::from_ymd_opt(year, start, 1)?,
        NaiveDate::from_ymd_opt(year, end, last)?,
    ))
}

fn freshness(issue: Option<NaiveDate>, conferred: Option<NaiveDate>, terms: &[Value]) -> Value {
    let mut latest: Option<(&str, NaiveDate)> = None;
    let mut covered = false;
    for term in terms {
        if let Some(name) = term["term"].as_str()
            && let Some((start, end)) = term_bounds(name)
        {
            if latest.is_none_or(|(_, prior)| end > prior) {
                latest = Some((name, end));
            }
            covered |= conferred.is_some_and(|date| start <= date && date <= end);
        }
    }
    let (verdict, reason) = match (conferred, issue) {
        (None, _) => ("not_checked", "no asserted conferral date"),
        (Some(_), None) => (
            "unknown_issue_date",
            "the transcript did not expose a printed issue date",
        ),
        (Some(c), Some(i)) if i < c => (
            "stale_before_conferral",
            "the printed issue date precedes the asserted conferral date",
        ),
        _ if !covered => (
            "stale_missing_terms",
            "no term on the transcript covers the asserted conferral date",
        ),
        _ => (
            "current",
            "the printed issue date and a transcript term cover the asserted conferral date",
        ),
    };
    json!({"verdict":verdict,"issue_date":iso(issue),"asserted_conferral_date":iso(conferred),"latest_term":latest.map(|v|v.0),"latest_term_end":iso(latest.map(|v|v.1)),"reason":reason})
}

/// Parse a transcript without trusting its filename or title as evidence
/// of freshness. The returned layout retains the source coordinates.
pub fn parse_transcript(path: &Path, conferred: Option<&str>) -> Result<Value, GoatError> {
    let asserted = conferred
        .map(|s| {
            iso_date(s).ok_or_else(|| GoatError::value_error("--conferred must be YYYY-MM-DD"))
        })
        .transpose()?;
    let source = goat_common::paths::resolve_lenient(&goat_common::paths::expanduser(
        &path.to_string_lossy(),
    ))?;
    if !source.is_file() {
        return Err(GoatError::exception(
            "FileNotFoundError",
            format!("file not found: {}", path.display()),
        ));
    }
    let metadata = std::fs::metadata(&source).map_err(|e| GoatError::os(&e, &source))?;
    let doc = crate::verbs::open(&source, None)?;
    let mut pages = Vec::with_capacity(doc.count);
    let mut lines = Vec::new();
    for index in 0..doc.count {
        let mut page = crate::layout::extract_page_layout(&doc.doc, index)?;
        page["page"] = (index + 1).into();
        if let Some(order) = page["reading_order"].as_array() {
            for (i, line) in order.iter().enumerate() {
                lines.push(Record {
                    page: index + 1,
                    column: line["column"].as_u64().unwrap_or(0) as usize,
                    line: i + 1,
                    text: goat_common::py::strip(line["text"].as_str().unwrap_or("")).to_owned(),
                });
            }
        }
        pages.push(page);
    }
    let re = EXPRESSIONS.as_ref().map_err(Clone::clone)?;
    let (mut institution, mut degree_name, mut issue, mut conferral, mut title) =
        (None, None, None, None, None);
    let mut degree_status = "unknown";
    let (mut student_name, mut student_id) = (false, false);
    for entry in &lines {
        let text = &entry.text;
        student_id |= re.student_id.is_match(text)?;
        student_name |= re.student_name.is_match(text)?;
        if issue.is_none() && re.issue.is_match(text)? {
            issue = find_date(text, re)?;
        }
        if conferral.is_none() && re.conferral.is_match(text)? {
            conferral = find_date(text, re)?;
        }
        if degree_name.is_none()
            && let Some(captures) = re.degree.captures(text)?
        {
            degree_name = captures.get(1).map(|m| clean(m.as_str()));
        }
        if re.awarded.is_match(text)? {
            degree_status = "awarded";
        } else if re.pending.is_match(text)? {
            degree_status = "pending";
        }
        if title.is_none() && re.title.is_match(text)? {
            title = Some(clean(text));
        }
        if institution.is_none()
            && !text.to_lowercase().starts_with("transfer")
            && re.institution.is_match(text)?
        {
            institution = Some(clean(text));
        }
    }
    let mut terms: Vec<Value> = Vec::new();
    let mut by_term = HashMap::new();
    let mut transfer: Vec<Value> = Vec::new();
    let mut current_term = None;
    let mut current_transfer = None;
    let mut totals = json!({});
    let mut unmatched = Vec::new();
    let mut matched = 0;
    for entry in &lines {
        let text = &entry.text;
        if text.is_empty() {
            continue;
        }
        if let Some(captures) = re.term.captures(text)? {
            let season = captures
                .name("season")
                .map_or("", |m| m.as_str())
                .to_lowercase();
            let season = match season.as_str() {
                "fall" => "Fall",
                "spring" => "Spring",
                "summer" => "Summer",
                _ => "Winter",
            };
            let year = captures.name("year").map_or("", |m| m.as_str());
            let name = format!("{season} {year}");
            let index = *by_term.entry(name.clone()).or_insert_with(|| {
                let index = terms.len();
                terms.push(json!({"term":name,"courses":[],"totals":{}}));
                index
            });
            current_term = Some(index);
            matched += 1;
            continue;
        }
        if let Some(captures) = re.transfer.captures(text)? {
            let institution = captures
                .get(1)
                .map_or_else(String::new, |m| clean(m.as_str()));
            current_term = None;
            current_transfer = Some(transfer.len());
            transfer.push(json!({"institution":institution,"courses":[]}));
            matched += 1;
            continue;
        }
        if let Some(captures) = re.total.captures(text)? {
            let target = captures.get(1).map_or("", |m| m.as_str());
            let number = captures.get(2).map_or("", |m| m.as_str());
            let value = goat_common::py::parse_float(number)?;
            if target.eq_ignore_ascii_case("term")
                && let Some(index) = current_term
            {
                terms[index]["totals"]["gpa"] = value.into();
            } else {
                totals["gpa"] = value.into();
            }
            matched += 1;
            continue;
        }
        if let Some(captures) = re.course.captures(text)? {
            let target = if let Some(index) = current_term {
                Some(&mut terms[index])
            } else {
                current_transfer.map(|index| &mut transfer[index])
            };
            if let Some(target) = target {
                let get = |name| captures.name(name).map(|m| m.as_str());
                let record = json!({"course":get("code").unwrap_or("").to_uppercase(),"title":clean(get("title").unwrap_or("")),"grade":get("grade").unwrap_or("").to_uppercase(),
                    "units":get("units").map(goat_common::py::parse_float).transpose()?,"points":get("points").map(goat_common::py::parse_float).transpose()?,"page":entry.page,"column":entry.column});
                if let Some(courses) = target["courses"].as_array_mut() {
                    courses.push(record);
                }
                matched += 1;
                continue;
            }
        }
        let lower = text.to_lowercase();
        if re.header.is_match(text)?
            || ["university ", "college ", "institution:"]
                .iter()
                .any(|prefix| lower.starts_with(prefix))
            || (!re.grade.is_match(text)? && re.known.is_match(text)?)
        {
            matched += 1;
        } else {
            unmatched.push(entry.value());
        }
    }
    let meaningful = lines.iter().filter(|r| !r.text.is_empty()).count();
    let ratio = if meaningful == 0 {
        0.0
    } else {
        matched as f64 / meaningful as f64
    };
    let courses = terms
        .iter()
        .chain(&transfer)
        .map(|v| v["courses"].as_array().map_or(0, Vec::len))
        .sum::<usize>();
    let quality = json!({"confidence":if ratio>=0.8 {"high"} else if ratio>=0.5 {"medium"} else {"low"},"matched_line_count":matched,"unparsed_line_count":unmatched.len(),"unparsed_lines":unmatched.len(),"course_count":courses,"term_count":terms.len(),"warnings":unmatched.iter().take(10).collect::<Vec<_>>()});
    let freshness = freshness(issue, asserted, &terms);
    let provenance = crate::transcript_io::provenance(&source, doc.count, &metadata)?;
    Ok(
        json!({"document_identity":{"document_type":"academic_transcript","title":title,"institution":institution,"student_name_present":student_name,"student_identifier_present":student_id},
        "issue_date":iso(issue),"degree":{"name":degree_name,"status":degree_status,"conferral_date":iso(conferral)},"terms":terms,"transfer_credit":transfer,"cumulative_totals":totals,
        "freshness":freshness,"source_provenance":provenance,"parse_quality":quality,"layout":{"pages":pages,"page_count":doc.count}}),
    )
}
