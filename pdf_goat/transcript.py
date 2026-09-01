"""Structured extraction for academic transcripts without title-only assumptions."""

from __future__ import annotations

import hashlib
import os
import re
from calendar import monthrange
from datetime import date, datetime, timezone
from fnmatch import fnmatch
from pathlib import Path

from .layout import extract_document_layout

_DATE_RE = re.compile(
    r"(?P<iso>20\d{2}-\d{1,2}-\d{1,2})|"
    r"(?P<slash>\d{1,2}/\d{1,2}/20\d{2})|"
    r"(?P<month>(?:Jan(?:uary)?|Feb(?:ruary)?|Mar(?:ch)?|Apr(?:il)?|May|"
    r"Jun(?:e)?|Jul(?:y)?|Aug(?:ust)?|Sep(?:t(?:ember)?)?|Oct(?:ober)?|"
    r"Nov(?:ember)?|Dec(?:ember)?)\s+\d{1,2},?\s+20\d{2})",
    re.IGNORECASE,
)
_TERM_RE = re.compile(
    r"\b(?P<season>Fall|Spring|Summer|Winter)\s+(?P<year>20\d{2})\b", re.IGNORECASE
)
_COURSE_RE = re.compile(
    r"^(?P<code>[A-Z][A-Z0-9-]{1,8}\s+\d{2,4}[A-Z]?)\s+"
    r"(?P<title>.+?)\s+(?P<grade>A[+-]?|B[+-]?|C[+-]?|D[+-]?|F|P|NP|S|U|CR|NC|I|IP|W)"
    r"(?:\s+(?P<units>\d+(?:\.\d+)?))?"
    r"(?:\s+(?P<points>\d+(?:\.\d+)?))?$",
    re.IGNORECASE,
)
_GRADE_RE = re.compile(
    r"\b(?:A[+-]?|B[+-]?|C[+-]?|D[+-]?|F|P|NP|S|U|CR|NC|I|IP|W)\b", re.IGNORECASE
)
_MONTHS = {
    "jan": 1,
    "january": 1,
    "feb": 2,
    "february": 2,
    "mar": 3,
    "march": 3,
    "apr": 4,
    "april": 4,
    "may": 5,
    "jun": 6,
    "june": 6,
    "jul": 7,
    "july": 7,
    "aug": 8,
    "august": 8,
    "sep": 9,
    "sept": 9,
    "september": 9,
    "oct": 10,
    "october": 10,
    "nov": 11,
    "november": 11,
    "dec": 12,
    "december": 12,
}
_SEASON_MONTHS = {
    "winter": (1, 3),
    "spring": (1, 6),
    "summer": (6, 8),
    "fall": (8, 12),
}


def _date_from_match(match: re.Match[str]) -> date | None:
    value = match.group(0).strip()
    try:
        if match.group("iso"):
            return date.fromisoformat(match.group("iso"))
        if match.group("slash"):
            month, day, year = (int(part) for part in value.split("/"))
            return date(year, month, day)
        month_name, day_text, year_text = re.split(r"\s+", value.replace(",", ""))
        return date(int(year_text), _MONTHS[month_name.lower()], int(day_text))
    except (KeyError, TypeError, ValueError):
        return None


def _find_date(text: str) -> date | None:
    for match in _DATE_RE.finditer(text):
        parsed = _date_from_match(match)
        if parsed is not None:
            return parsed
    return None


def _iso(value: date | None) -> str | None:
    return value.isoformat() if value else None


def _term_value(match: re.Match[str]) -> str:
    return f"{match.group('season').title()} {match.group('year')}"


def _term_bounds(term: str) -> tuple[date, date] | None:
    match = _TERM_RE.search(term)
    if not match:
        return None
    year = int(match.group("year"))
    start_month, end_month = _SEASON_MONTHS[match.group("season").lower()]
    return date(year, start_month, 1), date(
        year, end_month, monthrange(year, end_month)[1]
    )


def _clean_value(value: str) -> str:
    return re.sub(r"\s+", " ", value.strip(" :-\t"))


def _fingerprint(metadata: os.stat_result) -> tuple[int, int, int, int]:
    return (metadata.st_dev, metadata.st_ino, metadata.st_size, metadata.st_mtime_ns)


def _provenance(
    path: Path, page_count: int, initial_metadata: os.stat_result
) -> dict[str, object]:
    expected = _fingerprint(initial_metadata)
    digest = hashlib.sha256()
    with path.open("rb") as source:
        opened = os.fstat(source.fileno())
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(chunk)
        closed = os.fstat(source.fileno())
    if (
        _fingerprint(opened) != expected
        or _fingerprint(closed) != expected
        or _fingerprint(path.stat()) != expected
    ):
        raise RuntimeError("source changed during transcript read")
    modified = datetime.fromtimestamp(
        initial_metadata.st_mtime, timezone.utc
    ).isoformat()
    return {
        "path": str(path.resolve()),
        "sha256": digest.hexdigest(),
        "byte_size": initial_metadata.st_size,
        "modified_time": modified,
        "page_count": page_count,
    }


def _identity(
    lines: list[dict[str, object]],
) -> tuple[dict[str, object], str | None, date | None, dict[str, object]]:
    institution: str | None = None
    degree_name: str | None = None
    conferral_date: date | None = None
    issue_date: date | None = None
    degree_status = "unknown"
    student_name_present = False
    student_identifier_present = False
    title: str | None = None

    for entry in lines:
        text = str(entry["text"])
        lower = text.lower()
        if re.search(r"\b(?:student\s+id|student\s+number|id\s*:)\b", lower):
            student_identifier_present = True
        if re.search(r"\b(?:student|name)\s*:", lower):
            student_name_present = True
        if issue_date is None and re.search(
            r"\b(?:issued|issue\s+date|transcript\s+date|date\s+issued)\b", lower
        ):
            issue_date = _find_date(text)
        if conferral_date is None and re.search(
            r"\b(?:conferred|conferral|awarded|degree\s+date)\b", lower
        ):
            conferral_date = _find_date(text)
        degree_match = re.search(
            r"\b(?:degree|program)\s*[:\-]\s*(.+)$", text, re.IGNORECASE
        )
        if degree_match and degree_name is None:
            degree_name = _clean_value(degree_match.group(1))
        if re.search(r"\b(?:awarded|conferred|graduated)\b", lower):
            degree_status = "awarded"
        elif re.search(r"\b(?:pending|candidate|in progress)\b", lower):
            degree_status = "pending"
        if title is None and re.search(
            r"\bofficial\b.*\btranscript\b|\bacademic\s+transcript\b", lower
        ):
            title = _clean_value(text)
        if (
            institution is None
            and not lower.startswith("transfer")
            and re.search(
                r"\b(?:university|college|institute|school)\b|\buc\s+[a-z]+\b", lower
            )
        ):
            institution = _clean_value(text)

    identity = {
        "document_type": "academic_transcript",
        "title": title,
        "institution": institution,
        "student_name_present": student_name_present,
        "student_identifier_present": student_identifier_present,
    }
    degree = {
        "name": degree_name,
        "status": degree_status,
        "conferral_date": _iso(conferral_date),
    }
    return (
        identity,
        degree_name,
        issue_date,
        {"degree": degree, "conferral_date": conferral_date},
    )


def _freshness(
    issue_date: date | None, conferred: date | None, terms: list[dict[str, object]]
) -> dict[str, object]:
    bounds = [
        (str(term["term"]), term_bounds)
        for term in terms
        if (term_bounds := _term_bounds(str(term["term"]))) is not None
    ]
    latest_term, latest_bounds = max(
        bounds, key=lambda item: item[1][1], default=(None, None)
    )
    latest_end = latest_bounds[1] if latest_bounds else None
    term_covers_conferral = bool(
        conferred and any(start <= conferred <= end for _, (start, end) in bounds)
    )
    if conferred is None:
        verdict = "not_checked"
        reason = "no asserted conferral date"
    elif issue_date is None:
        verdict = "unknown_issue_date"
        reason = "the transcript did not expose a printed issue date"
    elif issue_date < conferred:
        verdict = "stale_before_conferral"
        reason = "the printed issue date precedes the asserted conferral date"
    elif not term_covers_conferral:
        verdict = "stale_missing_terms"
        reason = "no term on the transcript covers the asserted conferral date"
    else:
        verdict = "current"
        reason = "the printed issue date and a transcript term cover the asserted conferral date"
    return {
        "verdict": verdict,
        "issue_date": _iso(issue_date),
        "asserted_conferral_date": _iso(conferred),
        "latest_term": latest_term,
        "latest_term_end": _iso(latest_end),
        "reason": reason,
    }


def _line_records(layout: dict[str, object]) -> list[dict[str, object]]:
    records: list[dict[str, object]] = []
    for page in layout["pages"]:
        page_number = int(page["page"])
        lines = page.get("reading_order")
        if lines is None:
            lines = [
                {**line, "column": column_index}
                for column_index, column in enumerate(page["columns"], start=1)
                for line in column["lines"]
            ]
        for line_index, line in enumerate(lines, start=1):
            records.append(
                {
                    "page": page_number,
                    "column": int(line.get("column", 0)),
                    "line": line_index,
                    "text": str(line["text"]).strip(),
                }
            )
    return records


def _is_header_or_identity(text: str) -> bool:
    lower = text.lower()
    return bool(
        re.search(
            r"^(?:official|unofficial|academic)?\s*transcript$|^course\s+(?:id|code|title)|"
            r"^(?:student|name|address|student\s+id|id|career|level)\s*[:#]",
            lower,
        )
    ) or lower.startswith(("university ", "college ", "institution:"))


def _course_record(match: re.Match[str], entry: dict[str, object]) -> dict[str, object]:
    return {
        "course": match.group("code").upper(),
        "title": _clean_value(match.group("title")),
        "grade": match.group("grade").upper(),
        "units": float(match.group("units")) if match.group("units") else None,
        "points": float(match.group("points")) if match.group("points") else None,
        "page": entry["page"],
        "column": entry["column"],
    }


def _store_gpa(
    target: str,
    value: float,
    current_term: str | None,
    by_term: dict[str, dict[str, object]],
    cumulative_totals: dict[str, object],
) -> None:
    if target == "term" and current_term is not None:
        by_term[current_term]["totals"]["gpa"] = value
    else:
        cumulative_totals["gpa"] = value


def _append_course(
    match: re.Match[str],
    entry: dict[str, object],
    current_term: str | None,
    current_transfer: str | None,
    by_term: dict[str, dict[str, object]],
    transfer_blocks: list[dict[str, object]],
) -> bool:
    if current_term is not None:
        by_term[current_term]["courses"].append(_course_record(match, entry))
    elif current_transfer is not None:
        transfer_blocks[-1]["courses"].append(_course_record(match, entry))
    else:
        return False
    return True


def _is_known_record(text: str) -> bool:
    identity_record = _GRADE_RE.search(text) is None and re.search(
        r"\b(?:issued|issue\s+date|degree|program|awarded|conferred|student|name|address|career|level)\b",
        text,
        re.IGNORECASE,
    )
    return _is_header_or_identity(text) or bool(identity_record)


def _asserted_conferral(value: str | None) -> date | None:
    if value is None:
        return None
    try:
        return date.fromisoformat(value)
    except ValueError as error:
        raise ValueError("--conferred must be YYYY-MM-DD") from error


def _parse_quality(
    lines: list[dict[str, object]],
    matched_count: int,
    terms: list[dict[str, object]],
    transfer_blocks: list[dict[str, object]],
    unmatched: list[dict[str, object]],
) -> dict[str, object]:
    meaningful_lines = sum(bool(str(entry["text"]).strip()) for entry in lines)
    ratio = matched_count / meaningful_lines if meaningful_lines else 0.0
    confidence = "high" if ratio >= 0.8 else "medium" if ratio >= 0.5 else "low"
    courses = sum(len(term["courses"]) for term in terms) + sum(
        len(block["courses"]) for block in transfer_blocks
    )
    return {
        "confidence": confidence,
        "matched_line_count": matched_count,
        "unparsed_line_count": len(unmatched),
        "unparsed_lines": len(unmatched),
        "course_count": courses,
        "term_count": len(terms),
        "warnings": unmatched[:10],
    }


def parse_transcript(
    path: str | Path, conferred: str | None = None
) -> dict[str, object]:
    """Extract transcript records, page geometry, identity, and freshness evidence."""

    asserted = _asserted_conferral(conferred)
    source = Path(path).expanduser().resolve()
    if not source.is_file():
        raise FileNotFoundError(f"file not found: {path}")
    initial_metadata = source.stat()
    layout = extract_document_layout(str(source))
    lines = _line_records(layout)
    identity, degree_name, issue_date, degree_data = _identity(lines)
    terms: list[dict[str, object]] = []
    by_term: dict[str, dict[str, object]] = {}
    transfer_blocks: list[dict[str, object]] = []
    current_term: str | None = None
    current_transfer: str | None = None
    cumulative_totals: dict[str, object] = {}
    unmatched: list[dict[str, object]] = []
    matched_count = 0

    for entry in lines:
        text = str(entry["text"])
        if not text:
            continue
        term_match = _TERM_RE.search(text)
        if term_match:
            current_term = _term_value(term_match)
            if current_term not in by_term:
                by_term[current_term] = {
                    "term": current_term,
                    "courses": [],
                    "totals": {},
                }
                terms.append(by_term[current_term])
            matched_count += 1
            continue

        transfer_match = re.search(
            r"^transfer\s+(?:credit|institution)\s*[:\-]?\s*(.+)$", text, re.IGNORECASE
        )
        if transfer_match:
            current_transfer = _clean_value(transfer_match.group(1))
            current_term = None
            transfer_blocks.append({"institution": current_transfer, "courses": []})
            matched_count += 1
            continue

        total_match = re.search(
            r"\b(term|cumulative)\s+gpa\b\s*[:\-]?\s*(\d+(?:\.\d+)?)",
            text,
            re.IGNORECASE,
        )
        if total_match:
            _store_gpa(
                total_match.group(1).lower(),
                float(total_match.group(2)),
                current_term,
                by_term,
                cumulative_totals,
            )
            matched_count += 1
            continue

        course_match = _COURSE_RE.match(text)
        if course_match and _append_course(
            course_match,
            entry,
            current_term,
            current_transfer,
            by_term,
            transfer_blocks,
        ):
            matched_count += 1
            continue
        if _is_known_record(text):
            matched_count += 1
            continue
        unmatched.append(
            {
                "page": entry["page"],
                "column": entry["column"],
                "line": entry["line"],
                "text": text,
            }
        )

    if degree_data["degree"]["name"] is None:
        degree_data["degree"]["name"] = degree_name
    parse_quality = _parse_quality(
        lines, matched_count, terms, transfer_blocks, unmatched
    )
    return {
        "document_identity": identity,
        "issue_date": _iso(issue_date),
        "degree": degree_data["degree"],
        "terms": terms,
        "transfer_credit": transfer_blocks,
        "cumulative_totals": cumulative_totals,
        "freshness": _freshness(issue_date, asserted, terms),
        "source_provenance": _provenance(
            source, int(layout["page_count"]), initial_metadata
        ),
        "parse_quality": parse_quality,
        "layout": layout,
    }


def _candidate_patterns(patterns: list[str] | None) -> list[str]:
    return patterns or ["*.pdf"]


def discover_transcripts(
    roots: list[str], patterns: list[str] | None = None
) -> list[dict[str, object]]:
    """Inspect only explicitly named roots, one directory level deep.

    The workflow owns Drive enumeration. It can stage those bounded candidates
    beside local files and pass the staging root here; this function never crawls
    Drive, home directories, or recursive descendants.
    """

    candidates: list[Path] = []
    for raw_root in roots:
        root = Path(raw_root).expanduser().resolve()
        if root.is_file():
            candidates.append(root)
            continue
        if not root.is_dir():
            continue
        for item in root.iterdir():
            if item.is_file() and any(
                fnmatch(item.name, pattern) for pattern in _candidate_patterns(patterns)
            ):
                candidates.append(item)
    unique = {str(path): path for path in candidates}
    rows: list[dict[str, object]] = []
    for path in unique.values():
        try:
            parsed = parse_transcript(path)
        except (OSError, ValueError, RuntimeError) as error:
            rows.append({"path": str(path), "parse_error": str(error)})
            continue
        rows.append(
            {
                "path": str(path),
                "issue_date": parsed["issue_date"],
                "document_identity": parsed["document_identity"],
                "degree": parsed["degree"],
                "source_provenance": parsed["source_provenance"],
                "freshness": parsed["freshness"],
                "parse_quality": parsed["parse_quality"],
            }
        )
    rows.sort(
        key=lambda row: (
            str(row.get("issue_date") or "0000-00-00"),
            str(row.get("source_provenance", {}).get("modified_time", "")),
        ),
        reverse=True,
    )
    for rank, row in enumerate(rows, start=1):
        row["rank"] = rank
    return rows
