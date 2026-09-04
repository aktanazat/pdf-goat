from __future__ import annotations

import contextlib
import io
import json
import shutil
import sqlite3
import tempfile
import unittest
from pathlib import Path

import pymupdf

from pdf_goat import cli
from tests.fixtures import (
    jpeg_bytes,
    write_declined_image_pdf,
    write_empty_page_pdf,
    write_image_pdf,
    write_transcript,
)


class AgentToolTests(unittest.TestCase):
    def setUp(self) -> None:
        self.tempdir = Path(tempfile.mkdtemp(prefix="pdf-goat-agent-tools-"))
        self.source = write_transcript(
            self.tempdir / "source.pdf",
            "2026-04-03",
            [
                (
                    "Spring 2026",
                    [
                        "CS 101 Intro to Computing A 4.00 16.00",
                        "CS 102 Systems B 3.00 9.00",
                    ],
                )
            ],
        )
        self.previous_home = cli.HOME
        self.previous_db = cli.DB_PATH
        cli.HOME = self.tempdir / "ledger"
        cli.DB_PATH = cli.HOME / "ledger.db"

    def tearDown(self) -> None:
        cli.HOME = self.previous_home
        cli.DB_PATH = self.previous_db
        shutil.rmtree(self.tempdir)

    def run_agent(self, *arguments: str) -> dict:
        output = io.StringIO()
        with contextlib.redirect_stdout(output):
            code = cli.main(["--agent", *arguments])
        self.assertEqual(code, 0, output.getvalue())
        return json.loads(output.getvalue())

    def run_agent_error(self, *arguments: str) -> dict:
        output = io.StringIO()
        with contextlib.redirect_stdout(output):
            code = cli.main(["--agent", *arguments])
        self.assertEqual(code, 1, output.getvalue())
        return json.loads(output.getvalue())

    def test_capabilities_exposes_required_page_selection_for_extract(self) -> None:
        schema = self.run_agent("capabilities", "extract")["schemas"]["extract"]
        pages = next(argument for argument in schema["arguments"] if argument["name"] == "pages")
        self.assertEqual(
            {key: pages[key] for key in ("flags", "required", "type")},
            {"flags": ["--pages"], "required": True, "type": "string"},
        )

    def test_blank_rejects_counts_outside_the_supported_range(self) -> None:
        for count in ("0", "101"):
            with self.subTest(count=count):
                error = self.run_agent_error(
                    "pages", "blank", str(self.source), "--count", count
                )
                self.assertEqual(error["error"], "--count must be between 1 and 100")

    def test_parser_error_is_not_ledgered_or_echoed(self) -> None:
        secret = "review-secret-7Qx"
        error = self.run_agent_error("info", str(self.source), "--passwrod", secret)
        self.assertEqual(
            (error["error"], secret in json.dumps(error), cli.DB_PATH.exists()),
            ("unrecognized arguments", False, False),
        )

    def test_extract_creates_the_selected_output_without_mutating_the_source(self) -> None:
        original = self.source.read_bytes()
        output = self.tempdir / "extract.pdf"
        self.run_agent("extract", str(self.source), "--pages", "1", "-o", str(output))
        with pymupdf.open(output) as document:
            self.assertEqual((self.source.read_bytes(), document.page_count), (original, 1))

    def test_failed_extract_leaves_no_partial_file(self) -> None:
        directory = self.tempdir / "atomic"
        directory.mkdir()
        blocked = directory / "out.pdf"
        blocked.mkdir()
        self.run_agent_error("extract", str(self.source), "--pages", "1", "-o", str(blocked))
        self.assertEqual(list(directory.iterdir()), [blocked])

    def test_ledger_redacts_search_patterns(self) -> None:
        secret = "agent-secret-7Qx"
        self.run_agent(
            "redact",
            str(self.source),
            "--find",
            secret,
            "-o",
            str(self.tempdir / "redacted.pdf"),
        )
        with sqlite3.connect(cli.DB_PATH) as connection:
            stored = connection.execute(
                "SELECT inputs, outputs, detail, message FROM jobs WHERE verb = 'redact'"
            ).fetchone()
        self.assertEqual((stored is not None, secret in json.dumps(stored)), (True, False))

    def test_render_writes_png_bytes(self) -> None:
        rendered = self.run_agent(
            "render",
            str(self.source),
            "--dpi",
            "72",
            "--format",
            "png",
            "-o",
            str(self.tempdir / "render"),
        )
        self.assertEqual(
            Path(rendered["outputs"][0]).read_bytes()[:8], b"\x89PNG\r\n\x1a\n"
        )

    def test_image_extraction_preserves_stored_jpeg_bytes(self) -> None:
        rgb = jpeg_bytes("RGB", (32, 24))
        cmyk = jpeg_bytes("CMYK", (16, 12))
        source = write_image_pdf(self.tempdir / "images.pdf", rgb, cmyk)
        extracted = self.run_agent(
            "get", "images", str(source), "-o", str(self.tempdir / "images")
        )
        self.assertEqual(
            {Path(path).read_bytes() for path in extracted["outputs"]}, {rgb, cmyk}
        )

    def test_image_extraction_falls_back_when_pikepdf_declines(self) -> None:
        source = write_declined_image_pdf(self.tempdir / "declined.pdf")
        extracted = self.run_agent(
            "get", "images", str(source), "-o", str(self.tempdir / "declined-images")
        )
        self.assertEqual(
            (
                extracted["count"],
                [Path(path).stat().st_size > 0 for path in extracted["outputs"]],
            ),
            (2, [True, True]),
        )

    def test_search_marks_limited_results_as_truncated(self) -> None:
        result = self.run_agent("search", str(self.source), "CS", "--limit", "1")
        self.assertEqual((result["count"], result["truncated"]), (1, True))

    def test_count_matches_mupdf_text_and_word_boxes(self) -> None:
        with pymupdf.open(self.source) as document:
            expected = (
                document.page_count,
                sum(len(page.get_text("words")) for page in document),
                sum(len(page.get_text("text")) for page in document),
            )
        counted = self.run_agent("count", str(self.source))
        self.assertEqual(
            (counted["pages"], counted["words"], counted["chars"]), expected
        )

    def test_preflight_marks_only_resource_free_pages_empty(self) -> None:
        source = write_empty_page_pdf(self.tempdir / "empty-pages.pdf")
        preflight = self.run_agent("preflight", str(source))
        self.assertEqual(
            {
                finding["code"]: finding["pages"]
                for finding in preflight["findings"]
                if finding["code"] == "empty_pages"
            },
            {"empty_pages": [1]},
        )

    def test_text_output_streams_page_text_to_file(self) -> None:
        output = self.tempdir / "text.txt"
        with pymupdf.open(self.source) as document:
            expected = document[0].get_text("text")
        written = self.run_agent("text", str(self.source), "-o", str(output))
        self.assertEqual(
            (
                output.read_text(),
                written["page_count"],
                written["outputs"],
                "pages" in written,
            ),
            (expected, 1, [str(output.resolve())], False),
        )

    def test_in_place_compression_reports_size_on_disk(self) -> None:
        target = self.tempdir / "inplace.pdf"
        shutil.copyfile(self.source, target)
        compressed = self.run_agent("compress", str(target), "-o", str(target))
        self.assertEqual(
            (Path(compressed["outputs"][0]), compressed["compressed_bytes"]),
            (target.resolve(), target.stat().st_size),
        )
