#!/usr/bin/env python3
"""Check the pdf agent skill against a real pdf-goat binary.

The skill in .agents/skills/pdf (SKILL.md and references/*.md) tells agents which
pdf-goat commands to run and which result fields prove a job worked. This script
builds synthetic inputs with pdf-goat itself, then runs:

- every pdf-goat line of every ```bash block, exactly as written, through bash,
  with the variables each reference names ($IN, $W, ...) pointing at the inputs;
- every ```mcp example through the pdf-goat-mcp server over stdio JSON-RPC;
- every inline `pdf-goat --agent ...` example that has no placeholders.

```json $W/name and ```python $W/name blocks are written to that path first. A
command passes when it exits 0 with "ok": true and every path in "outputs" exists.
A command whose warnings say to run OCR again is run again, as ocr.md tells agents to,
up to 3 more times. Without --keep the work directory is a fresh one under the system
temp dir, removed when the run passes and kept, with its path printed, when it fails.
On top of that it asserts the result fields and values the skill quotes (EXPECT),
checks that the SKILL.md command map names every command `capabilities` reports,
and that SKILL.md stays within 200 lines.

Usage:
    tools/check_pdf_skill.py [--network] [--keep DIR]

    PDFGOAT_BIN    pdf-goat binary to check (default: pdf-goat on PATH)
    PDFGOAT_MCP    pdf-goat-mcp binary (default: pdf-goat-mcp beside the binary, when present)
    PDF_GOAT_HOME  home whose models/ holds the meaning model (default: ~/.pdf-goat)
    --network      also run commands that reach the network: time-stamp authority,
                   online revocation, model download
    --keep DIR     write inputs and outputs to DIR (new or empty) instead of a fresh
                   directory under the system temp dir

Signing uses a throwaway root and signer made with the openssl CLI in the work dir.
A command that needs something this machine lacks (macOS for OCR and its fonts, say,
office2pdf, LibreOffice, openssl, the meaning model, the network, pdf-goat-mcp) is
listed as not run with the reason, and so is every command that reads its output.
Exit status 0 means nothing failed. Python standard library only; every PDF is made
and read by pdf-goat.
"""

import argparse
import json
import os
import queue
import re
import shutil
import struct
import subprocess
import sys
import tempfile
import threading
import time
import zlib

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
SKILL = os.path.join(REPO, ".agents", "skills", "pdf")
DOCS = [
    "SKILL.md",
    "references/review-and-cite.md",
    "references/fill-form.md",
    "references/sign-paper-form.md",
    "references/certificate-signing.md",
    "references/ocr.md",
    "references/redact.md",
    "references/compare.md",
    "references/organize-pages.md",
    "references/create-pdfs.md",
    "references/document-properties.md",
    "references/office.md",
]
SOFFICE = "/Applications/LibreOffice.app/Contents/MacOS/soffice"
P12_PASSWORD = "skill-check-pass"
MCP_TIMEOUT = 600

AGREEMENT_MD = """# Service Agreement

This agreement is between Acme Corp and Jane Q. Member.

## Payment

The client pays {amount} USD per month. Account number 123-45-6789.

## Signatures

SIGNATURE: ____________________  DATE: __________

Visit [Acme](https://example.com/acme) for details.
"""

REPORT_HTML = (
    '<!doctype html><html lang="en"><head><meta charset="utf-8"><title>Quarterly Report</title>'
    "<style>body{font-family:Helvetica;} .pb{page-break-before:always;} "
    "table{border-collapse:collapse;} td,th{border:1px solid #000;padding:4px;}</style></head><body>"
    "<h1>Quarterly Report</h1><p>Alpha section. Revenue grew in the first quarter.</p>"
    '<h2 class="pb">Second Page</h2><p>Beta section. Costs fell.</p>'
    "<table><tr><th>Item</th><th>Amount</th></tr><tr><td>Rent</td><td>1000</td></tr>"
    "<tr><td>Power</td><td>250</td></tr></table>"
    '<h2 class="pb">Third Page</h2><p>Gamma section. Outlook is stable.</p></body></html>'
)

PAPER_HTML = (
    '<!doctype html><html lang="en"><head><meta charset="utf-8"><title>Membership Form</title>'
    "<style>body{font-family:Helvetica;font-size:12pt}</style></head><body>"
    "<h1>Membership Form</h1><p>Member name: ____________________</p>"
    "<p>&#9744; I agree to the terms.</p>"
    "<p>SIGNATURE: ____________________ DATE: __________</p></body></html>"
)

TRANSCRIPT_MD = """UNIVERSITY OF TEST

OFFICIAL ACADEMIC TRANSCRIPT

Student: REDACTED

Issued: 2026-07-01

Degree: Master of Science

Degree Awarded: 2026-06-15

Spring 2026

Course ID Course Title Grade Units Points

CS 101 Intro to Computing A 4.00 16.00

MATH 201 Discrete Math B+ 3.00 9.99

Term GPA: 3.50
"""

MEMO_MD = """# Board Memo

Prepared for the September meeting.

## Decisions

1. Approve the 2027 budget.
2. Renew the office lease.

| Item | Amount |
|------|-------:|
| Rent | 1,000 |
| Power | 250 |

Contact [the secretary](mailto:secretary@example.com) with questions.
"""

MEMO_CSS = """body { font-family: Georgia, serif; font-size: 11pt; }
h1 { color: #0a1a5c; }
table { border-collapse: collapse; }
td, th { border: 1px solid #888; padding: 3px 8px; }
"""

FLYER_HTML = """<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<title>Open House</title>
<style>
  @page { size: Letter; margin: 0.75in; }
  body { font-family: Helvetica, sans-serif; }
  h1 { font-size: 28pt; margin-bottom: 4pt; }
  .when { font-size: 14pt; color: #444; }
  .next { page-break-before: always; }
</style>
</head>
<body>
<h1>Open House</h1>
<p class="when">Saturday, October 10, 10:00 to 14:00</p>
<p>Tour the new office and meet the team.</p>
<h2 class="next">Directions</h2>
<p>Take the north entrance and follow the signs.</p>
</body>
</html>
"""

OPENSSL_CNF = """[req]
distinguished_name = dn
prompt = no
[dn]
O = Example Club
CN = Example Club Root
[root]
basicConstraints = critical, CA:TRUE
keyUsage = critical, keyCertSign, cRLSign
subjectKeyIdentifier = hash
[signer]
basicConstraints = CA:FALSE
keyUsage = critical, digitalSignature, nonRepudiation
subjectKeyIdentifier = hash
authorityKeyIdentifier = keyid
"""

# Fields over the paper form's blanks (search frame), as fill-form.md describes them.
FIELD_RECTS = {
    "member_name": "146,110,280,125",
    "date": "312,162,380,177",
    "agree": "62.5,141,70.5,149.5",
}

# Per reference: the variables its commands use. {fx} is the input directory, {work} the run.
DOC_VARS = {
    "SKILL": {"IN": "{fx}/agreement.pdf"},
    "review-and-cite": {"IN": "{fx}/agreement.pdf", "TRANSCRIPT": "{fx}/transcript.pdf",
                        "TRANSCRIPTS": "{fx}"},
    "fill-form": {"IN": "{fx}/form.pdf", "FLAT": "{fx}/paper.pdf"},
    "sign-paper-form": {"IN": "{fx}/paper.pdf", "SIG": "{fx}/signature.png"},
    "certificate-signing": {"IN": "{fx}/agreement.pdf", "SIG": "{fx}/signature.png"},
    "ocr": {"IN": "{fx}/scan.pdf"},
    "redact": {"IN": "{fx}/agreement.pdf", "SCANTEXT": "{work}/recipes/ocr/searchable.pdf"},
    "compare": {"A": "{fx}/agreement.pdf", "B": "{fx}/agreement-v2.pdf"},
    "organize-pages": {"IN": "{fx}/report.pdf", "OTHER": "{fx}/agreement.pdf"},
    "create-pdfs": {"MD": "{fx}/memo.md", "CSS": "{fx}/memo.css", "HTML": "{fx}/flyer.html",
                    "IMG1": "{fx}/photo-png/agreement_p001.png",
                    "IMG2": "{fx}/photo-jpg/report_p003.jpg"},
    "document-properties": {"IN": "{fx}/report.pdf", "ATTACH": "{fx}/notes.csv",
                            "PDF_PW": "open-sesame"},
    "office": {"IN": "{fx}/report.pdf"},
}

REMOVED = ["javascript", "embedded_files", "attached_files", "xml_metadata", "thumbnails"]

# ocr.md: a warning that says to run OCR again means the text may be wrong, so the command runs
# again (at most OCR_RERUNS more times); a warning that a file was moved aside is a repair note.
OCR_RERUN = "running OCR again"
OCR_RERUNS = 3
OCR_REPAIRED = r"^macOS had compiled part of the text recognizer wrongly, .* so macOS compiles it again, "

# Per reference: (regex matching exactly one command shown, [(json path, op, expected)]).
# Ops: "=" equal, "~" numbers or rects within 0.15 (or (value, tolerance)), "has" substring or
# member, "end" string suffix, "set" present and not empty, "len" length, "any" some list item
# carries all the given keys, "only" every list item matches the regex. A path starting "@file:"
# reads the file named at that path.
# MCP results carry "_content", the content item types of the reply.
EXPECT = {
    "SKILL": [
        (r"capabilities$", [("command_count", "=", 106), ("families", "len", 16),
                            ("commands", "len", 25)]),
        (r"capabilities edit$", [("schemas", "set", None)]),
        (r'info "\$IN"$', [("pages", "=", 1)]),
        (r"jobs --limit 5$", [("jobs", "set", None)]),
        (r"^capabilities \{", [("schemas", "set", None)]),
        (r'^run \{"command": "search"', [("count", "=", 1),
                                         ("hits.0.rect", "~", [139.9, 191.9, 193.3, 204.8])]),
        (r"^render \{", [("_content", "has", "image"), ("outputs.0", "end", ".png")]),
    ],
    "review-and-cite": [
        (r'agent preflight "\$IN"$', [("risk", "=", "low")]),
        (r'search "\$IN" "1,200 USD"$', [("count", "=", 1), ("hits.0.page", "=", 1),
                                        ("hits.0.rect", "~", [139.9, 191.9, 193.3, 204.8])]),
        (r"--clip 50,180,420,215", [("outputs.0", "end", "/cite/agreement_p001.png")]),
        (r"setup status", [("installed", "=", True)]),
        (r"--meaning", [("mode", "=", "meaning"), ("hits.0.text", "has", "1,200")]),
        (r"annotate highlight", [("marks", "=", 1)]),
        (r"annotate list", [("count", "=", 2), ("annotations", "any", {"type": "Highlight"})]),
        (r"transcript read", [("freshness.verdict", "=", "current")]),
    ],
    "fill-form": [
        (r'form list "\$IN"$', [("field_count", "=", 3), ("xfa", "=", False)]),
        (r'form fill "\$IN" --data "\$W/values.json" -o', [
            ("fields_set", "=", ["member_name", "date", "agree"]), ("unknown_fields", "=", []),
            ("flattened", "=", False)]),
        (r'form list "\$W/filled.pdf"', [
            ("fields.0.value", "=", "Jane Q. Member"), ("fields.1.value", "=", "09/30/2026"),
            ("fields.2.value", "=", "Yes"), ("fields.2.checked", "=", True)]),
        (r'render "\$W/filled.pdf"', [("outputs.0", "end", "/look/filled_p001.png")]),
        (r"--flatten -o", [("flattened", "=", True), ("unknown_fields", "=", [])]),
        (r'form list "\$W/flat.pdf"', [("field_count", "=", 0)]),
        (r'search "\$W/flat.pdf"', [("count", "=", 1),
                                    ("hits.0.rect", "~", [148.0, 109.5, 231.4, 124.9])]),
        (r"form export", [("format", "=", "xfdf"), ("field_count", "=", 3),
                          ("@file:outputs.0", "has", "<value>/Yes</value>")]),
        (r"form import", [("fields_set", "=", ["member_name", "date", "agree"]),
                          ("unknown_fields", "=", [])]),
        (r'form list "\$W/imported.pdf"', [("fields.0.value", "=", "Jane Q. Member"),
                                           ("fields.2.checked", "=", True)]),
        (r'search "\$FLAT" "____"', [("count", "=", 3),
                                     ("hits.0.rect", "~", [146.3, 112.4, 279.7, 124.4])]),
        (r"form create-text", [("field", "=", "member_name")]),
        (r'form list "\$W/fillable.pdf"', [("field_count", "=", 2),
                                           ("fields.0.rect", "~", [146, 110, 280, 125]),
                                           ("fields.1.on_state", "=", "Yes")]),
    ],
    "sign-paper-form": [
        (r'search "\$IN" "____"', [("count", "=", 3),
                                   ("hits.0.rect", "~", [146.3, 112.4, 279.7, 124.4]),
                                   ("hits.1.rect", "~", [138.0, 164.5, 271.5, 176.5]),
                                   ("hits.2.rect", "~", [312.6, 164.5, 379.3, 176.5])]),
        (r'search "\$IN" "SIGNATURE:"', [("hits.0.rect", "~", [62.2, 164.5, 134.7, 176.5])]),
        (r'search "\$IN" "DATE:"', [("hits.0.rect", "~", [274.8, 164.5, 309.3, 176.5])]),
        (r'search "\$IN" "☐"', [("hits.0.rect", "~", [62.2, 136.4, 71.1, 152.5])]),
        (r'-o "\$W/step1.pdf"', [("bbox", "~", [150.0, 109.2, 231.9, 124.3]),
                                 ("font", "=", "Helvetica"), ("size", "~", 11.0)]),
        (r'-o "\$W/step2.pdf"', [("bbox", "~", [62.8, 140.7, 70.2, 149.6]),
                                 ("font", "=", "ZapfDingbats"), ("size", "~", 8.83)]),
        (r'-o "\$W/step3.pdf"', [("bbox", "~", [142.0, 152.0, 253.9, 174.0]),
                                 ("font", "=", "SnellRoundhand"), ("size", "~", 17.45)]),
        (r'-o "\$W/signed.pdf"', [("bbox", "~", [316.0, 162.2, 371.0, 177.3])]),
        (r'search "\$W/signed.pdf"', [("count", "=", 1)]),
        (r"edit add-image", [("bbox", "~", [142.0, 146.0, 262.0, 176.0])]),
        (r"annotate ink", [("points", "=", 8)]),
        (r'annotate list "\$W/signed-ink.pdf"', [("count", "=", 0)]),
    ],
    "certificate-signing": [
        (r'--reason "Membership application"', [
            ("signer", "=", "Jane Q. Member"), ("self_signed", "=", False),
            ("pades_level", "=", "B-B"), ("visible", "=", True), ("field", "=", "Member"),
            ("timestamp", "=", None)]),
        (r'verify "\$W/signed.pdf"$', [
            ("signatures.0.intact", "=", True), ("signatures.0.valid", "=", True),
            ("signatures.0.chain_trusted", "=", False),
            ("signatures.0.trust_error", "=", "the chain ends at Common Name: Example Club Root, "
             "Organization: Example Club, which is not a trusted root")]),
        (r'verify "\$W/signed.pdf" --trust', [("signatures.0.chain_trusted", "=", True)]),
        (r'form list "\$W/signed.pdf"', [("fields", "any", {"name": "Member", "type": "Signature"})]),
        (r'-o "\$W/signed-t.pdf"', [("timestamp", "set", None), ("pades_level", "=", "B-T")]),
        (r'-o "\$W/signed-lta.pdf"', [("pades_level", "set", None), ("document_timestamp", "set", None),
                                      ("dss.certificates", "set", None)]),
        (r'verify "\$W/signed-lta.pdf"', [
            ("signature_count", "=", 2),
            ("signatures", "any", {"kind": "document_timestamp", "field": "DocTimeStamp1"}),
            ("signatures", "any", {"kind": "signature", "modified": "LTA_UPDATES"})]),
        (r'-o "\$W/signed-lt.pdf"', [("pades_level", "=", "B-B"), ("dss.certificates", "=", 2),
                                     ("dss.ocsp_responses", "=", 0), ("dss.unchecked", "set", None)]),
        (r"--certify 2", [("certified", "=", True), ("docmdp_level", "=", 2)]),
        (r'verify "\$W/certified.pdf"', [("signatures.0.certified", "=", True),
                                         ("signatures.0.docmdp_level", "=", 2)]),
        (r"--field Witness", [("field", "=", "Witness")]),
        (r'verify "\$W/countersigned.pdf"', [
            ("signature_count", "=", 2), ("signatures.0.intact", "=", True),
            ("signatures.0.modified", "=", "FORM_FILLING"), ("signatures.0.changes_allowed", "=", True)]),
        (r'verify "\$W/signed-pss.pdf"', [("signatures.0.intact", "=", True),
                                          ("signatures.0.valid", "=", True)]),
        (r"demo-signed", [("signer", "=", "pdf-goat demo"), ("self_signed", "=", True)]),
    ],
    "ocr": [
        (r'info "\$IN"$', [("has_text", "=", False)]),
        (r"convert ocr", [("standard", "=", "PDF/A-2u"), ("warnings", "only", OCR_REPAIRED)]),
        (r'info "\$W/searchable.pdf"', [("has_text", "=", True)]),
        (r'search "\$W/searchable.pdf"', [("count", "=", 1), ("hits.0.page", "=", 1),
                                          ("hits.0.rect", "~", ([141.8, 110.5, 218.1, 124.2], 1.0))]),
    ],
    "redact": [
        (r'search "\$IN" "123-45-6789"', [("count", "=", 1),
                                          ("hits.0.rect", "~", [337.8, 191.9, 404.5, 204.8])]),
        (r'text "\$IN" --mask', [("pages.0.text", "has", "[REDACTED]")]),
        (r'-o "\$W/redacted-1.pdf"', [("redactions", "=", 1)]),
        (r'-o "\$W/redacted.pdf"', [("redactions", "=", 1)]),
        (r'search "\$W/redacted.pdf" "123-45-6789"', [("count", "=", 0)]),
        (r'search "\$W/redacted.pdf" "Acme Corp"', [("count", "=", 0)]),
        (r'text "\$W/redacted.pdf"', [("pages.0.text", "has", "Account number")]),
        (r"security sanitize", [("removed", "=", REMOVED)]),
        (r'meta get "\$W/clean.pdf"', [("metadata", "=", {"format": "PDF 1.7"})]),
        (r'-o "\$W/scan-redacted.pdf"', [("redactions", "=", 1)]),
        (r'search "\$W/scan-redacted.pdf"', [("count", "=", 0)]),
        (r'get images "\$W/scan-redacted.pdf"', [("count", "=", 2)]),
    ],
    "compare": [
        (r'compare text "\$A" "\$B"$', [("identical", "=", False), ("added", "=", 1),
                                        ("removed", "=", 1), ("pages.0.match.page", "=", 1)]),
        (r"compare text .* --mask", [("identical", "=", False)]),
        (r"compare visual", [("pages.0.bbox", "=", [149, 194, 161, 203]),
                             ("pages.0.changed_ratio", "~", (0.0002, 0.0001)),
                             ("outputs.0", "end", "/visual/diff_p1.png")]),
        (r'render "\$B"', [("outputs.0", "end", "/look/agreement-v2_p001.png")]),
    ],
    "organize-pages": [
        (r"agent merge", [("merged_pages", "=", 4)]),
        (r'info "\$W/merged.pdf"', [("pages", "=", 4)]),
        (r"agent split", [("parts", "=", 3), ("outputs.2", "end", "/split/report_003.pdf")]),
        (r"agent extract", [("pages", "=", [3, 1])]),
        (r"agent delete", [("deleted_pages", "=", [2]), ("remaining_pages", "=", 2)]),
        (r"agent reorder", [("order", "=", [3, 1, 2])]),
        (r'text "\$W/reordered.pdf"', [("pages.0.text", "has", "Third Page"),
                                       ("pages.1.text", "has", "Quarterly Report")]),
        (r"agent rotate", [("rotated_pages", "=", [2]), ("deg", "=", 90)]),
        (r'inspect "\$W/rotated.pdf"', [("pages.1.rotation", "=", 90), ("pages.1.width_pt", "~", 841.9)]),
        (r"pages insert", [("at", "=", 2)]),
        (r"pages replace", [("replaced", "=", [3])]),
        (r'text "\$W/replaced.pdf"', [("pages.2.text", "has", "Service Agreement")]),
        (r"pages blank", [("total_pages", "=", 4)]),
        (r"pages duplicate", [("duplicated_pages", "=", [1]), ("total_pages", "=", 4)]),
        (r"pages scale", [("factor", "=", 0.5)]),
        (r"pages crop", [("box", "=", [36.0, 36.0, 559.0, 806.0])]),
        (r'inspect "\$W/cropped.pdf"', [("pages.0.width_pt", "~", 523.0), ("pages.0.height_pt", "~", 770.0)]),
        (r'get object "\$W/trimmed.pdf"', [("object", "has", "/TrimBox [ 10 10 585 831 ]")]),
        (r"pages nup", [("sheets", "=", 2)]),
        (r"pages booklet", [("page_order", "=", [4, 1, 2, 3]), ("sheets", "=", 2)]),
        (r'search "\$W/footer.pdf"', [("count", "=", 1), ("hits.0.page", "=", 2),
                                      ("hits.0.rect", "~", [272.1, 795.1, 323.2, 808.9])]),
        (r"pages bates", [("first", "=", "ACME-00100")]),
        (r'search "\$W/bates.pdf"', [("count", "=", 1), ("hits.0.page", "=", 3)]),
        (r"annotate stamp", [("stamp", "=", 0)]),
        (r"pages flatten", [("flattened", "=", ["annotations", "form_fields"])]),
        (r'annotate list "\$W/approved-flat.pdf"', [("count", "=", 0)]),
    ],
    "create-pdfs": [
        (r"from-md", [("output_bytes", "set", None)]),
        (r'info "\$W/memo.pdf"', [("pages", "=", 1), ("metadata.title", "=", "memo")]),
        (r'search "\$W/memo.pdf"', [("count", "=", 1), ("hits.0.rect", "~", [92.3, 181.9, 202.3, 194.4])]),
        (r'get links "\$W/memo.pdf"', [("links.0.uri", "=", "mailto:secretary@example.com")]),
        (r"meta set", [("metadata.title", "=", "Board Memo")]),
        (r'inspect "\$W/flyer.pdf"', [("total_pages", "=", 2), ("pages.0.width_pt", "~", 612.0),
                                      ("pages.0.height_pt", "~", 792.0)]),
        (r'text "\$W/flyer.pdf"', [("pages.1.text", "has", "Directions")]),
        (r"from-images", [("image_count", "=", 2)]),
        (r'inspect "\$W/photos.pdf"', [("pages.0.width_pt", "~", 596.0), ("pages.1.height_pt", "~", 842.0)]),
        (r"convert pdfa", [("standard", "=", "PDF/A-2b"), ("conformance_validated", "=", False)]),
    ],
    "document-properties": [
        (r'meta get "\$IN"', [("has_xmp", "=", False), ("metadata.title", "=", "Quarterly Report")]),
        (r"meta set", [("metadata.title", "=", "Quarterly Report Q3"),
                       ("metadata.author", "=", "Finance Team")]),
        (r'meta get "\$W/meta.pdf"', [("metadata.keywords", "=", "revenue, costs")]),
        (r"bookmarks set", [("count", "=", 3)]),
        (r"get bookmarks", [("bookmarks.1.level", "=", 2), ("bookmarks.2.title", "=", "Third Page")]),
        (r"--goto 3", [("page", "=", 1)]),
        (r'get links "\$W/links.pdf"', [("count", "=", 2), ("links.0.target_page", "=", 2),
                                        ("links.1.uri", "=", "https://example.com/q3")]),
        (r"links remove", [("removed", "=", 1), ("external_only", "=", True)]),
        (r"bookmarks clear", [("removed", "=", 3)]),
        (r"agent attach", [("attached", "=", "notes.csv")]),
        (r"get attachments", [("count", "=", 1), ("outputs.0", "end", "/attachments/notes.csv"),
                              ("@file:outputs.0", "=", "item,amount\nrent,1000\npower,250\n")]),
        (r"agent detach", [("removed", "=", ["notes.csv"]), ("count", "=", 1)]),
        (r'accessibility check "\$W/meta.pdf"', [("issues", "=", ["untagged", "no_lang"])]),
        (r"accessibility set", [("lang", "=", "en-US")]),
        (r'accessibility check "\$W/a11y.pdf"', [("issues", "=", [])]),
        (r"security encrypt", [("algorithm", "=", "AES-256")]),
        (r'preflight "\$W/locked.pdf"', [("needs_password", "=", True), ("risk", "=", "unknown")]),
        (r'info "\$W/unlocked.pdf"', [("encrypted", "=", False), ("pages", "=", 3)]),
        (r"security permissions", [("no_print", "=", True), ("no_copy", "=", True),
                                   ("no_modify", "=", False)]),
        (r'info "\$W/restricted.pdf"', [("permissions.print", "=", False),
                                        ("permissions.copy", "=", False)]),
        (r"optimize reduce", [("preset", "=", "screen"), ("saved_bytes", "set", None)]),
        (r"agent compress", [("kept_original", "set", None), ("linearized", "set", None)]),
        (r"agent repair", [("warnings", "=", False)]),
        (r"agent count", [("pages", "=", 3)]),
        (r'inspect "\$IN" --limit 1', [("truncated", "=", True), ("next_page", "=", 2)]),
        (r"get fonts", [("fonts", "set", None)]),
        (r'get object "\$IN" catalog', [("object", "has", "/Type /Catalog")]),
    ],
    "office": [
        (r"convert xlsx", [("sheets", "=", 1)]),
        (r"convert pptx", [("slides", "=", 3)]),
        (r"convert tables", [("tables", "=", 1), ("outputs.0", "end", "/tables/p2_t1.csv"),
                             ("@file:outputs.0", "has", "Item,Amount"),
                             ("@file:outputs.0", "has", "Power,250")]),
        (r"convert audio", [("chars", "set", None), ("duration_sec", "set", None)]),
        (r"convert from-office", [("engine", "=", "office2pdf"), ("warnings", "=", [])]),
        (r"office run", [("stdout", "has", "41")]),
        (r'text "\$W/letter.pdf"', [("pages.0.text", "has", "Board minutes 2026-09-30. Motion carried.")]),
    ],
}


class Failure(Exception):
    pass


def write(path, text):
    with open(path, "w", encoding="utf-8") as handle:
        handle.write(text)


def png(path, width, height, pixels):
    """Write an 8-bit RGBA PNG."""
    stride = width * 4
    raw = b"".join(b"\x00" + bytes(pixels[y * stride:(y + 1) * stride]) for y in range(height))

    def chunk(tag, data):
        body = tag + data
        return struct.pack(">I", len(data)) + body + struct.pack(">I", zlib.crc32(body) & 0xFFFFFFFF)

    header = struct.pack(">IIBBBBB", width, height, 8, 6, 0, 0, 0)
    with open(path, "wb") as handle:
        handle.write(b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", header)
                     + chunk(b"IDAT", zlib.compress(raw, 9)) + chunk(b"IEND", b""))


def signature_png(path):
    """A dark blue hand-drawn stroke on a transparent background."""
    width, height, radius = 480, 120, 4
    pixels = bytearray(width * height * 4)
    points = [(20, 90), (60, 20), (90, 100), (130, 30), (170, 95), (210, 40),
              (250, 85), (300, 50), (360, 70), (460, 60)]
    for (x0, y0), (x1, y1) in zip(points, points[1:]):
        steps = int(max(abs(x1 - x0), abs(y1 - y0))) + 1
        for i in range(steps + 1):
            x = x0 + (x1 - x0) * i / steps
            y = y0 + (y1 - y0) * i / steps
            for dy in range(-radius, radius + 1):
                for dx in range(-radius, radius + 1):
                    xi, yi = int(x + dx), int(y + dy)
                    if dx * dx + dy * dy <= radius * radius and 0 <= xi < width and 0 <= yi < height:
                        offset = (yi * width + xi) * 4
                        pixels[offset:offset + 4] = bytes((10, 26, 92, 255))
    png(path, width, height, pixels)


def build_inputs(binary, fx, env):
    """Make every input the recipes start from; every PDF comes from pdf-goat."""

    def agent(*args):
        proc = subprocess.run([binary, "--agent", *args], capture_output=True, text=True, env=env)
        try:
            result = json.loads(proc.stdout)
        except json.JSONDecodeError as error:
            raise Failure(f"input {args[:2]}: no JSON ({error}): {proc.stderr[-300:]}") from error
        if proc.returncode != 0 or not result.get("ok"):
            raise Failure(f"input {args[:2]}: {result.get('error')}")
        return result

    os.makedirs(fx)
    write(f"{fx}/agreement.md", AGREEMENT_MD.format(amount="1,200"))
    write(f"{fx}/agreement-v2.md", AGREEMENT_MD.format(amount="1,350"))
    write(f"{fx}/report.html", REPORT_HTML)
    write(f"{fx}/paper.html", PAPER_HTML)
    write(f"{fx}/transcript.md", TRANSCRIPT_MD)
    write(f"{fx}/memo.md", MEMO_MD)
    write(f"{fx}/memo.css", MEMO_CSS)
    write(f"{fx}/flyer.html", FLYER_HTML)
    write(f"{fx}/notes.csv", "item,amount\nrent,1000\npower,250\n")
    signature_png(f"{fx}/signature.png")
    agent("from-md", f"{fx}/agreement.md", "-o", f"{fx}/agreement.pdf")
    agent("from-md", f"{fx}/agreement-v2.md", "-o", f"{fx}/agreement-v2.pdf")
    agent("from-html", f"{fx}/report.html", "-o", f"{fx}/report.pdf")
    agent("from-html", f"{fx}/paper.html", "-o", f"{fx}/paper.pdf")
    agent("from-md", f"{fx}/transcript.md", "-o", f"{fx}/transcript.pdf")
    scans = agent("render", f"{fx}/report.pdf", "--pages", "1-2", "--dpi", "200",
                  "-o", f"{fx}/scan-pages")["outputs"]
    agent("from-images", *scans, "-o", f"{fx}/scan.pdf")
    agent("render", f"{fx}/agreement.pdf", "--dpi", "72", "-o", f"{fx}/photo-png")
    agent("render", f"{fx}/report.pdf", "--pages", "3", "--dpi", "72", "--format", "jpg",
          "-o", f"{fx}/photo-jpg")
    agent("form", "create-text", f"{fx}/paper.pdf", "--name", "member_name",
          "--rect", FIELD_RECTS["member_name"], "-o", f"{fx}/form-1.pdf")
    agent("form", "create-text", f"{fx}/form-1.pdf", "--name", "date",
          "--rect", FIELD_RECTS["date"], "-o", f"{fx}/form-2.pdf")
    agent("form", "create-checkbox", f"{fx}/form-2.pdf", "--name", "agree",
          "--rect", FIELD_RECTS["agree"], "-o", f"{fx}/form.pdf")


def make_identity(directory):
    """A throwaway root and signer PKCS#12 made with the openssl CLI; None without openssl."""
    openssl = shutil.which("openssl")
    if not openssl:
        return None
    os.makedirs(directory)
    write(f"{directory}/openssl.cnf", OPENSSL_CNF)
    steps = [
        ["req", "-x509", "-new", "-newkey", "rsa:2048", "-nodes", "-keyout", "ca.key", "-out", "ca.pem",
         "-days", "7", "-config", "openssl.cnf", "-extensions", "root"],
        ["req", "-new", "-newkey", "rsa:2048", "-nodes", "-keyout", "signer.key", "-out", "signer.csr",
         "-config", "openssl.cnf", "-subj", "/O=Example Club/CN=Jane Q. Member"],
        ["x509", "-req", "-in", "signer.csr", "-CA", "ca.pem", "-CAkey", "ca.key", "-CAcreateserial",
         "-out", "signer.pem", "-days", "7", "-extfile", "openssl.cnf", "-extensions", "signer"],
        ["pkcs12", "-export", "-inkey", "signer.key", "-in", "signer.pem", "-certfile", "ca.pem",
         "-name", "Jane Q. Member", "-out", "signer.p12", "-passout", f"pass:{P12_PASSWORD}"],
    ]
    for step in steps:
        proc = subprocess.run([openssl, *step], cwd=directory, capture_output=True, text=True)
        if proc.returncode != 0:
            raise Failure(f"openssl {step[0]} failed: {proc.stderr[-300:]}")
    return {"P12": f"{directory}/signer.p12", "CA": f"{directory}/ca.pem", "P12_PW": P12_PASSWORD}


def unavailable(network, identity, models_dir, models_ok):
    """Map a text a command line contains to the reason it cannot run on this machine."""
    reasons = {}
    if sys.platform != "darwin":
        reasons["convert ocr"] = "OCR uses macOS Vision"
        reasons["Snell Roundhand"] = "the font ships with macOS"
    if not shutil.which("say"):
        reasons["convert audio"] = "convert audio needs macOS say on PATH"
    if not shutil.which("office2pdf"):
        reasons["convert from-office"] = "office2pdf is not on PATH"
    if not os.path.exists(SOFFICE):
        reasons["office run"] = reasons["office export"] = "LibreOffice is not in /Applications"
    if identity is None:
        reasons["$P12"] = reasons["$CA"] = "openssl is not on PATH, so there is no signing identity"
    if not models_ok:
        reasons["--meaning"] = reasons["setup status"] = f"no meaning model in {models_dir}"
    if not network:
        for marker in ("--tsa", "--online", "setup meaning"):
            reasons[marker] = "needs the network (pass --network)"
    return reasons


def expand(text, env):
    return re.sub(r"\$\{?(\w+)\}?", lambda m: env.get(m.group(1), m.group(0)), text)


OUTPUT_ARG = re.compile(r"""(?:^|\s)-o\s+("[^"]*"|'[^']*'|\S+)""")


def output_of(line, env):
    """The -o path of a command line, expanded, and the line without it."""
    match = OUTPUT_ARG.search(line)
    if not match:
        return None, line
    target = expand(match.group(1).strip("\"'"), env)
    return target, line[:match.start()] + line[match.end():]


def blocks(text):
    """Split Markdown into ("prose", None, [line]) and (lang, target, body) fenced blocks."""
    out, lang, target, body, inside = [], None, None, [], False
    for line in text.splitlines():
        if line.startswith("```"):
            if inside:
                out.append((lang, target, body))
                inside = False
            else:
                parts = line[3:].split()
                lang = parts[0] if parts else ""
                target = parts[1] if len(parts) > 1 else None
                body, inside = [], True
        elif inside:
            body.append(line)
        else:
            out.append(("prose", None, [line]))
    return out


def run_cmd(line, env):
    """Run one command line as shown; return (status, result or detail)."""
    proc = subprocess.run(["bash", "-c", line], capture_output=True, text=True, env=env, timeout=1800)
    try:
        result = json.loads(proc.stdout)
    except json.JSONDecodeError:
        return "FAIL", f"exit {proc.returncode}, no JSON: {proc.stdout[-200:]} {proc.stderr[-200:]}"
    if proc.returncode != 0 or result.get("ok") is not True:
        return "FAIL", f"exit {proc.returncode}: {result.get('error')}"
    missing = [path for path in result.get("outputs", []) if not os.path.exists(path)]
    if missing:
        return "FAIL", f"outputs not on disk: {missing}"
    return "PASS", result


class McpClient:
    """A pdf-goat-mcp server on stdio, spoken to in newline-delimited JSON-RPC."""

    def __init__(self, path, env):
        self.proc = subprocess.Popen([path], stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                     stderr=subprocess.DEVNULL, text=True, env=env)
        self.lines = queue.Queue()
        self.ident = 0
        threading.Thread(target=self._read, daemon=True).start()
        self.request("initialize", {"protocolVersion": "2025-06-18", "capabilities": {},
                                    "clientInfo": {"name": "check_pdf_skill", "version": "1"}})
        self._send({"jsonrpc": "2.0", "method": "notifications/initialized"})

    def _read(self):
        for line in self.proc.stdout:
            self.lines.put(line)
        self.lines.put(None)

    def _send(self, message):
        self.proc.stdin.write(json.dumps(message) + "\n")
        self.proc.stdin.flush()

    def request(self, method, params):
        self.ident += 1
        self._send({"jsonrpc": "2.0", "id": self.ident, "method": method, "params": params})
        deadline = time.monotonic() + MCP_TIMEOUT
        while True:
            try:
                line = self.lines.get(timeout=max(0.1, deadline - time.monotonic()))
            except queue.Empty as error:
                raise Failure(f"pdf-goat-mcp sent no reply to {method} in {MCP_TIMEOUT} s") from error
            if line is None:
                raise Failure(f"pdf-goat-mcp exited during {method}")
            message = json.loads(line)
            if message.get("id") == self.ident:
                return message

    def call(self, line, env):
        """Run one example line `tool {json arguments}`; return (status, result or detail)."""
        tool, _, raw = line.partition(" ")
        reply = self.request("tools/call", {"name": tool, "arguments": json.loads(expand(raw, env))})
        result = reply.get("result")
        if "error" in reply or not isinstance(result, dict):
            return "FAIL", json.dumps(reply)[:300]
        content = result.get("content") or []
        texts = [item.get("text", "") for item in content if item.get("type") == "text"]
        if result.get("isError"):
            return "FAIL", " ".join(texts)[:300]
        payload = result.get("structuredContent")
        for text in texts:
            if payload is not None:
                break
            try:
                payload = json.loads(text)
            except json.JSONDecodeError:
                continue
        if not isinstance(payload, dict):
            return "FAIL", f"no JSON object in the reply: {json.dumps(result)[:300]}"
        payload = dict(payload, _content=[item.get("type") for item in content])
        missing = [path for path in payload.get("outputs", []) if not os.path.exists(path)]
        if missing:
            return "FAIL", f"outputs not on disk: {missing}"
        return "PASS", payload

    def close(self):
        self.proc.stdin.close()
        self.proc.wait(timeout=30)


def lookup(result, path):
    if path.startswith("@file:"):
        with open(lookup(result, path[len("@file:"):]), encoding="utf-8") as handle:
            return handle.read()
    value = result
    for part in path.split("."):
        value = value[int(part)] if isinstance(value, list) else value[part]
    return value


def check(result, path, op, want):
    try:
        got = lookup(result, path)
    except (KeyError, IndexError, TypeError, ValueError, OSError) as error:
        return False, f"{path}: missing ({error!r})"
    if op == "=":
        ok = got == want
    elif op == "~":
        value, tolerance = want if isinstance(want, tuple) else (want, 0.15)
        if isinstance(value, list):
            ok = isinstance(got, list) and len(got) == len(value) and all(
                abs(g - w) <= tolerance for g, w in zip(got, value))
        else:
            ok = isinstance(got, (int, float)) and abs(got - value) <= tolerance
    elif op == "has":
        ok = want in got
    elif op == "end":
        ok = isinstance(got, str) and got.endswith(want)
    elif op == "set":
        ok = got is not None and got != "" and got != [] and got != {}
    elif op == "len":
        ok = len(got) == want
    elif op == "any":
        ok = any(all(item.get(key) == val for key, val in want.items()) for item in got)
    elif op == "only":
        ok = isinstance(got, list) and all(re.search(want, item) for item in got)
    else:
        raise ValueError(op)
    return ok, f"{path} {op} {want!r}, got {json.dumps(got)[:160]}"


def leaf_commands(binary, env):
    def caps(*selector):
        proc = subprocess.run([binary, "--agent", "capabilities", *selector],
                              capture_output=True, text=True, env=env, check=True)
        return json.loads(proc.stdout)

    top = caps()
    leaves = set()

    def walk(name, schema):
        subs = schema.get("commands") or {}
        if not subs:
            leaves.add((schema.get("command") or name).removeprefix("pdf-goat ").strip())
        for sub_name, sub_schema in subs.items():
            walk(sub_name, sub_schema)

    for selector in top["commands"] + top["families"]:
        for name, schema in caps(selector)["schemas"].items():
            walk(name, schema)
    return leaves, top["command_count"]


def mapped_commands(skill_text):
    section = skill_text.split("## Command map", 1)[1].split("\n## ", 1)[0]
    names = set()
    for line in section.splitlines():
        if not line.startswith("- "):
            continue
        for span in re.findall(r"`([^`]+)`", line):
            words = []
            for token in span.split():
                if not re.fullmatch(r"[a-z][a-z0-9-]*(\|[a-z][a-z0-9-]*)*", token):
                    break
                words.append(token.split("|"))
            combos = [""]
            for alternatives in words:
                combos = [f"{combo} {alt}".strip() for combo in combos for alt in alternatives]
            names.update(combo for combo in combos if combo)
    return names


def doc_commands(name, text, env, reasons, skipped, reruns, mcp, mcp_reason):
    """Run one document's commands in order; return [(line, status, result or detail)]."""
    commands = []

    def attempt(line, runner):
        target, rest = output_of(line, env)
        reason = next((why for marker, why in reasons.items() if marker in line), None)
        if reason is None:
            expanded = expand(rest, env)
            if any(re.search(re.escape(path) + r"""(?=["'\s/]|$)""", expanded) for path in skipped):
                reason = "reads the output of a command that was not run"
        if reason is not None:
            if target:
                skipped.add(target)
            commands.append((line, "NOT RUN", reason))
            return
        status, result = runner(line)
        for _ in range(OCR_RERUNS):
            warnings = (result.get("warnings") or []) if status == "PASS" else []
            if not any(OCR_RERUN in warning for warning in warnings):
                break
            reruns.append(f"{name}: {line}")
            status, result = runner(line)
        commands.append((line, status, result))

    for lang, target, body in blocks(text):
        if lang == "prose":
            for span in re.findall(r"`(pdf-goat --agent [^`]+)`", body[0]):
                if "..." in span or "…" in span:
                    continue
                if "$" in span:
                    commands.append((span, "NOT RUN", "inline example with placeholders"))
                else:
                    attempt(span, lambda line: run_cmd(line, env))
        elif target and target.startswith("$"):
            write(expand(target, env), "\n".join(body) + "\n")
        elif lang == "bash":
            for line in (raw.strip() for raw in body):
                if not line or line.startswith("#"):
                    continue
                if not line.startswith("pdf-goat "):
                    commands.append((line, "FAIL", "not a pdf-goat command"))
                    continue
                attempt(line, lambda line: run_cmd(line, env))
        elif lang == "mcp":
            for line in (raw.strip() for raw in body):
                if not line:
                    continue
                if mcp_reason:
                    commands.append((line, "NOT RUN", mcp_reason))
                else:
                    attempt(line, lambda line: mcp().call(line, env))
    return commands


def main():
    parser = argparse.ArgumentParser(description="Check the pdf agent skill against pdf-goat.")
    parser.add_argument("--network", action="store_true",
                        help="also run commands that reach the network")
    parser.add_argument("--keep", metavar="DIR", help="write inputs and outputs to DIR (new or empty)")
    args = parser.parse_args()

    binary = os.environ.get("PDFGOAT_BIN") or shutil.which("pdf-goat")
    if not binary or not os.access(binary, os.X_OK):
        sys.exit("pdf-goat not found: set PDFGOAT_BIN or put pdf-goat on PATH")
    binary = os.path.abspath(binary)
    mcp_path = os.environ.get("PDFGOAT_MCP")
    if not mcp_path:
        beside = os.path.join(os.path.dirname(os.path.realpath(binary)), "pdf-goat-mcp")
        mcp_path = beside if os.access(beside, os.X_OK) else None
    mcp_reason = None if mcp_path else f"no pdf-goat-mcp beside {binary}; set PDFGOAT_MCP"

    if args.keep:
        work = os.path.abspath(args.keep)
        os.makedirs(work, exist_ok=True)
        if os.listdir(work):
            sys.exit(f"--keep {work}: the directory must be new or empty")
    else:
        work = tempfile.mkdtemp(prefix="pdf-skill-check-")
    home = os.environ.get("PDF_GOAT_HOME") or os.path.expanduser("~/.pdf-goat")
    models = os.path.join(home, "models")
    models_ok = os.path.isdir(models) and bool(os.listdir(models))
    os.makedirs(f"{work}/home")
    if models_ok:
        os.symlink(models, f"{work}/home/models")
    os.makedirs(f"{work}/bin")
    os.symlink(binary, f"{work}/bin/pdf-goat")
    env = dict(os.environ, PDF_GOAT_HOME=f"{work}/home", PATH=f"{work}/bin{os.pathsep}{os.environ['PATH']}")
    started = time.monotonic()
    print(f"work {work}\nbinary {binary}\nmcp {mcp_path or 'not found'}\nnetwork {args.network}")
    fx = f"{work}/inputs"
    build_inputs(binary, fx, env)
    identity = make_identity(f"{work}/identity")
    reasons = unavailable(args.network, identity, models, models_ok)

    client = None

    def mcp():
        nonlocal client
        if client is None:
            client = McpClient(mcp_path, env)
        return client

    tally = {"commands": {"PASS": 0, "FAIL": 0, "NOT RUN": 0}, "checks": {"PASS": 0, "FAIL": 0, "NOT RUN": 0}}
    not_run, failures, skipped, reruns = [], [], set(), []
    try:
        for doc in DOCS:
            name = os.path.splitext(os.path.basename(doc))[0]
            recipe = f"{work}/recipes/{name}"
            os.makedirs(recipe)
            denv = dict(env, W=recipe)
            denv.update({key: value.format(fx=fx, work=work) for key, value in DOC_VARS[name].items()})
            if name == "certificate-signing" and identity:
                denv.update(identity)
            with open(os.path.join(SKILL, doc), encoding="utf-8") as handle:
                text = handle.read()
            commands = doc_commands(name, text, denv, reasons, skipped, reruns, mcp, mcp_reason)
            for line, status, detail in commands:
                tally["commands"][status] += 1
                print(f"{status:7} {name}: {line}")
                if status == "FAIL":
                    failures.append(f"{name}: {line}\n        {detail}")
                elif status == "NOT RUN":
                    not_run.append(f"{name}: {line} ({detail})")
            for pattern, checks in EXPECT.get(name, []):
                matches = [c for c in commands if re.search(pattern, c[0])]
                if len(matches) != 1:
                    tally["checks"]["FAIL"] += len(checks)
                    failures.append(f"{name}: expectation /{pattern}/ matched {len(matches)} commands")
                    continue
                line, status, result = matches[0]
                for path, op, want in checks:
                    if status != "PASS":
                        tally["checks"]["NOT RUN" if status == "NOT RUN" else "FAIL"] += 1
                        continue
                    ok, detail = check(result, path, op, want)
                    tally["checks"]["PASS" if ok else "FAIL"] += 1
                    if not ok:
                        failures.append(f"{name}: {line}\n        check {detail}")
    finally:
        if client is not None:
            client.close()

    with open(os.path.join(SKILL, "SKILL.md"), encoding="utf-8") as handle:
        skill_text = handle.read()
    leaves, count = leaf_commands(binary, env)
    unmapped = sorted(leaves - mapped_commands(skill_text))
    lines = skill_text.count("\n")
    if unmapped or len(leaves) != count:
        failures.append(f"command map: {len(leaves)} leaves, command_count {count}, missing {unmapped}")
    if lines > 200:
        failures.append(f"SKILL.md has {lines} lines (limit 200)")

    print("\nNOT RUN" if not_run else "\nNOT RUN none")
    for item in not_run:
        print(f"  {item}")
    print("\nRAN AGAIN" if reruns else "\nRAN AGAIN none")
    for item in reruns:
        print(f"  {item} (its warnings said to run OCR again)")
    print("\nFAILURES" if failures else "\nFAILURES none")
    for item in failures:
        print(f"  {item}")
    c, k = tally["commands"], tally["checks"]
    print(f"\ntime: {time.monotonic() - started:.0f} s")
    print(f"commands: {c['PASS']} pass, {c['FAIL']} fail, {c['NOT RUN']} not run")
    print(f"checks: {k['PASS']} pass, {k['FAIL']} fail, {k['NOT RUN']} not run")
    print(f"command map: {len(leaves) - len(unmapped)}/{count} commands named; SKILL.md {lines} lines")
    print("RESULT: " + ("FAIL" if failures else "PASS"))
    if not failures and not args.keep:
        shutil.rmtree(work)
    sys.exit(1 if failures else 0)


if __name__ == "__main__":
    main()
