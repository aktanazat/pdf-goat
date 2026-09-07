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

from pdf_goat import cli, textcache
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
        self.previous_home = cli.HOME
        self.previous_db = cli.DB_PATH
        self.previous_cache_mb = os.environ.get("PDF_GOAT_CACHE_MB")
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
        cli.HOME = self.tempdir / "ledger"
        cli.DB_PATH = cli.HOME / "ledger.db"

    def tearDown(self) -> None:
        cli.HOME = self.previous_home
        cli.DB_PATH = self.previous_db
        if self.previous_cache_mb is None:
            os.environ.pop("PDF_GOAT_CACHE_MB", None)
        else:
            os.environ["PDF_GOAT_CACHE_MB"] = self.previous_cache_mb
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
        pages = next(
            argument for argument in schema["arguments"] if argument["name"] == "pages"
        )
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

    def test_encrypt_uses_the_user_password_when_owner_is_omitted(self) -> None:
        output = self.tempdir / "encrypted.pdf"
        receipt = self.run_agent(
            "security",
            "encrypt",
            str(self.source),
            "--password",
            "test-password",
            "-o",
            str(output),
        )
        with pymupdf.open(output) as document:
            needs_password = document.needs_pass
            authenticated = document.authenticate("test-password")
        self.assertEqual(receipt["outputs"], [str(output.resolve())])
        self.assertTrue(needs_password)
        self.assertGreater(authenticated, 0)

    def test_extract_creates_the_selected_output_without_mutating_the_source(
        self,
    ) -> None:
        original = self.source.read_bytes()
        output = self.tempdir / "extract.pdf"
        self.run_agent("extract", str(self.source), "--pages", "1", "-o", str(output))
        with pymupdf.open(output) as document:
            self.assertEqual(
                (self.source.read_bytes(), document.page_count), (original, 1)
            )

    def test_failed_extract_leaves_no_partial_file(self) -> None:
        directory = self.tempdir / "atomic"
        directory.mkdir()
        blocked = directory / "out.pdf"
        blocked.mkdir()
        self.run_agent_error(
            "extract", str(self.source), "--pages", "1", "-o", str(blocked)
        )
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
        self.assertEqual(
            (stored is not None, secret in json.dumps(stored)), (True, False)
        )

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

    def test_search_and_redact_share_case_insensitive_word_hits(self) -> None:
        source = self.tempdir / "mixed-case.pdf"
        document = pymupdf.open()
        page = document.new_page()
        page.insert_text(
            (72, 72),
            "Commission COMMISSION commission Commissioner unrelated",
        )
        document.save(source)
        document.close()

        before = self.run_agent("search", str(source), "Commission")
        self.assertEqual((before["count"], before["truncated"]), (4, False))

        output = self.tempdir / "mixed-case-redacted.pdf"
        receipt = self.run_agent(
            "redact", str(source), "--find", "Commission", "-o", str(output)
        )
        after = self.run_agent("search", str(output), "Commission")
        with pymupdf.open(output) as redacted:
            text = "\n".join(page.get_text() for page in redacted)
        self.assertEqual((after["count"], after["truncated"]), (0, False))
        self.assertEqual(receipt["redactions"], before["count"])
        self.assertNotIn("commission", text.lower())
        self.assertIn("unrelated", text)

    def test_search_matches_a_phrase_across_block_lines(self) -> None:
        source = self.tempdir / "line-break.pdf"
        document = pymupdf.open()
        page = document.new_page()
        page.insert_textbox(pymupdf.Rect(72, 60, 250, 100), "New\nYork", fontsize=10)
        document.save(source)
        document.close()

        result = self.run_agent("search", str(source), "New York")
        rectangles = [hit["rect"] for hit in result["hits"]]
        self.assertEqual(result["count"], 2)
        self.assertEqual(len({rectangle[1] for rectangle in rectangles}), 2)

    def write_cache_fixture(self) -> Path:
        source = self.tempdir / "cache-fixture.pdf"
        document = pymupdf.open()
        page = document.new_page()
        writer = pymupdf.TextWriter(page.rect)
        writer.append((72, 72), "De\ufb01ning the \ufb02ow", font=pymupdf.Font("helv"))
        writer.write_text(page)
        page = document.new_page()
        page.insert_textbox(pymupdf.Rect(72, 60, 250, 100), "New\nYork", fontsize=10)
        page = document.new_page()
        page.insert_text((72, 72), "cache page three")
        document.save(source)
        document.close()
        return source

    def write_cache_document(self, name: str, pages: list[str]) -> Path:
        source = self.tempdir / name
        document = pymupdf.open()
        for text in pages:
            page = document.new_page()
            page.insert_text((72, 72), text)
        document.save(source)
        document.close()
        return source

    def test_text_cache_matches_cold_warm_and_no_cache_receipts(self) -> None:
        source = self.write_cache_fixture()
        text_output = self.tempdir / "cache-text.txt"
        cases = (
            ("text-json", ("text", str(source))),
            ("text-file", ("text", str(source), "-o", str(text_output))),
            ("count", ("count", str(source))),
            ("search-phrase", ("search", str(source), "New York")),
            ("search-ligature", ("search", str(source), "Defining")),
            ("compare", ("compare", "text", str(source), str(source))),
        )
        cold = {}
        cold_file = None
        for name, arguments in cases:
            cold[name] = self.run_agent(*arguments)
            if name == "text-file":
                cold_file = text_output.read_bytes()

        warm = {name: self.run_agent(*arguments) for name, arguments in cases}
        warm_file = text_output.read_bytes()
        self.assertEqual(cold, warm)
        self.assertEqual(cold_file, warm_file)

        with sqlite3.connect(cli.HOME / "cache.sqlite") as connection:
            before_rows = connection.execute("SELECT count(*) FROM pages").fetchone()[0]
        uncached = {
            name: self.run_agent(*arguments, "--no-cache") for name, arguments in cases
        }
        uncached_file = text_output.read_bytes()
        with sqlite3.connect(cli.HOME / "cache.sqlite") as connection:
            after_rows = connection.execute("SELECT count(*) FROM pages").fetchone()[0]
        for name, expected in cold.items():
            with self.subTest(case=name):
                self.assertEqual(expected, uncached[name])
        self.assertEqual(
            (cold_file, warm_file, uncached_file), (cold_file, cold_file, cold_file)
        )
        self.assertEqual(after_rows, before_rows)
        self.assertGreater(before_rows, 0)

    def test_text_cache_serves_stored_text_until_no_cache_bypasses_it(self) -> None:
        source = self.write_cache_fixture()
        live = self.run_agent("text", str(source))

        with sqlite3.connect(cli.HOME / "cache.sqlite") as connection:
            connection.execute(
                "UPDATE pages SET text_value=? WHERE form='text'", ("cached sentinel",)
            )

        poisoned = self.run_agent("text", str(source))
        self.assertEqual(
            {page["text"] for page in poisoned["pages"]}, {"cached sentinel"}
        )
        self.assertEqual(self.run_agent("text", str(source), "--no-cache"), live)

    def test_text_cache_rejects_a_page_count_its_own_rows_contradict(self) -> None:
        source = self.write_cache_document(
            "short-count.pdf", ["page one", "page two", "page three target"]
        )
        live_text = self.run_agent("text", str(source))
        live_hit = self.run_agent("search", str(source), "target", "--limit", "1")

        with sqlite3.connect(cli.HOME / "cache.sqlite") as connection:
            connection.execute("UPDATE documents SET page_count=1")

        self.assertEqual(self.run_agent("text", str(source)), live_text)
        self.assertEqual(
            self.run_agent("search", str(source), "target", "--limit", "1"), live_hit
        )

    def test_cache_discards_a_page_count_the_file_cannot_hold(self) -> None:
        source = self.write_cache_document("huge-count.pdf", ["only page"])
        self.run_agent("text", str(source))

        with sqlite3.connect(cli.HOME / "cache.sqlite") as connection:
            connection.execute(
                "UPDATE documents SET page_count=?", (source.stat().st_size + 1,)
            )

        cache = textcache.Cache(cli.HOME / "cache.sqlite")
        self.addCleanup(cache.close)
        self.assertIsNone(cache.document(textcache.document_key(source)))

    def test_cache_reprimes_after_a_page_index_outside_the_document(self) -> None:
        source = self.write_cache_document("stray-index.pdf", ["only page"])
        live = self.run_agent("text", str(source))

        with sqlite3.connect(cli.HOME / "cache.sqlite") as connection:
            connection.execute("UPDATE pages SET page_index=9999 WHERE form='text'")
        self.assertEqual(self.run_agent("text", str(source)), live)

        with sqlite3.connect(cli.HOME / "cache.sqlite") as connection:
            connection.execute(
                "UPDATE pages SET text_value=? WHERE form='text'", ("cached sentinel",)
            )
        reprimed = self.run_agent("text", str(source))
        self.assertEqual(
            {page["text"] for page in reprimed["pages"]}, {"cached sentinel"}
        )

    def test_text_cache_digest_detects_same_size_mtime_edit(self) -> None:
        source = self.tempdir / "digest.pdf"
        document = pymupdf.open()
        page = document.new_page()
        page.insert_text((72, 72), "OLDWORD")
        document.save(source, deflate=False)
        document.close()

        self.assertEqual(self.run_agent("search", str(source), "OLDWORD")["count"], 1)
        before = source.stat()
        payload = source.read_bytes()
        self.assertIn(b"4f4c44574f5244", payload)
        source.write_bytes(payload.replace(b"4f4c44574f5244", b"4e4557574f5244", 1))
        os.utime(source, ns=(before.st_atime_ns, before.st_mtime_ns))
        after = source.stat()
        self.assertEqual(
            (after.st_size, after.st_mtime_ns), (before.st_size, before.st_mtime_ns)
        )
        self.assertEqual(self.run_agent("search", str(source), "NEWWORD")["count"], 1)
        self.assertEqual(self.run_agent("search", str(source), "OLDWORD")["count"], 0)

    def test_text_cache_treats_mismatched_word_text_as_a_cache_miss(self) -> None:
        source = self.write_cache_document("corrupt-words.pdf", ["cache alpha"])
        self.assertEqual(self.run_agent("search", str(source), "alpha")["count"], 1)

        with sqlite3.connect(cli.HOME / "cache.sqlite") as connection:
            connection.execute(
                """
                UPDATE pages
                SET word_text=word_text || char(10) || 'orphanmarker'
                WHERE form='words'
                  AND page_index=(
                      SELECT min(page_index) FROM pages WHERE form='words'
                  )
                """
            )

        recovered = self.run_agent("search", str(source), "alpha")
        self.assertEqual(recovered["count"], 1)

        missing = self.run_agent("search", str(source), "orphanmarker")
        self.assertEqual((missing["count"], missing["hits"]), (0, []))

    def test_text_cache_reextracts_invalid_text_and_count_rows(self) -> None:
        source = self.write_cache_document("corrupt-scalars.pdf", ["cache alpha"])
        expected_text = self.run_agent("text", str(source))
        expected_count = self.run_agent("count", str(source))

        with sqlite3.connect(cli.HOME / "cache.sqlite") as connection:
            connection.execute("UPDATE pages SET text_value=x'00' WHERE form='text'")
            connection.execute("UPDATE pages SET char_count=-1 WHERE form='count'")

        self.assertEqual(self.run_agent("text", str(source)), expected_text)
        self.assertEqual(self.run_agent("count", str(source)), expected_count)

    def test_text_cache_recovers_from_stale_page_count(self) -> None:
        source = self.write_cache_document("stale-page-count.pdf", ["only page"])
        expected = self.run_agent("text", str(source))

        with sqlite3.connect(cli.HOME / "cache.sqlite") as connection:
            connection.execute("UPDATE documents SET page_count=2")

        self.assertEqual(self.run_agent("text", str(source)), expected)

    def test_zero_cache_budget_bypasses_existing_rows(self) -> None:
        source = self.write_cache_document("zero-budget.pdf", ["live text"])
        expected = self.run_agent("text", str(source))
        with sqlite3.connect(cli.HOME / "cache.sqlite") as connection:
            connection.execute(
                "UPDATE pages SET text_value='cached wrong text' WHERE form='text'"
            )

        os.environ["PDF_GOAT_CACHE_MB"] = "0"
        self.assertEqual(self.run_agent("text", str(source)), expected)

    def test_cache_budget_counts_empty_page_rows(self) -> None:
        source = self.write_cache_document("empty-pages.pdf", [""] * 4)
        os.environ["PDF_GOAT_CACHE_MB"] = str(1 / (1024 * 1024))
        receipt = self.run_agent("text", str(source))
        with sqlite3.connect(cli.HOME / "cache.sqlite") as connection:
            document_count = connection.execute(
                "SELECT count(*) FROM documents"
            ).fetchone()[0]
            page_count = connection.execute("SELECT count(*) FROM pages").fetchone()[0]

        self.assertEqual(
            receipt["pages"],
            [{"page": page, "text": ""} for page in range(1, 5)],
        )
        self.assertEqual((document_count, page_count), (0, 0))

    def test_text_cache_evicts_documents_as_units_at_cap(self) -> None:
        first = self.write_cache_document(
            "cache-first.pdf", ["same page alpha", "same page bravo"]
        )
        second = self.write_cache_document(
            "cache-second.pdf", ["same page gamma", "same page delta"]
        )
        self.run_agent("text", str(first))
        with sqlite3.connect(cli.HOME / "cache.sqlite") as connection:
            first_bytes = connection.execute(
                "SELECT row_bytes FROM documents WHERE path=?", (str(first.resolve()),)
            ).fetchone()[0]
        cap_bytes = first_bytes + max(1, first_bytes // 2)
        os.environ["PDF_GOAT_CACHE_MB"] = str(cap_bytes / (1024 * 1024))
        self.run_agent("text", str(second))

        with sqlite3.connect(cli.HOME / "cache.sqlite") as connection:
            documents = connection.execute(
                "SELECT path, page_count, row_bytes FROM documents"
            ).fetchall()
            page_count = connection.execute("SELECT count(*) FROM pages").fetchone()[0]
        self.assertEqual(len(documents), 1)
        self.assertEqual(documents[0][0], str(second.resolve()))
        self.assertEqual(documents[0][1], 2)
        self.assertEqual(page_count, 2)
        self.assertLessEqual(documents[0][2], cap_bytes)

    def write_ligature_pdf(self) -> Path:
        source = self.tempdir / "ligature.pdf"
        document = pymupdf.open()
        page = document.new_page()
        writer = pymupdf.TextWriter(page.rect)
        writer.append((72, 72), "De\ufb01ning the \ufb02ow", font=pymupdf.Font("helv"))
        writer.write_text(page)
        document.save(source)
        document.close()
        return source

    def test_search_and_redact_read_ligature_glyphs_as_letters(self) -> None:
        source = self.write_ligature_pdf()

        found = self.run_agent("search", str(source), "Defining")
        self.assertEqual(found["count"], 1)

        output = self.tempdir / "ligature-redacted.pdf"
        self.run_agent("redact", str(source), "--find", "flow", "-o", str(output))
        with pymupdf.open(output) as redacted:
            words = [word[4] for word in redacted[0].get_text("words")]
        self.assertEqual(words, ["De\ufb01ning", "the"])

    def test_search_and_redact_fold_ligature_glyphs_typed_in_the_query(self) -> None:
        source = self.write_ligature_pdf()

        found = self.run_agent("search", str(source), "De\ufb01ning")
        self.assertEqual(found["count"], 1)

        output = self.tempdir / "ligature-query-redacted.pdf"
        self.run_agent("redact", str(source), "--find", "\ufb02ow", "-o", str(output))
        with pymupdf.open(output) as redacted:
            words = [word[4] for word in redacted[0].get_text("words")]
        self.assertEqual(words, ["De\ufb01ning", "the"])

    def test_redact_counts_a_word_once_however_often_the_pattern_matches(self) -> None:
        source = self.tempdir / "repeat.pdf"
        document = pymupdf.open()
        page = document.new_page()
        page.insert_text((72, 72), "Mississippi river")
        document.save(source)
        document.close()

        output = self.tempdir / "repeat-redacted.pdf"
        receipt = self.run_agent(
            "redact", str(source), "--find", "s", "-o", str(output)
        )
        with pymupdf.open(output) as redacted:
            words = [word[4] for word in redacted[0].get_text("words")]
        self.assertEqual((receipt["redactions"], words), (1, ["river"]))

    def test_redact_match_on_a_join_keeps_the_word_before_it(self) -> None:
        source = self.tempdir / "join.pdf"
        document = pymupdf.open()
        page = document.new_page()
        page.insert_text((72, 72), "keep New York tail")
        document.save(source)
        document.close()

        output = self.tempdir / "join-redacted.pdf"
        receipt = self.run_agent(
            "redact", str(source), "--find", r"\s+New\s+York", "-o", str(output)
        )
        with pymupdf.open(output) as redacted:
            words = [word[4] for word in redacted[0].get_text("words")]
        self.assertEqual((receipt["redactions"], words), (1, ["keep", "tail"]))

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

    def test_page_verbs_agree_between_the_worker_pool_and_the_sequential_run(
        self,
    ) -> None:
        source = self.tempdir / "twelve-pages.pdf"
        document = pymupdf.open()
        for number in range(12):
            page = document.new_page()
            page.insert_text((72, 100), f"page {number + 1} alpha bravo")
        document.save(source)
        document.close()
        verbs = {
            "count": ("count", str(source)),
            "search": ("search", str(source), "alpha"),
            "text": ("text", str(source)),
            "text --layout": ("text", str(source), "--layout"),
            "inspect": ("inspect", str(source), "--limit", "12"),
            "render": (
                "render",
                str(source),
                "--dpi",
                "36",
                "-o",
                str(self.tempdir / "renders"),
            ),
        }
        previous = cli._POOL_AFTER_SECONDS, cli._POOL_MIN_PAGES
        self.addCleanup(self.set_pool_tuning, *previous)
        self.set_pool_tuning(0.0, 1)
        pooled = {name: self.run_agent(*args) for name, args in verbs.items()}
        # The sequential run overwrites the same render files, so read the
        # pooled images first; a worker that reports a path it never wrote
        # fails here.
        pooled_images = [Path(p).read_bytes() for p in pooled["render"]["outputs"]]
        # Redaction lands on whichever page the pool says held the hit, so check
        # the output document rather than the hit count.
        redacted = self.tempdir / "redacted.pdf"
        self.run_agent("redact", str(source), "--find", "^3$", "-o", str(redacted))
        with pymupdf.open(redacted) as document:
            kept_numbers = [
                str(number + 1) in page.get_text().split()
                for number, page in enumerate(document)
            ]
        self.set_pool_tuning(float("inf"), previous[1])
        uncached = {"count", "search", "text", "text --layout"}
        sequential = {}
        for name, arguments in verbs.items():
            suffix = ("--no-cache",) if name in uncached else ()
            sequential[name] = self.run_agent(*arguments, *suffix)
        for name in verbs:
            with self.subTest(verb=name):
                self.assertEqual(pooled[name], sequential[name])
        self.assertEqual(kept_numbers, [number != 2 for number in range(12)])
        self.assertEqual(
            pooled_images,
            [Path(p).read_bytes() for p in sequential["render"]["outputs"]],
        )

    @staticmethod
    def set_pool_tuning(after_seconds: float, min_pages: int) -> None:
        cli._POOL_AFTER_SECONDS = after_seconds
        cli._POOL_MIN_PAGES = min_pages
