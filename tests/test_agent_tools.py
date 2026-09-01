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


class AgentToolTests(unittest.TestCase):
    def setUp(self) -> None:
        self.tempdir = Path(tempfile.mkdtemp(prefix="pdf-goat-agent-tools-"))
        self.source = self.tempdir / "source.pdf"
        document = pymupdf.open()
        for number in range(1, 3):
            page = document.new_page(width=400, height=500)
            page.insert_text((50, 50), f"Page {number} agent text")
        document[0].insert_link(
            {
                "kind": pymupdf.LINK_URI,
                "from": pymupdf.Rect(50, 70, 180, 90),
                "uri": "https://example.com",
            }
        )
        document[0].insert_link(
            {
                "kind": pymupdf.LINK_GOTO,
                "from": pymupdf.Rect(50, 100, 180, 120),
                "page": 1,
            }
        )
        document.embfile_add("note.txt", b"embedded note")
        attachment = document[0].add_file_annot(
            pymupdf.Point(220, 70), b"annotation note", "annotation.txt"
        )
        attachment.update()
        document.set_toc([[1, "Start", 1]])
        document.save(self.source)
        document.close()

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
        result = json.loads(output.getvalue())
        self.assertTrue(result["ok"])
        return result

    def run_agent_error(self, *arguments: str) -> dict:
        output = io.StringIO()
        with contextlib.redirect_stdout(output):
            code = cli.main(["--agent", *arguments])
        self.assertEqual(code, 1, output.getvalue())
        result = json.loads(output.getvalue())
        self.assertFalse(result["ok"])
        return result

    def file_attachment_count(self, path: Path) -> int:
        with pymupdf.open(path) as document:
            return sum(
                1
                for page in document
                for annotation in (page.annots() or ())
                if annotation.type[0] == pymupdf.PDF_ANNOT_FILE_ATTACHMENT
            )

    def test_discovery_and_observation_are_bounded(self) -> None:
        capabilities = self.run_agent("capabilities")
        self.assertGreaterEqual(capabilities["command_count"], 90)
        self.assertIn("pages", capabilities["families"])
        self.assertIn("inspect", capabilities["commands"])
        root_flags = {
            flag
            for argument in capabilities["root_arguments"]
            for flag in argument["flags"]
        }
        self.assertIn("--agent", root_flags)
        self.assertFalse(cli.DB_PATH.exists())

        page_capabilities = self.run_agent("capabilities", "pages")
        page_commands = page_capabilities["schemas"]["pages"]["commands"]
        self.assertIn("blank", page_commands)
        self.assertIn("duplicate", page_commands)

        detach_capabilities = self.run_agent("capabilities", "detach")
        detach_schema = detach_capabilities["schemas"]["detach"]
        name_argument = next(
            argument
            for argument in detach_schema["arguments"]
            if argument["name"] == "names"
        )
        self.assertTrue(name_argument["repeatable"])
        self.assertIn(
            {"required": True, "arguments": ["names", "remove_all"]},
            detach_schema["mutually_exclusive_groups"],
        )

        inspection = self.run_agent("inspect", str(self.source), "--limit", "1")
        self.assertEqual(inspection["next_page"], 2)
        self.assertTrue(inspection["truncated"])
        self.assertEqual(inspection["pages"][0]["link_count"], 2)

        preflight = self.run_agent("preflight", str(self.source))
        self.assertEqual(preflight["attachments"], 2)
        self.assertEqual(preflight["external_links"], 1)
        self.assertEqual(preflight["risk"], "medium")

        attachment_dir = self.tempdir / "extracted-attachments"
        attachments = self.run_agent(
            "get", "attachments", str(self.source), "-o", str(attachment_dir)
        )
        self.assertEqual(attachments["count"], 2)
        self.assertEqual(
            {path.name for path in attachment_dir.iterdir()},
            {"note.txt", "annotation.txt"},
        )
        self.assertEqual((attachment_dir / "note.txt").read_bytes(), b"embedded note")
        self.assertEqual(
            (attachment_dir / "annotation.txt").read_bytes(), b"annotation note"
        )

        blocks = self.run_agent(
            "get", "text-blocks", str(self.source), "--max-blocks", "1"
        )
        self.assertEqual(blocks["count"], 1)
        self.assertTrue(blocks["block_truncated"])
        self.assertFalse(blocks["page_truncated"])
        self.assertIn("Page 1 agent text", blocks["blocks"][0]["text"])
        self.assertEqual(blocks["next_block"], 1)
        continued = self.run_agent(
            "get",
            "text-blocks",
            str(self.source),
            "--start-block",
            "1",
            "--max-blocks",
            "1",
        )
        self.assertEqual(continued["start_block"], 1)
        self.assertFalse(continued["block_truncated"])
        self.assertIsNone(continued["next_block"])
        self.assertIn("Page 2 agent text", continued["blocks"][0]["text"])
        error = self.run_agent_error(
            "get", "text-blocks", str(self.source), "--start-block", "-1"
        )
        self.assertIn("--start-block", error["error"])

        with sqlite3.connect(cli.DB_PATH) as connection:
            rows = connection.execute("SELECT verb, detail FROM jobs").fetchall()
        details = {verb: json.loads(detail) for verb, detail in rows}
        self.assertEqual(details["inspect"]["total_pages"], 2)
        self.assertEqual(details["inspect"]["pages_count"], 1)
        self.assertNotIn("page_count", details["inspect"])
        self.assertEqual(details["preflight"]["risk"], "medium")
        self.assertEqual(details["preflight"]["attachments"], 2)
        text_detail = details["get-text-blocks"]
        self.assertNotIn("Page 1 agent text", json.dumps(text_detail))
        self.assertEqual(text_detail["blocks_count"], 1)
        self.assertEqual(text_detail["total_pages"], 2)

        long_pdf = self.tempdir / "long.pdf"
        document = pymupdf.open()
        for page_number in range(26):
            page = document.new_page()
            page.insert_text((50, 50), f"Page {page_number + 1}")
        document.save(long_pdf)
        document.close()
        long_blocks = self.run_agent(
            "get", "text-blocks", str(long_pdf), "--max-blocks", "100"
        )
        self.assertEqual(long_blocks["total_pages"], 26)
        self.assertEqual(long_blocks["count"], 25)
        self.assertTrue(long_blocks["page_truncated"])
        self.assertFalse(long_blocks["block_truncated"])
        self.assertEqual(long_blocks["next_page"], 26)
        self.assertTrue(long_blocks["truncated"])
        first_long_block = self.run_agent(
            "get", "text-blocks", str(long_pdf), "--max-blocks", "1"
        )
        self.assertEqual(first_long_block["next_block"], 1)
        self.assertIsNone(first_long_block["next_page"])
        end_of_window = self.run_agent(
            "get",
            "text-blocks",
            str(long_pdf),
            "--start-block",
            "24",
            "--max-blocks",
            "1",
        )
        self.assertFalse(end_of_window["block_truncated"])
        self.assertEqual(end_of_window["next_page"], 26)

    def test_page_and_region_render_tools_write_valid_outputs(self) -> None:
        blank = self.tempdir / "blank.pdf"
        self.run_agent(
            "pages",
            "blank",
            str(self.source),
            "--at",
            "2",
            "--count",
            "2",
            "-o",
            str(blank),
        )
        with pymupdf.open(blank) as document:
            self.assertEqual(document.page_count, 4)
            self.assertEqual(document[1].rect, document[0].rect)

        duplicate = self.tempdir / "duplicate.pdf"
        self.run_agent(
            "pages",
            "duplicate",
            str(self.source),
            "--pages",
            "1",
            "-o",
            str(duplicate),
        )
        with pymupdf.open(duplicate) as document:
            self.assertEqual(document.page_count, 3)
            self.assertEqual(document[0].get_text(), document[1].get_text())

        render_dir = self.tempdir / "render"
        rendered = self.run_agent(
            "render",
            str(self.source),
            "--pages",
            "1",
            "--dpi",
            "72",
            "--clip",
            "200,250,0,0",
            "-o",
            str(render_dir),
        )
        pixmap = pymupdf.Pixmap(rendered["outputs"][0])
        self.assertEqual((pixmap.width, pixmap.height), (200, 250))

        mixed = self.tempdir / "mixed-page-sizes.pdf"
        document = pymupdf.open()
        document.new_page(width=400, height=500)
        document.new_page(width=100, height=100)
        document.save(mixed)
        document.close()
        failed_dir = self.tempdir / "failed-render"
        error = self.run_agent_error(
            "render",
            str(mixed),
            "--pages",
            "1-2",
            "--dpi",
            "72",
            "--clip",
            "200,200,300,300",
            "-o",
            str(failed_dir),
        )
        self.assertIn("page 2", error["error"])
        self.assertFalse(failed_dir.exists())

    def test_review_and_navigation_tools_preserve_unselected_content(self) -> None:
        area = self.tempdir / "area.pdf"
        self.run_agent(
            "annotate",
            "area-highlight",
            str(self.source),
            "--rect",
            "40,40,200,80",
            "-o",
            str(area),
        )
        polygon = self.tempdir / "polygon.pdf"
        self.run_agent(
            "annotate",
            "polygon",
            str(self.source),
            "--points",
            "40,150;100,120;160,150",
            "-o",
            str(polygon),
        )
        for path in (area, polygon):
            with pymupdf.open(path) as document:
                page = document[0]
                self.assertEqual(sum(1 for _ in (page.annots() or ())), 2)

        launch_link = self.tempdir / "launch-link.pdf"
        with pymupdf.open(self.source) as document:
            document[0].insert_link(
                {
                    "kind": pymupdf.LINK_LAUNCH,
                    "from": pymupdf.Rect(50, 130, 180, 150),
                    "file": "/Applications/TextEdit.app",
                }
            )
            document.save(launch_link)

        links = self.tempdir / "links.pdf"
        self.run_agent(
            "links", "remove", str(launch_link), "--external-only", "-o", str(links)
        )
        with pymupdf.open(links) as document:
            remaining = document[0].get_links()
            self.assertEqual(len(remaining), 1)
            self.assertEqual(remaining[0]["kind"], pymupdf.LINK_GOTO)

        unbookmarked = self.tempdir / "unbookmarked.pdf"
        self.run_agent("bookmarks", "clear", str(self.source), "-o", str(unbookmarked))
        with pymupdf.open(unbookmarked) as document:
            self.assertEqual(document.get_toc(), [])

        detached = self.tempdir / "detached.pdf"
        result = self.run_agent(
            "detach",
            str(self.source),
            "--name",
            "note.txt",
            "--name",
            "note.txt",
            "-o",
            str(detached),
        )
        self.assertEqual(result["removed"], ["note.txt"])
        self.assertEqual(result["count"], 1)
        with pymupdf.open(detached) as document:
            self.assertEqual(document.embfile_names(), [])
        self.assertEqual(self.file_attachment_count(detached), 1)

        detached_all = self.tempdir / "detached-all.pdf"
        result = self.run_agent(
            "detach", str(self.source), "--all", "-o", str(detached_all)
        )
        self.assertEqual(set(result["removed"]), {"note.txt", "annotation.txt"})
        self.assertEqual(result["count"], 2)
        with pymupdf.open(detached_all) as document:
            self.assertEqual(document.embfile_names(), [])
        self.assertEqual(self.file_attachment_count(detached_all), 0)

    def test_parser_errors_do_not_enter_ledger(self) -> None:
        error = self.run_agent_error(
            "info", str(self.source), "--passwrod", "review-secret-7Qx"
        )
        self.assertIn("unrecognized arguments", error["error"])
        self.assertEqual(error["verb"], "pdf-goat")
        self.assertNotIn("review-secret-7Qx", json.dumps(error))

        invalid_value = "value-secret-2Vn"
        error = self.run_agent_error(
            "pages", "blank", str(self.source), "--count", invalid_value
        )
        self.assertIn("--count", error["error"])
        self.assertNotIn(invalid_value, json.dumps(error))
        self.assertFalse(cli.DB_PATH.exists())

    def test_ledger_stores_text_lengths_not_text(self) -> None:
        output = self.tempdir / "safe.pdf"
        pattern = "Page"
        self.run_agent("redact", str(self.source), "--find", pattern, "-o", str(output))

        with sqlite3.connect(cli.DB_PATH) as connection:
            inputs, outputs, raw_detail, message = connection.execute(
                "SELECT inputs, outputs, detail, message FROM jobs WHERE verb = 'redact'"
            ).fetchone()
        self.assertNotIn(pattern, json.dumps([inputs, outputs, raw_detail, message]))
        detail = json.loads(raw_detail)
        self.assertEqual(detail["pattern_char_count"], len(pattern))

    def test_invalid_requests_do_not_change_inputs_or_outputs(self) -> None:
        original = self.source.read_bytes()
        blank = self.tempdir / "invalid-blank.pdf"
        error = self.run_agent_error(
            "pages", "blank", str(self.source), "--count", "0", "-o", str(blank)
        )
        self.assertIn("--count", error["error"])
        self.assertFalse(blank.exists())

        zero_at = self.tempdir / "zero-at.pdf"
        error = self.run_agent_error(
            "pages", "blank", str(self.source), "--at", "0", "-o", str(zero_at)
        )
        self.assertIn("--at", error["error"])
        self.assertFalse(zero_at.exists())

        invalid_opacity = self.tempdir / "invalid-opacity.pdf"
        error = self.run_agent_error(
            "annotate",
            "area-highlight",
            str(self.source),
            "--rect",
            "40,40,200,80",
            "--opacity",
            "1.5",
            "-o",
            str(invalid_opacity),
        )
        self.assertIn("--opacity", error["error"])
        self.assertFalse(invalid_opacity.exists())

        invalid_page = self.tempdir / "invalid-page.pdf"
        error = self.run_agent_error(
            "annotate",
            "area-highlight",
            str(self.source),
            "--page",
            "0",
            "--rect",
            "40,40,200,80",
            "-o",
            str(invalid_page),
        )
        self.assertIn("--page", error["error"])
        self.assertFalse(invalid_page.exists())
        invalid_polygon = self.tempdir / "invalid-polygon.pdf"
        error = self.run_agent_error(
            "annotate",
            "polygon",
            str(self.source),
            "--page",
            "0",
            "--points",
            "20,20;80,20;50,80",
            "-o",
            str(invalid_polygon),
        )
        self.assertIn("--page", error["error"])
        self.assertFalse(invalid_polygon.exists())

        error = self.run_agent_error("inspect", str(self.source), "--start-page", "3")
        self.assertIn("between 1 and 2", error["error"])

        detached = self.tempdir / "missing-attachment.pdf"
        self.run_agent_error(
            "detach", str(self.source), "--name", "missing.txt", "-o", str(detached)
        )
        self.assertFalse(detached.exists())

        no_choice = self.tempdir / "no-choice.pdf"
        error = self.run_agent_error("detach", str(self.source), "-o", str(no_choice))
        self.assertIn("required", error["error"])
        self.assertFalse(no_choice.exists())

        both_choices = self.tempdir / "both-choices.pdf"
        error = self.run_agent_error(
            "detach",
            str(self.source),
            "--name",
            "note.txt",
            "--all",
            "-o",
            str(both_choices),
        )
        self.assertIn("not allowed", error["error"])
        self.assertFalse(both_choices.exists())

        attachment_dir = self.tempdir / "attachments"
        attachment_dir.mkdir()
        existing = attachment_dir / "annotation.txt"
        existing.write_text("keep me")
        self.run_agent_error(
            "get", "attachments", str(self.source), "-o", str(attachment_dir)
        )
        self.assertEqual(existing.read_text(), "keep me")
        self.assertEqual(list(attachment_dir.iterdir()), [existing])

        duplicate_source = self.tempdir / "duplicate-attachment-name.pdf"
        document = pymupdf.open()
        page = document.new_page()
        document.embfile_add("same.txt", b"catalog")
        attachment = page.add_file_annot(
            pymupdf.Point(50, 50), b"annotation", "same.txt"
        )
        attachment.update()
        document.save(duplicate_source)
        document.close()
        duplicate_dir = self.tempdir / "duplicate-attachment-output"
        self.run_agent_error(
            "get", "attachments", str(duplicate_source), "-o", str(duplicate_dir)
        )
        self.assertFalse(duplicate_dir.exists())
        casefold_source = self.tempdir / "casefold-attachment-name.pdf"
        document = pymupdf.open()
        page = document.new_page()
        document.embfile_add("A.txt", b"catalog")
        attachment = page.add_file_annot(pymupdf.Point(50, 50), b"annotation", "a.txt")
        attachment.update()
        document.save(casefold_source)
        document.close()
        casefold_dir = self.tempdir / "casefold-attachment-output"
        self.run_agent_error(
            "get", "attachments", str(casefold_source), "-o", str(casefold_dir)
        )
        self.assertFalse(casefold_dir.exists())

        symlink_source = self.tempdir / "symlink-attachment.pdf"
        document = pymupdf.open()
        document.new_page()
        document.embfile_add("leak.txt", b"not outside")
        document.save(symlink_source)
        document.close()
        symlink_dir = self.tempdir / "symlink-attachment-output"
        symlink_dir.mkdir()
        outside = self.tempdir / "outside.txt"
        dangling = symlink_dir / "leak.txt"
        dangling.symlink_to(outside)
        self.run_agent_error(
            "get", "attachments", str(symlink_source), "-o", str(symlink_dir)
        )
        self.assertTrue(dangling.is_symlink())
        self.assertFalse(outside.exists())

        self.assertEqual(self.source.read_bytes(), original)

        unsafe = self.tempdir / "unsafe.pdf"
        with pymupdf.open(self.source) as document:
            document[0].insert_link(
                {
                    "kind": pymupdf.LINK_LAUNCH,
                    "from": pymupdf.Rect(50, 130, 180, 150),
                    "file": "/Applications/TextEdit.app",
                }
            )
            document.save(unsafe)
        preflight = self.run_agent("preflight", str(unsafe))
        self.assertEqual(preflight["risk"], "high")
        self.assertEqual(preflight["unsafe_links"], 1)
        self.assertEqual(preflight["launch_actions"], 1)


if __name__ == "__main__":
    unittest.main()
