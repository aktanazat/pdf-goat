# Metadata, outline, links, attachments, accessibility, encryption, size

`IN` the PDF, `W` an empty work dir, `ATTACH` a file to embed.

## 1. Metadata

```bash
pdf-goat --agent meta get "$IN"
pdf-goat --agent meta set "$IN" --set "title=Quarterly Report Q3" --set "author=Finance Team" --set "subject=Q3 results" --set "keywords=revenue, costs" -o "$W/meta.pdf"
pdf-goat --agent meta get "$W/meta.pdf"
```

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

- `bookmarks set` replaces the whole outline. `get links` `target_page` counts from 0.

## 3. Attachments

```bash
pdf-goat --agent attach "$W/meta.pdf" "$ATTACH" -o "$W/attached.pdf"
pdf-goat --agent get attachments "$W/attached.pdf" -o "$W/attachments"
pdf-goat --agent detach "$W/attached.pdf" --name notes.csv -o "$W/detached.pdf"
```

## 4. Accessibility basics

```bash
pdf-goat --agent accessibility check "$W/meta.pdf"
pdf-goat --agent accessibility set "$W/meta.pdf" --title "Quarterly Report Q3" --lang en-US -o "$W/a11y.pdf"
pdf-goat --agent accessibility check "$W/a11y.pdf"
```

- Sets Marked, Lang and Title only; empty `issues` is not a full audit.

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

- Presets downsample images; text-only files usually save 0. `compress` `kept_original: true` means no gain. `repair` `warnings: true` means it rebuilt a damaged file. Render to check legibility.

## 7. Look inside

```bash
pdf-goat --agent count "$IN"
pdf-goat --agent inspect "$IN" --limit 1
pdf-goat --agent get fonts "$IN"
pdf-goat --agent get object "$IN" catalog
```
