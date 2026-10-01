# Metadata, outline, links, attachments, accessibility, encryption, size

Set `IN` to the PDF, `W` to an empty work directory and `ATTACH` to a file to embed, all absolute. The example `IN` is a 3-page report titled "Quarterly Report".

## 1. Metadata

```bash
pdf-goat --agent meta get "$IN"
pdf-goat --agent meta set "$IN" --set "title=Quarterly Report Q3" --set "author=Finance Team" --set "subject=Q3 results" --set "keywords=revenue, costs" -o "$W/meta.pdf"
pdf-goat --agent meta get "$W/meta.pdf"
```

- `metadata` holds `format`, `title`, `author`, `subject`, `keywords`, `creator`, `producer`, `creationDate`, `modDate` when present. `meta set` returns the new `metadata`; `meta get` on the output must show the same values.
- `has_xmp` is true when the file also carries an XMP metadata stream (false for this report).
- `meta strip` removes everything but `format` (see `redact.md`).

## 2. Outline and links

```json $W/outline.json
[{"level": 1, "title": "Quarterly Report", "page": 1}, {"level": 2, "title": "Second Page", "page": 2}, {"level": 1, "title": "Third Page", "page": 3}]
```

```bash
pdf-goat --agent bookmarks set "$W/meta.pdf" --data "$W/outline.json" -o "$W/outline.pdf"
pdf-goat --agent get bookmarks "$W/outline.pdf"
pdf-goat --agent links add "$W/outline.pdf" --page 1 --rect 62,72,253,96 --goto 3 -o "$W/link1.pdf"
pdf-goat --agent links add "$W/link1.pdf" --page 3 --rect 62,72,253,96 --uri https://example.com/q3 -o "$W/links.pdf"
pdf-goat --agent get links "$W/links.pdf"
pdf-goat --agent links remove "$W/links.pdf" --external-only -o "$W/internal-links.pdf"
pdf-goat --agent bookmarks clear "$W/outline.pdf" -o "$W/no-outline.pdf"
```

- `bookmarks set` replaces the whole outline: `count` 3, and `get bookmarks` returns the same list (`level`, `title`, `page`, 1-based).
- `links add` returns `page`. `get links` lists `page`, `rect` (search frame), `uri` and `target_page`, which counts from 0: the `--goto 3` link reads `target_page: 2`.
- `links remove` reports `removed` (here 1) and `external_only`; `bookmarks clear` reports `removed` (here 3).
- Find a link's rect with `search` on the words it should cover.

## 3. Attachments

```bash
pdf-goat --agent attach "$W/meta.pdf" "$ATTACH" -o "$W/attached.pdf"
pdf-goat --agent get attachments "$W/attached.pdf" -o "$W/attachments"
pdf-goat --agent detach "$W/attached.pdf" --name notes.csv -o "$W/detached.pdf"
```

- `attach` reports `attached` (the file name). `get attachments` writes each one into the directory: `count` 1 and `outputs` `$W/attachments/notes.csv`, byte-identical to the original (`cmp` them).
- `detach` reports `removed` (`["notes.csv"]`) and `count`; `--all` removes every attachment.

## 4. Accessibility basics

```bash
pdf-goat --agent accessibility check "$W/meta.pdf"
pdf-goat --agent accessibility set "$W/meta.pdf" --title "Quarterly Report Q3" --lang en-US -o "$W/a11y.pdf"
pdf-goat --agent accessibility check "$W/a11y.pdf"
```

- `accessibility check`: `tagged`, `has_title`, `title`, `has_lang`, `lang`, `images`, `images_without_alt`, `issues`. Here `issues` `["untagged", "no_lang"]` before and `[]` after.
- `accessibility set` sets the Marked flag, Lang and Title only; its `note` says it builds no tag tree. An empty `issues` list is not a full accessibility audit: say so.

## 5. Passwords and permissions

```bash
pdf-goat --agent security encrypt "$W/meta.pdf" --password "$PDF_PW" -o "$W/locked.pdf"
pdf-goat --agent preflight "$W/locked.pdf"
pdf-goat --agent security decrypt "$W/locked.pdf" --password "$PDF_PW" -o "$W/unlocked.pdf"
pdf-goat --agent info "$W/unlocked.pdf"
pdf-goat --agent security permissions "$W/meta.pdf" --owner "$PDF_PW" --no-print --no-copy -o "$W/restricted.pdf"
pdf-goat --agent info "$W/restricted.pdf"
```

- Keep the password in an environment variable (`PDF_PW` here) and never print it.
- `security encrypt`: `algorithm` `AES-256`; `--owner` sets a separate owner password. On the locked file `info`, `text` and `inspect` fail, and `preflight` returns `needs_password: true` with `risk: "unknown"`.
- `security decrypt` returns `ok`; `info` on the result shows `encrypted: false` and the page count. A wrong password fails with `PasswordError: …: invalid password`.
- `security permissions` returns `no_print`, `no_copy`, `no_modify`; the file opens without a password and `info` `permissions` shows `print` and `copy` false. Readers are trusted to honour these flags; they are not protection.

## 6. Size and damage

```bash
pdf-goat --agent optimize reduce "$IN" --preset screen -o "$W/smaller.pdf"
pdf-goat --agent compress "$IN" --level /screen -o "$W/compressed.pdf"
pdf-goat --agent repair "$IN" -o "$W/repaired.pdf"
pdf-goat --agent info "$W/smaller.pdf"
pdf-goat --agent render "$W/smaller.pdf" --pages 1 --dpi 96 -o "$W/smaller-look"
```

- `optimize reduce`: `preset` (`screen`, `ebook` default, `printer`, `prepress`), `original_bytes`, `reduced_bytes`, `ratio`, `saved_bytes`. Presets downsample images: a 200 dpi two-page scan went from 76,401 to 25,143 bytes with `screen`. A text-only file usually saves 0 bytes.
- `compress` also reports `linearized` and `kept_original` (true when the result would not be smaller, so the original bytes are written).
- `repair` rewrites the file's structure; `warnings` is true when the input was damaged and had to be rebuilt, false when it loaded cleanly.
- Read the result back with `info` (same `pages`) and render a page to check images are still legible at the size it will be used.

## 7. Look inside

```bash
pdf-goat --agent count "$IN"
pdf-goat --agent inspect "$IN" --limit 1
pdf-goat --agent get fonts "$IN"
pdf-goat --agent get object "$IN" catalog
```

- `count`: `pages`, `words`, `chars`. `inspect`: per page `width_pt`, `height_pt`, `rotation`, `text_chars`, `word_count`, `image_count`, `link_count`, `annotation_count`, `form_field_count`; `truncated` and `next_page` page through long files.
- `get fonts`: `name`, `type`, `ext`, `encoding` per font. `get object` prints an object (`catalog`, `trailer`, `page:N` or a number) as text, for questions no other command answers.
