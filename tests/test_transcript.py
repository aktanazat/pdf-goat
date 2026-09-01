from __future__ import annotations

import contextlib
import io
import json
import os
import shutil
import sqlite3
import tempfile
import unittest
from pathlib import Path

import pymupdf

from pdf_goat import cli
from pdf_goat.layout import extract_page_layout
from pdf_goat.transcript import _provenance, discover_transcripts, parse_transcript
from tests.fixtures import write_single_column, write_transcript


class TranscriptExtractionTests(unittest.TestCase):
    def setUp(self) -> None:
        self.tempdir = Path(tempfile.mkdtemp(prefix="pdf-goat-tests-"))
        self.pdf = write_transcript(
            self.tempdir / "transcript.pdf",
            "2026-04-03",
            [
                (
                    "Fall 2025",
                    [
                        "CS 101 Intro to Computing A 4.00 16.00",
                        "MATH 201 Discrete Math B+ 3.00 9.99",
                    ],
                )
            ],
        )

    def tearDown(self) -> None:
        shutil.rmtree(self.tempdir)

    def test_layout_separates_columns_and_preserves_single_column_text(self) -> None:
        document = pymupdf.open(self.pdf)
        sorted_text = document[0].get_text("text", sort=True)
        layout = extract_page_layout(document[0])
        document.close()

        collapsed = [
            line
            for line in sorted_text.splitlines()
            if "UNIVERSITY OF TEST" in line and "Fall 2025" in line
        ]
        self.assertEqual(len(collapsed), 1)
        self.assertEqual(layout["column_count"], 2)
        self.assertEqual(layout["columns"][0]["lines"][0]["text"], "UNIVERSITY OF TEST")
        self.assertEqual(layout["columns"][1]["lines"][0]["text"], "Fall 2025")

        single = write_single_column(self.tempdir / "single.pdf")
        document = pymupdf.open(single)
        expected = document[0].get_text("text", sort=True)
        single_layout = extract_page_layout(document[0])
        document.close()
        self.assertEqual(single_layout["text"], expected)

        wide_header = self.tempdir / "wide-header.pdf"
        document = pymupdf.open()
        page = document.new_page(width=612, height=792)
        page.insert_text(
            (40, 50),
            "UNIVERSITY OF TEST OFFICIAL ACADEMIC TRANSCRIPT STUDENT RECORD SUMMARY VERIFIED COPY",
            fontsize=10,
        )
        for row, y in enumerate((120, 150), start=1):
            page.insert_text((40, y), f"LEFT COLUMN {row}", fontsize=10)
            page.insert_text((350, y), f"RIGHT COLUMN {row}", fontsize=10)
        page.insert_text(
            (40, 190),
            "SPRING 2026 ACADEMIC PERIOD COURSE RECORD CONTINUES BELOW VERIFIED COPY",
            fontsize=10,
        )
        page.insert_text((40, 230), "LEFT COLUMN 3", fontsize=10)
        page.insert_text((350, 230), "RIGHT COLUMN 3", fontsize=10)
        page.insert_text(
            (40, 280),
            "UNIVERSITY OF TEST OFFICIAL TRANSCRIPT END OF RECORD VERIFIED COPY",
            fontsize=10,
        )
        document.save(wide_header)
        document.close()

        with pymupdf.open(wide_header) as document:
            header_layout = extract_page_layout(document[0])
        self.assertEqual(header_layout["column_count"], 2)
        first_column = [line["text"] for line in header_layout["columns"][0]["lines"]]
        second_column = [line["text"] for line in header_layout["columns"][1]["lines"]]
        self.assertTrue(
            any(line.startswith("UNIVERSITY OF TEST") for line in first_column)
        )
        self.assertIn("LEFT COLUMN 1", first_column)
        self.assertIn("RIGHT COLUMN 1", second_column)
        text = header_layout["text"]
        self.assertLess(text.index("UNIVERSITY OF TEST"), text.index("LEFT COLUMN 1"))
        self.assertLess(text.index("RIGHT COLUMN 2"), text.index("SPRING 2026"))
        self.assertLess(text.index("SPRING 2026"), text.index("LEFT COLUMN 3"))
        self.assertLess(text.index("RIGHT COLUMN 3"), text.index("END OF RECORD"))

    def test_structured_records_identity_transfer_and_stale_freshness(self) -> None:
        parsed = parse_transcript(self.pdf, "2026-06-15")
        self.assertEqual(
            parsed["document_identity"]["institution"], "UNIVERSITY OF TEST"
        )
        self.assertEqual(parsed["issue_date"], "2026-04-03")
        self.assertEqual(parsed["degree"]["status"], "awarded")
        self.assertEqual(parsed["freshness"]["verdict"], "stale_before_conferral")
        self.assertEqual(parsed["terms"][0]["term"], "Fall 2025")
        self.assertEqual(len(parsed["terms"][0]["courses"]), 2)
        self.assertEqual(parsed["transfer_credit"][0]["institution"], "EXAMPLE COLLEGE")
        self.assertEqual(len(parsed["transfer_credit"][0]["courses"]), 1)
        self.assertEqual(parsed["parse_quality"]["confidence"], "high")
        self.assertEqual(len(parsed["source_provenance"]["sha256"]), 64)

    def test_totals_and_provenance_keep_their_meaning(self) -> None:
        totals_pdf = self.tempdir / "totals.pdf"
        with pymupdf.open(self.pdf) as document:
            document[0].insert_text(
                (350, 300), "Cumulative Totals: Units 30", fontsize=10
            )
            document.save(totals_pdf)
        parsed = parse_transcript(totals_pdf)
        self.assertEqual(parsed["terms"][0]["totals"]["gpa"], 3.5)
        self.assertNotIn("gpa", parsed["cumulative_totals"])

        original_metadata = totals_pdf.stat()
        replacement = write_single_column(self.tempdir / "replacement.pdf")
        os.replace(replacement, totals_pdf)
        with self.assertRaisesRegex(RuntimeError, "source changed"):
            _provenance(totals_pdf, 1, original_metadata)

    def test_freshness_reports_current_and_missing_terms(self) -> None:
        current_pdf = write_transcript(
            self.tempdir / "current.pdf",
            "2026-07-01",
            [("Spring 2026", ["CS 101 Intro to Computing A 4.00 16.00"])],
        )
        current = parse_transcript(current_pdf, "2026-06-15")
        self.assertEqual(current["freshness"]["verdict"], "current")

        missing_pdf = write_transcript(
            self.tempdir / "missing-terms.pdf",
            "2026-07-01",
            [("Fall 2025", ["CS 101 Intro to Computing A 4.00 16.00"])],
        )
        missing = parse_transcript(missing_pdf, "2026-06-15")
        self.assertEqual(missing["freshness"]["verdict"], "stale_missing_terms")

        future_pdf = write_transcript(
            self.tempdir / "future-term.pdf",
            "2026-07-01",
            [("Fall 2026", ["CS 101 Intro to Computing A 4.00 16.00"])],
        )
        future = parse_transcript(future_pdf, "2026-06-15")
        self.assertEqual(future["freshness"]["verdict"], "stale_missing_terms")

        overlapping_pdf = write_transcript(
            self.tempdir / "current-and-future.pdf",
            "2026-07-01",
            [
                ("Spring 2026", ["CS 101 Intro to Computing A 4.00 16.00"]),
                ("Fall 2026", ["CS 201 Data Structures A 4.00 16.00"]),
            ],
        )
        overlapping = parse_transcript(overlapping_pdf, "2026-06-15")
        self.assertEqual(overlapping["freshness"]["verdict"], "current")

        unknown_pdf = write_single_column(self.tempdir / "unknown.pdf")
        unknown = parse_transcript(unknown_pdf, "2026-06-15")
        self.assertEqual(unknown["freshness"]["verdict"], "unknown_issue_date")

    def test_resolve_is_bounded_and_ranks_by_printed_issue_date(self) -> None:
        candidate_root = self.tempdir / "candidates"
        candidate_root.mkdir()
        old = write_transcript(
            candidate_root / "z-title-authoritative.pdf",
            "2026-04-03",
            [("Fall 2025", ["CS 101 Intro to Computing A 4.00 16.00"])],
        )
        new = write_transcript(
            candidate_root / "a-different-title.pdf",
            "2026-07-01",
            [("Spring 2026", ["CS 101 Intro to Computing A 4.00 16.00"])],
        )
        nested = candidate_root / "nested"
        nested.mkdir()
        write_transcript(nested / "not-crawled.pdf", "2026-12-01", [("Fall 2026", [])])

        rows = discover_transcripts([str(candidate_root)])
        paths = [row["path"] for row in rows]
        self.assertEqual(len(rows), 2)
        self.assertEqual(rows[0]["path"], str(new.resolve()))
        self.assertEqual(rows[1]["path"], str(old.resolve()))
        self.assertNotIn(str((nested / "not-crawled.pdf").resolve()), paths)

    def test_read_refuses_source_as_json_output(self) -> None:
        original = self.pdf.read_bytes()
        output = io.StringIO()
        with contextlib.redirect_stdout(output):
            code = cli.main(
                [
                    "--agent",
                    "transcript",
                    "read",
                    str(self.pdf),
                    "-o",
                    str(self.pdf),
                ]
            )

        self.assertEqual(code, 1)
        result = json.loads(output.getvalue())
        self.assertIn("output must differ from input", result["error"])
        self.assertEqual(self.pdf.read_bytes(), original)
        with pymupdf.open(self.pdf) as document:
            self.assertGreater(document.page_count, 0)

    def test_read_refuses_hardlink_to_source_as_json_output(self) -> None:
        original = self.pdf.read_bytes()
        hardlink = self.tempdir / "same-inode.json"
        hardlink.hardlink_to(self.pdf)
        output = io.StringIO()
        with contextlib.redirect_stdout(output):
            code = cli.main(
                [
                    "--agent",
                    "transcript",
                    "read",
                    str(self.pdf),
                    "-o",
                    str(hardlink),
                ]
            )

        self.assertEqual(code, 1)
        result = json.loads(output.getvalue())
        self.assertIn("output must differ from input", result["error"])
        self.assertEqual(self.pdf.read_bytes(), original)
        self.assertEqual(hardlink.read_bytes(), original)

    def test_ledger_does_not_store_document_text(self) -> None:
        previous_home, previous_db = cli.HOME, cli.DB_PATH
        ledger_home = self.tempdir / "ledger"
        cli.HOME = ledger_home
        cli.DB_PATH = ledger_home / "ledger.db"
        try:
            with contextlib.redirect_stdout(io.StringIO()):
                self.assertEqual(cli.main(["--agent", "text", str(self.pdf)]), 0)
            with sqlite3.connect(cli.DB_PATH) as connection:
                detail = connection.execute(
                    "SELECT detail FROM jobs ORDER BY id DESC LIMIT 1"
                ).fetchone()[0]
            self.assertNotIn("UNIVERSITY OF TEST", detail)
            self.assertNotIn("Intro to Computing", detail)
            self.assertIn("page_count", detail)
        finally:
            cli.HOME, cli.DB_PATH = previous_home, previous_db


if __name__ == "__main__":
    unittest.main()
