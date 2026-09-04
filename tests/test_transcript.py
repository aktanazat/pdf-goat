from __future__ import annotations

import contextlib
import hashlib
import io
import json
import tempfile
import unittest
from datetime import datetime, timezone
from pathlib import Path

from pdf_goat import cli
from pdf_goat.transcript import parse_transcript
from tests.fixtures import write_single_column, write_transcript


class TranscriptContractTests(unittest.TestCase):
    CONFERRAL_DATE = "2026-06-15"

    def setUp(self) -> None:
        self.tempdir = tempfile.TemporaryDirectory(prefix="pdf-goat-tests-")
        self.addCleanup(self.tempdir.cleanup)
        self.workdir = Path(self.tempdir.name)
        self.transcript = write_transcript(
            self.workdir / "transcript.pdf",
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

    def run_agent(self, *arguments: str) -> dict:
        output = io.StringIO()
        with contextlib.redirect_stdout(output):
            code = cli.main(["--agent", *arguments])
        self.assertEqual(code, 0, output.getvalue())
        result = json.loads(output.getvalue())
        self.assertIs(result["ok"], True)
        return result

    def run_agent_error(self, *arguments: str) -> dict:
        output = io.StringIO()
        with contextlib.redirect_stdout(output):
            code = cli.main(["--agent", *arguments])
        self.assertEqual(code, 1, output.getvalue())
        result = json.loads(output.getvalue())
        self.assertIs(result["ok"], False)
        return result

    def test_parse_transcript_reads_columns_in_page_order(self) -> None:
        parsed = parse_transcript(self.transcript)
        reading_order = [
            line["text"] for line in parsed["layout"]["pages"][0]["reading_order"]
        ]

        self.assertLess(
            reading_order.index("Degree Awarded: 2026-06-15"),
            reading_order.index("Fall 2025"),
        )
        self.assertLess(
            reading_order.index("Fall 2025"),
            reading_order.index("CS 101 Intro to Computing A 4.00 16.00"),
        )
        self.assertLess(
            reading_order.index("CS 101 Intro to Computing A 4.00 16.00"),
            reading_order.index("MATH 201 Discrete Math B+ 3.00 9.99"),
        )

    def test_parse_transcript_extracts_document_identity(self) -> None:
        parsed = parse_transcript(self.transcript)

        self.assertEqual(
            parsed["document_identity"],
            {
                "document_type": "academic_transcript",
                "title": "OFFICIAL ACADEMIC TRANSCRIPT",
                "institution": "UNIVERSITY OF TEST",
                "student_name_present": True,
                "student_identifier_present": False,
            },
        )
        self.assertEqual(
            parsed["degree"],
            {
                "name": "Master of Science",
                "status": "awarded",
                "conferral_date": self.CONFERRAL_DATE,
            },
        )

    def test_parse_transcript_extracts_transfer_credit(self) -> None:
        parsed = parse_transcript(self.transcript)

        self.assertEqual(
            [
                (
                    block["institution"],
                    [
                        (course["course"], course["title"], course["grade"])
                        for course in block["courses"]
                    ],
                )
                for block in parsed["transfer_credit"]
            ],
            [("EXAMPLE COLLEGE", [("HIST 100", "World History", "B")])],
        )

    def test_parse_transcript_reports_freshness(self) -> None:
        current = write_transcript(
            self.workdir / "current.pdf",
            "2026-07-01",
            [("Spring 2026", ["CS 101 Intro to Computing A 4.00 16.00"])],
        )
        missing_terms = write_transcript(
            self.workdir / "missing-terms.pdf",
            "2026-07-01",
            [("Fall 2025", ["CS 101 Intro to Computing A 4.00 16.00"])],
        )
        unknown_issue_date = write_single_column(self.workdir / "unknown.pdf")
        cases = [
            ("no asserted conferral", self.transcript, None, "not_checked"),
            (
                "issue before conferral",
                self.transcript,
                self.CONFERRAL_DATE,
                "stale_before_conferral",
            ),
            ("current", current, self.CONFERRAL_DATE, "current"),
            (
                "term does not cover conferral",
                missing_terms,
                self.CONFERRAL_DATE,
                "stale_missing_terms",
            ),
            (
                "no printed issue date",
                unknown_issue_date,
                self.CONFERRAL_DATE,
                "unknown_issue_date",
            ),
        ]
        for case, source, conferred, expected_verdict in cases:
            with self.subTest(case=case):
                self.assertEqual(
                    parse_transcript(source, conferred)["freshness"]["verdict"],
                    expected_verdict,
                )

    def test_parse_transcript_reports_source_provenance(self) -> None:
        source_bytes = self.transcript.read_bytes()
        provenance = parse_transcript(self.transcript)["source_provenance"]

        self.assertEqual(provenance["path"], str(self.transcript.resolve()))
        self.assertEqual(provenance["sha256"], hashlib.sha256(source_bytes).hexdigest())
        self.assertEqual(provenance["byte_size"], len(source_bytes))
        self.assertEqual(provenance["page_count"], 1)
        self.assertEqual(
            datetime.fromisoformat(provenance["modified_time"]),
            datetime.fromtimestamp(self.transcript.stat().st_mtime, timezone.utc),
        )

    def test_transcript_resolve_returns_bounded_ranked_candidates(self) -> None:
        candidates = self.workdir / "candidates"
        candidates.mkdir()
        older = write_transcript(
            candidates / "z-authoritative-title.pdf",
            "2026-04-03",
            [("Fall 2025", ["CS 101 Intro to Computing A 4.00 16.00"])],
        )
        newer = write_transcript(
            candidates / "a-different-title.pdf",
            "2026-07-01",
            [("Spring 2026", ["CS 101 Intro to Computing A 4.00 16.00"])],
        )
        nested = candidates / "nested"
        nested.mkdir()
        not_crawled = write_transcript(
            nested / "not-crawled.pdf", "2026-12-01", [("Fall 2026", [])]
        )

        result = self.run_agent("transcript", "resolve", "--root", str(candidates))
        rows = result["candidates"]
        self.assertEqual(result["candidate_count"], 2)
        self.assertEqual(
            [(row["rank"], row["path"], row["issue_date"]) for row in rows],
            [
                (1, str(newer.resolve()), "2026-07-01"),
                (2, str(older.resolve()), "2026-04-03"),
            ],
        )
        self.assertNotIn(str(not_crawled.resolve()), [row["path"] for row in rows])

    def test_transcript_read_exports_json(self) -> None:
        export = self.workdir / "transcript.json"
        result = self.run_agent(
            "transcript",
            "read",
            str(self.transcript),
            "--conferred",
            self.CONFERRAL_DATE,
            "--output",
            str(export),
        )
        exported = json.loads(export.read_text())
        expected = {
            key: value
            for key, value in result.items()
            if key not in {"verb", "inputs", "outputs", "ok"}
        }

        self.assertEqual(result["verb"], "transcript-read")
        self.assertEqual(result["outputs"], [str(export.resolve())])
        self.assertEqual(exported, expected)
        self.assertNotIn("layout", exported)

    def test_transcript_read_rejects_source_aliases(self) -> None:
        original = self.transcript.read_bytes()
        hardlink = self.workdir / "same-inode.json"

        for case, output in (("same path", self.transcript), ("hard link", hardlink)):
            with self.subTest(case=case):
                if output != self.transcript:
                    output.hardlink_to(self.transcript)
                error = self.run_agent_error(
                    "transcript", "read", str(self.transcript), "--output", str(output)
                )
                self.assertEqual(error["error"], "output must differ from input")
                self.assertEqual(self.transcript.read_bytes(), original)


if __name__ == "__main__":
    unittest.main()
