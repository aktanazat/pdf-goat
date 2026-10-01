---
name: pdf
description: "PDF jobs with pdf-goat, through the CLI (`pdf-goat --agent ...`) or the `pdf-goat` MCP tools (capabilities, run, render): read, search and cite page plus rectangle; fill forms; sign paper forms; certificate-sign with PKCS#12, timestamp, LTV or certify, then verify; redact and prove it; OCR scans to searchable PDF/A; compare versions; organize pages; make PDFs from Markdown, HTML and images; metadata, bookmarks, links, attachments, accessibility, encryption, size; Office conversion. Every change is read back and looked at before it is reported."
allowed-tools: "Read Bash"
argument-hint: "<command> [sub] <file> [options]"
metadata:
  bins:
    - pdf-goat
hide: true
---

# PDF work with pdf-goat

`pdf-goat` does every PDF step: 106 commands that read, render, edit, fill, sign, redact, OCR, convert, compare and optimize. Run it from PATH, or through the `pdf-goat` launcher in the root of its repository after `cargo build --release -p pdf-goat`. Each workflow has a reference file (listed at the end) with exact commands and the fields to check.

## Calling it

CLI: put `--agent` before the command and one JSON object comes back.

```bash
pdf-goat --agent capabilities
pdf-goat --agent capabilities edit
pdf-goat --agent info "$IN"
```

- Success: exit 0, `"ok": true`, `inputs`, `outputs` (every file written) and the command's own fields.
- Failure: exit 1, `"ok": false`, `error` (report it as written), empty `outputs`.
- `capabilities` lists 16 families and 25 top-level commands (`command_count` 106). `capabilities <family or command>` adds `schemas` with every argument, default and help line: read it before an unfamiliar command.
- `pdf-goat --agent jobs --limit 5` shows the latest runs from the ledger.

MCP: when the `pdf-goat` server is connected (stdio, binary `pdf-goat-mcp` beside `pdf-goat`), the same commands run as three tools, one pdf-goat process per call. Clients list them under the server name; omp shows `xd://mcp__pdf_goat_capabilities`, `xd://mcp__pdf_goat_run` and `xd://mcp__pdf_goat_render`.

```mcp
capabilities {"selector": "edit"}
run {"command": "search", "args": ["$IN", "1,200 USD"]}
render {"file": "$IN", "page": 1, "dpi": 144, "clip": "50,180,420,215", "marks": ["139.9,191.9,193.3,204.8"]}
```

- `run`: `command` is the command path (`info`, `edit add-text`, `security sign`); `args` is the rest of the command line, one token per item, exactly as the CLI takes it. It returns pdf-goat's JSON; a nonzero exit or `ok: false` comes back as an error result with pdf-goat's message. The first 4 PNG or JPEG outputs (up to 5 MiB each) come back as images. It replaces an existing `-o` file, and `office run` executes scripts.
- `render`: `page` (default 1), `dpi` (default 96), optional `clip` and `marks` as in the CLI, each rectangle as `"x0,y0,x1,y1"` or as an array such as a search `rect`, passed as is; returns the PNG and the JSON.

## Ground rules

- Absolute paths in every argument.
- Never write over an input. Give every change a new `-o` path and chain them (`step1.pdf`, `step2.pdf`); an existing `-o` file is replaced without asking.
- Pages are 1-based and ranges look like `2-5,9`. Exception: `get links` reports `target_page` from 0.
- Coordinates are points (72 per inch). The search frame starts at the crop box's top-left corner, y down, page rotation ignored. `search`, `get text-blocks`, `form list`, `annotate list` and `get links` report it; `edit add-text --at/--rect` (and the `bbox` it returns), `edit add-image --rect`, every `annotate` position, `form create-* --rect`, `links add --rect`, `security sign --rect` and `render --mark` take it.
- `render --clip` is in the page as displayed (rotation applied). With `inspect` `rotation` 0 the two frames agree. On a rotated page leave `--clip` off and rely on `--mark`, or convert a search rect, with W and H the unrotated crop width and height: 90 gives `H-y1,x0,H-y0,x1`, 180 gives `W-x1,H-y1,W-x0,H-y0`, 270 gives `y0,W-x1,y1,W-x0`.
- Other frames: `pages crop --box` is top-left, y down, from the media box; `pages boxes --rect` is raw PDF space (bottom-left origin); `compare visual` `bbox` is pixels at its `--dpi` (points = pixels × 72 / dpi).
- Matching: `search` and `annotate highlight|underline|strikeout --find` match literal text, ignoring case. `edit text --find` matches literal text, case included (`replacements: 0` means no match). `redact --find` is a case-insensitive regex over the page's words; `text --mask` and `compare text --mask` take regexes too. In all three a space matches any run of whitespace, line breaks included, so `Acme Corp` also finds the name split across two lines.
- Only pdf-goat touches PDF bytes: no PyMuPDF, pypdf, pdfplumber, poppler, qpdf, Ghostscript, Tesseract, `sips` or pdf-lib. If pdf-goat cannot do a step, stop and name the missing capability; never hand back an unchecked file.
- Fill and edit first, certificate-sign last: after a signature, any change except another `security sign` (even a form fill on a file certified for form filling) makes `security verify` report `intact: false`.
- A .p12 password goes in an environment variable named by `--password-env`. Never print a password in a reply or report.

## Command map

F is a PDF, R a page range, `…` a rect `x0,y0,x1,y1`. Every command that writes takes `-o` (a directory for `render`, `split`, `compare visual`, `convert tables`, `get images`, `get attachments`).

- Look: `info F` · `inspect F [--start-page N --limit N]` · `preflight F` · `count F` · `render F [--pages R --dpi N --format png|jpg|ppm --clip … --mark …]`
- Read: `text F [--layout --mask RE]` · `search F QUERY [--pages R --first --limit N --meaning]` · `get text-blocks F [--pages R --max-blocks N --start-block N]` · `transcript read F [--conferred DATE]` · `transcript resolve --root DIR [--glob PAT]`
- Inside: `get fonts F` · `get images F` · `get object F catalog|trailer|page:N|NUM [--max-bytes N]` · `get attachments F` · `get bookmarks F` · `get links F`
- Organize: `merge FILES…` · `split F [--every N]` · `extract F --pages R` · `delete F --pages R` · `reorder F --order 3,1,2` · `rotate F --deg 90 [--pages R]`
- Pages: `pages blank F [--at N --count N]` · `pages duplicate F --pages R` · `pages insert F --source G [--at N]` · `pages replace F --source G --pages R` · `pages scale F --factor X` · `pages nup F --n 2|4` · `pages booklet F` · `pages crop F --box … [--pages R]` · `pages boxes F --box media|crop|trim|bleed --rect … [--pages R]` · `pages flatten F`
- Stamp: `pages header F --text T [--align --size --color]` · `pages footer F --text T` (`{page}`, `{pages}`) · `pages numbers F [--format T --start N --align --size]` · `pages bates F [--prefix P --start N --digits N --size N]` · `watermark F [--text T --size N --opacity X --angle N]` · `overlay F STAMP`
- Write: `edit add-text F --text T (--at x,y | --fit --rect …) [--page N | --pages R, --font --face --size --width --align --rotate --opacity --color]` · `edit add-image F --image PNG|JPG --rect … [--stretch --rotate --opacity --page --pages]` · `edit text F --find T --replace T`
- Annotate: `annotate highlight|underline|strikeout F --find T [--pages R --color C]` · `annotate note F --page N --at x,y --text T` · `annotate textbox F --rect … --text T [--size --color --fill]` · `annotate rect|circle F --rect … [--color --fill --width]` · `annotate line|arrow F --start x,y --end x,y` · `annotate ink F --points "x,y;x,y" [--color --width]` · `annotate polygon F --points …` · `annotate stamp F --rect … [--stamp 0-13]` · `annotate callout F --rect … --target x,y --text T` · `annotate area-highlight F --rect … [--opacity X]` · `annotate list F` · `annotate flatten F` · `annotate delete F [--type T --pages R]`
- Forms: `form list F` · `form fill F --data JSON [--flatten]` · `form export F --format json|xfdf|fdf` · `form import F --data FILE [--flatten]` · `form create-text F --name N --page N --rect …` · `form create-checkbox F --name N --rect …`
- Protect: `redact F --find RE` · `security sanitize F` · `security encrypt F --password P [--owner P]` · `security decrypt F --password P` · `security permissions F --owner P [--user P --no-print --no-copy --no-modify]`
- Sign: `security sign F [--p12 FILE --password-env VAR --pss --field N --page N --rect … --appearance-text T --appearance-image IMG --name N --reason T --tsa URL --ltv --timeout N --certify 1|2|3]` · `security verify F [--trust PEM --online --timeout N]`
- Properties: `meta get F` · `meta set F --set key=value` · `meta strip F` · `bookmarks set F --data JSON` · `bookmarks clear F` · `links add F --page N --rect … (--uri URL | --goto N)` · `links remove F [--pages R --external-only]` · `attach F FILE` · `detach F (--name N | --all)` · `accessibility check F` · `accessibility set F [--title T --lang L]`
- Create: `from-md FILE [--css CSS]` · `from-html FILE` · `from-images IMAGES…`
- Convert: `convert ocr F [--force]` · `convert pdfa F` · `convert html F` · `convert tables F` · `convert docx F` · `convert xlsx F` · `convert pptx F [--dpi N]` · `convert audio F [--voice V]` · `convert from-office DOCX|XLSX|PPTX`
- Compare: `compare text F OTHERS… [--context N --max-lines N --mask RE]` · `compare visual F OTHER [--dpi N]` · `compare structure F OTHER`
- Size: `optimize reduce F [--preset screen|ebook|printer|prepress]` · `compress F [--level /screen|/ebook|/printer|/prepress]` · `repair F`
- Office: `office run SCRIPT [--input FILE | --new writer|calc|impress] [--timeout N]` · `office export FILE [--timeout N]`
- Tooling: `capabilities [SELECTOR]` · `jobs [--limit N]` · `setup status` · `setup meaning [--force]`

## Verify before you report

1. Locate the target: `search`, `get text-blocks`, `form list` or `inspect` give its page and rect.
2. Mark it before changing anything: `render F --pages N --dpi 144 --clip <area around it> --mark <rect> -o DIR`, then open `outputs[0]` with Read and look. The magenta outline sits just outside the rect.
3. Make the change with a new `-o`; check `ok` and the fields below.
4. Read it back with a different command from the one that wrote it.
5. Render the changed region with `--mark` on the returned `bbox` or rect and look at it.
6. Report the output path, each change's page and rect, and what you checked.

| Change | Fields to check | Read back with |
|---|---|---|
| text or image placed | `bbox`, `placements`, `font`, `size` | `search` finds the text inside `bbox`; render with `--mark` |
| form filled | `fields_set` (fields it set), `unknown_fields` (keys no field has; must be empty) | `form list` `value` and `checked` per field |
| form flattened | `flattened: true` | `form list` `field_count` 0; `text` or `search` shows the values |
| redaction | `redactions`, `field_redactions` | `search` count 0, `text` lacks it, render shows a black box |
| certificate signature | `signer`, `self_signed`, `pades_level`, `timestamp`, `dss` | `security verify`, every field as returned |
| pages moved | `merged_pages`, `order`, `deleted_pages`, `rotated_pages` | `info` `pages`, `text` per page, `inspect` `rotation` |
| OCR | `standard`, `warnings` | `info` `has_text`, `search` a word you can see |
| metadata, outline, links | `count`, `removed` | `meta get`, `get bookmarks`, `get links` |
| encryption | `algorithm` | `info` fails without the password; `security decrypt` |
| size | `saved_bytes`, `ratio` | `info` and a render of one page |

## Honest limits

- Rendering and colour fidelity are still being improved: judge renders for position and legibility, and never promise pixel-exact output.
- `edit text` replaces simple text runs only; it does not reflow or match embedded fonts. `replacements: 0` means nothing changed, though `ok` is true.
- `convert pdfa` writes PDF/A-2b and reports `conformance_validated: false`. `convert ocr` reports `standard` PDF/A-2u or PDF/A-2b (null with a warning if that step failed), also unvalidated. Say so when an archival file is required.
- `accessibility set` sets Marked, Lang and Title; it does not build a tag tree.
- `security verify`: `trusted` is the local policy and `chain_trusted` the certificate chain against the system roots plus any `--trust`. `pades_level` follows the evidence stored in the file, so a certificate without a revocation answer (listed in `dss.unchecked`) holds it at `B-T`, and a critical certificate extension pdf-goat does not process makes `chain_trusted: false` with a `trust_error` naming it. Report `pades_level`, `chain_trusted`, `trust_error`, `revocation` and `modified` exactly as returned, never as a plain "valid signature".
- `security sign` without `--p12` uses a self-signed demo identity (signer "pdf-goat demo", `self_signed: true`): a test, never a real signature.
- `security sanitize` keeps the title and author; run `meta strip` as well.
- `redact` finds text only: OCR a scan first. `convert tables` finds ruled tables only.
- `compress` and `optimize reduce` never grow a file; on an already lean file they save 0 bytes.
- Encrypted input: `info`, `text` and `inspect` fail until `security decrypt`; `preflight` still answers with `needs_password`.
- Helpers: `convert from-office` needs `office2pdf` on PATH; `office run` and `office export` need LibreOffice in /Applications; `convert audio` uses macOS `say`; `search --meaning` needs `setup meaning` once. `convert ocr` needs macOS; elsewhere it fails with "OCR needs the macOS Vision framework". Off macOS, `security verify` checks chains against `--trust` anchors only, so `chain_trusted` is false without one.

## References

- `references/review-and-cite.md`: read, search, cite page and rect, mark up a review, academic transcripts
- `references/fill-form.md`: forms with fields; adding fields to a flat PDF
- `references/sign-paper-form.md`: forms without fields: typed name, check mark, script signature, date, signature image or ink
- `references/certificate-signing.md`: PKCS#12 signatures, visible box, timestamp, LTV, certify, verify
- `references/redact.md`: redact text and scans, and prove it is gone
- `references/ocr.md`: scan to searchable PDF/A
- `references/compare.md`: text, visual and structure comparison
- `references/organize-pages.md`: merge, split, reorder, rotate, insert, impose, stamp, number
- `references/create-pdfs.md`: Markdown, HTML and images to PDF
- `references/document-properties.md`: metadata, bookmarks, links, attachments, accessibility, encryption, size
- `references/office.md`: PDF to and from Word, Excel and PowerPoint; LibreOffice scripts
