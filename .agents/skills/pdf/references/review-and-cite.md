# Review a document and cite page plus rectangle

`IN` the PDF, `W` an empty work dir, both absolute. Numbers below come from a one-page A4 agreement.

## 1. Size it up

```bash
pdf-goat --agent info "$IN"
pdf-goat --agent preflight "$IN"
pdf-goat --agent inspect "$IN"
```

- `info`: `pages`, `needs_password` (true: get the password, then `security decrypt`), `has_text` (false: it is a scan, run `ocr.md` first), `has_forms`, `page_sizes_pt`, `metadata`.
- `preflight`: `risk` and each `findings[].code` (here `untagged` and `missing_language`). Mention findings that matter for the job.
- `inspect`: one entry per page with `rotation`, `width_pt`, `height_pt`, `annotation_count`, `form_field_count`, `link_count`. It pages 25 at a time; follow `next_page` with `--start-page`.

## 2. Read it

```bash
pdf-goat --agent text "$IN"
pdf-goat --agent text "$IN" -o "$W/agreement.txt"
pdf-goat --agent text "$IN" --pages 1
```

- Long documents: write to a file (JSON then carries only `outputs`, counts) and Read it. `--layout` when columns matter.

## 3. Find and cite

```bash
pdf-goat --agent search "$IN" "1,200 USD"
pdf-goat --agent get text-blocks "$IN" --pages 1
```

- `search`: literal substring, ignores case, no regex. `hits[]` with `page`, `rect`; `truncated: true` means `--limit`/`--first` stopped it.
- `get text-blocks`: `blocks[]` with `rect` for citing a paragraph.
- Cite as: page 1, rect [139.9, 191.9, 193.3, 204.8] in points, top-left origin, y down.

```bash
pdf-goat --agent render "$IN" --pages 1 --dpi 144 --clip 50,180,420,215 --mark 139.9,191.9,193.3,204.8 -o "$W/cite"
```

- Open `outputs[0]` and look. On a rotated page drop `--clip` (SKILL.md).

## 4. Find by meaning

```bash
pdf-goat --agent setup status
pdf-goat --agent search "$IN" "monthly fee" --meaning --limit 3
```

- `installed: false`: run `pdf-goat --agent setup meaning` once (~30 MB). Scores only rank: read the hit text.

## 5. Mark up a review

```bash
pdf-goat --agent annotate highlight "$IN" --find "1,200 USD" -o "$W/review-1.pdf"
pdf-goat --agent annotate note "$W/review-1.pdf" --page 1 --at 196,192 --text "Check this amount against the order form." -o "$W/review-2.pdf"
pdf-goat --agent annotate list "$W/review-2.pdf"
```

- `annotate highlight` returns `marks`: the number of matches it highlighted. `0` means the text was not found.
- `annotate list` returns `count` and `annotations[]` with `page`, `type`, `rect`, `content`: here a `Highlight` and a `Text` note with your comment. Highlight rects are padded a few points around the words.

## 6. Academic transcripts

```bash
pdf-goat --agent transcript read "$TRANSCRIPT" --conferred 2026-06-15
pdf-goat --agent transcript resolve --root "$TRANSCRIPTS" --glob "transcript*.pdf"
```

- `freshness.verdict`: `current`, `stale_before_conferral`, `stale_missing_terms`, `unknown_issue_date`, `not_checked`; report `freshness.reason` too. The filename is never evidence.
- `resolve` ranks PDFs in one directory (not recursive) by printed issue date.
