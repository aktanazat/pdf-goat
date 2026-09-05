"""Geometry-aware text extraction for positioned, multi-column PDFs."""

from __future__ import annotations

from itertools import pairwise

DEFAULT_GUTTER_PT = 24.0
DEFAULT_LINE_TOLERANCE_PT = 3.0


def _clusters(
    words: list[tuple[float, float, float, float, str]], gutter_pt: float
) -> list[dict[str, object]]:
    intervals = sorted((word[0], word[2]) for word in words)
    groups: list[dict[str, object]] = []
    for x0, x1 in intervals:
        if not groups or x0 - groups[-1]["x1"] > gutter_pt:
            groups.append({"x0": x0, "x1": x1})
        else:
            groups[-1]["x1"] = max(groups[-1]["x1"], x1)
    return groups


def _line_groups(
    words: list[tuple[float, float, float, float, str]],
    tolerance_pt: float,
) -> list[dict[str, object]]:
    ordered = sorted(words, key=lambda word: (word[1], word[0]))
    lines: list[dict[str, object]] = []
    for x0, y0, x1, y1, text in ordered:
        if not text:
            continue
        if not lines or abs(y0 - lines[-1]["y0"]) > tolerance_pt:
            lines.append({"x0": x0, "y0": y0, "x1": x1, "y1": y1, "words": [text]})
            continue
        line = lines[-1]
        line["x0"] = min(line["x0"], x0)
        line["x1"] = max(line["x1"], x1)
        line["y1"] = max(line["y1"], y1)
        line["words"].append(text)
    return [
        {
            "x0": round(line["x0"], 2),
            "y0": round(line["y0"], 2),
            "x1": round(line["x1"], 2),
            "y1": round(line["y1"], 2),
            "text": " ".join(line["words"]),
        }
        for line in lines
    ]


def _separate_spanning_lines(
    words: list[tuple[float, float, float, float, str]],
    page_width: float,
    gutter_pt: float,
    tolerance_pt: float,
) -> tuple[
    list[tuple[float, float, float, float, str]],
    list[tuple[float, float, float, float, str]],
]:
    lines: list[list[tuple[float, float, float, float, str]]] = []
    for word in sorted(words, key=lambda item: (item[1], item[0])):
        if not lines or abs(word[1] - lines[-1][0][1]) > tolerance_pt:
            lines.append([word])
        else:
            lines[-1].append(word)

    body_words = []
    spanning_words = []
    for line in lines:
        ordered = sorted(line, key=lambda item: item[0])
        gaps = [right[0] - left[2] for left, right in pairwise(ordered)]
        spans_page = ordered[-1][2] - ordered[0][0] > page_width * 0.55
        target = (
            spanning_words
            if spans_page and max(gaps, default=0.0) <= gutter_pt
            else body_words
        )
        target.extend(ordered)
    return (body_words, spanning_words) if body_words else (words, [])


def _page_reading_order(
    column_lines: list[list[dict[str, object]]],
    spanning_words: list[tuple[float, float, float, float, str]],
    tolerance_pt: float,
) -> list[dict[str, object]]:
    reading_order: list[dict[str, object]] = []
    lower_y = float("-inf")
    for span in _line_groups(spanning_words, tolerance_pt):
        upper_y = float(span["y0"])
        for column_index, lines in enumerate(column_lines, start=1):
            reading_order.extend(
                {**line, "column": column_index}
                for line in lines
                if lower_y <= float(line["y0"]) < upper_y
            )
        reading_order.append({**span, "column": 0})
        lower_y = upper_y
    for column_index, lines in enumerate(column_lines, start=1):
        reading_order.extend(
            {**line, "column": column_index}
            for line in lines
            if float(line["y0"]) >= lower_y
        )
    return reading_order


def extract_page_layout(
    page,
    gutter_pt: float = DEFAULT_GUTTER_PT,
    line_tolerance_pt: float = DEFAULT_LINE_TOLERANCE_PT,
) -> dict[str, object]:
    """Return columns and lines in reading order without discarding coordinates.

    A column is a connected run of word boxes whose horizontal gaps stay below
    ``gutter_pt``. This deliberately uses a point threshold rather than a
    fraction of page width: transcript gutters are stable while page sizes vary.
    Single-column pages use PyMuPDF's sorted text verbatim for compatibility.
    """

    import pymupdf

    textpage = page.get_textpage(flags=pymupdf.TEXTFLAGS_TEXT)
    raw_words = page.get_text("words", sort=False, textpage=textpage)
    words: list[tuple[float, float, float, float, str]] = []
    for raw in raw_words:
        if len(raw) < 5:
            continue
        text = str(raw[4]).strip()
        if not text:
            continue
        words.append((float(raw[0]), float(raw[1]), float(raw[2]), float(raw[3]), text))

    body_words, spanning_words = _separate_spanning_lines(
        words,
        float(page.rect.width),
        gutter_pt,
        line_tolerance_pt,
    )
    body_top = min((word[1] for word in body_words), default=0.0)
    leading_words = [word for word in spanning_words if word[1] < body_top]
    trailing_words = [word for word in spanning_words if word[1] >= body_top]
    groups = _clusters(body_words, gutter_pt)
    columns: list[dict[str, object]] = []
    body_column_lines: list[list[dict[str, object]]] = []
    for group_index, group in enumerate(groups):
        group_words = [
            word
            for word in body_words
            if word[0] >= group["x0"] - 0.01 and word[0] <= group["x1"] + 0.01
        ]
        body_lines = _line_groups(group_words, line_tolerance_pt)
        body_column_lines.append(body_lines)
        if group_index == 0:
            group_words.extend(leading_words)
        if group_index == len(groups) - 1:
            group_words.extend(trailing_words)
        lines = _line_groups(group_words, line_tolerance_pt)
        if not lines:
            continue
        columns.append(
            {
                "x0": round(group["x0"], 2),
                "x1": round(group["x1"], 2),
                "text": "\n".join(line["text"] for line in lines),
                "lines": lines,
                "word_count": len(group_words),
                "line_count": len(lines),
            }
        )

    columns.sort(key=lambda column: column["x0"])
    if len(columns) <= 1:
        sorted_text = page.get_text("text", sort=True, textpage=textpage)
        reading_order = [
            {**line, "column": 1} for line in _line_groups(words, line_tolerance_pt)
        ]
        if columns:
            columns[0]["text"] = sorted_text
        else:
            columns = [
                {
                    "x0": 0.0,
                    "x1": 0.0,
                    "text": sorted_text,
                    "lines": [],
                    "word_count": 0,
                    "line_count": 0,
                }
            ]
        text = sorted_text
    else:
        reading_order = _page_reading_order(
            body_column_lines, spanning_words, line_tolerance_pt
        )
        text = "\n".join(str(line["text"]) for line in reading_order)

    return {
        "text": text,
        "columns": columns,
        "reading_order": reading_order,
        "column_count": len(columns),
        "word_count": len(words),
    }


def extract_document_layout(path: str) -> dict[str, object]:
    """Extract all pages with geometry while keeping document opening local."""

    import pymupdf

    doc = pymupdf.open(path)
    pages = []
    for index, page in enumerate(doc):
        result = extract_page_layout(page)
        result["page"] = index + 1
        pages.append(result)
    doc.close()
    return {"pages": pages, "page_count": len(pages)}
