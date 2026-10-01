# Review a document and cite page plus rectangle

Set `IN` to the PDF and `W` to an empty work directory, both absolute. The numbers below come from a one-page A4 service agreement; yours will differ.

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
```

- The first returns `pages[]` with `page` and `text`. For a long document use the second: the JSON carries only `outputs`, `page_count` and `char_count`, and you read the file with Read.
- Text comes out in reading order, not layout. Add `--layout` when columns matter.

## 3. Find and cite

```bash
pdf-goat --agent search "$IN" "1,200 USD"
pdf-goat --agent get text-blocks "$IN" --pages 1
```

- `search` returns `count` and `hits[]` with `page` and `rect`: here one hit, page 1, `[139.9, 191.9, 193.3, 204.8]`. `truncated: true` means `--limit` or `--first` stopped early.
- `search` is literal and ignores case; it finds substrings, so "pay" also hits "pays". It takes no regular expressions.
- `get text-blocks` returns `blocks[]` with `page`, `block`, `rect`, `text`: cite a whole paragraph with its block rect (here block 3, `[62.69, 191.94, 404.46, 204.75]`).
- Cite as: page 1, rect [139.9, 191.9, 193.3, 204.8] in points from the page's top-left corner, y down.

Look at what you cite:

```bash
pdf-goat --agent render "$IN" --pages 1 --dpi 144 --clip 50,180,420,215 --mark 139.9,191.9,193.3,204.8 -o "$W/cite"
```

- `outputs[0]` is `$W/cite/agreement_p001.png`; open it with Read. `marks` echoes the rect, outlined in magenta just outside it.
- The clip here equals the search frame because `inspect` reported `rotation` 0. On a rotated page drop `--clip` (see SKILL.md for the conversion).

## 4. Find by meaning

When you know the idea but not the wording:

```bash
pdf-goat --agent setup status
pdf-goat --agent search "$IN" "monthly fee" --meaning --limit 3
```

- `setup status`: `installed: true` means the meaning model is present; otherwise run `pdf-goat --agent setup meaning` once (about 30 MB download).
- `--meaning` returns `mode: "meaning"` and `hits[]` with `page`, `block`, `rect`, `text` and `score`, best first. Here the top hit is the payment sentence. Confirm a meaning hit by reading its text; scores only rank.

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

- `transcript read` returns `document_identity`, `issue_date`, `degree` (`name`, `status`, `conferral_date`), `terms[]` with `courses[]` (`course`, `title`, `grade`, `units`, `points`) and `parse_quality.confidence`.
- `freshness.verdict` compares the printed issue date and the terms with the conferral date you assert: `current`, `stale_before_conferral`, `stale_missing_terms`, `unknown_issue_date`, or `not_checked` without `--conferred`. Report `freshness.reason` with it; the filename is never evidence.
- `transcript resolve` ranks the matching PDFs in one directory (not recursive) by printed issue date: `candidates[]` with `rank`, `path` and `issue_date`.
