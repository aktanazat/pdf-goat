from __future__ import annotations

from pathlib import Path

import pymupdf


def write_transcript(
    path: Path, issue_date: str, terms: list[tuple[str, list[str]]]
) -> Path:
    """Write a positioned, synthetic transcript with no real-person content."""

    document = pymupdf.open()
    page = document.new_page(width=612, height=792)
    writer = pymupdf.TextWriter(page.rect)
    left = [
        "UNIVERSITY OF TEST",
        "OFFICIAL ACADEMIC TRANSCRIPT",
        "Student: REDACTED",
        f"Issued: {issue_date}",
        "Degree: Master of Science",
        "Degree Awarded: 2026-06-15",
    ]
    for row, text in enumerate(left):
        writer.append((40, 60 + row * 20), text, fontsize=10)

    row = 0
    for term, courses in terms:
        writer.append((350, 60 + row * 20), term, fontsize=10)
        row += 1
        writer.append(
            (350, 60 + row * 20),
            "Course ID Course Title Grade Units Points",
            fontsize=10,
        )
        row += 1
        for course in courses:
            writer.append((350, 60 + row * 20), course, fontsize=10)
            row += 1
        writer.append((350, 60 + row * 20), "Term GPA: 3.50", fontsize=10)
        row += 1
    writer.append((350, 60 + row * 20), "Transfer Credit: EXAMPLE COLLEGE", fontsize=10)
    writer.append(
        (350, 80 + row * 20), "HIST 100 World History B 3.00 9.00", fontsize=10
    )
    writer.write_text(page)
    document.save(path)
    document.close()
    return path


def write_single_column(path: Path) -> Path:
    document = pymupdf.open()
    page = document.new_page(width=612, height=792)
    page.insert_text((40, 60), "SINGLE COLUMN SAFE FIXTURE", fontsize=10)
    page.insert_text((40, 80), "No private content", fontsize=10)
    document.save(path)
    document.close()
    return path
