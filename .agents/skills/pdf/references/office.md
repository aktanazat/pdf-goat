# Word, Excel, PowerPoint, HTML, tables, audio

Set `IN` to a PDF and `W` to an empty work directory, both absolute. The example `IN` is a 3-page report whose page 2 holds a ruled table (Item / Amount, Rent 1000, Power 250).

Helpers: `convert from-office` needs `office2pdf` on PATH; `office run` and `office export` need LibreOffice in /Applications; `convert audio` uses macOS `say`. A missing helper fails with an error naming it: report that.

## 1. PDF to other formats

```bash
pdf-goat --agent convert docx "$IN" -o "$W/report.docx"
pdf-goat --agent convert xlsx "$IN" -o "$W/report.xlsx"
pdf-goat --agent convert pptx "$IN" -o "$W/report.pptx"
pdf-goat --agent convert html "$IN" -o "$W/report.html"
pdf-goat --agent convert tables "$IN" -o "$W/tables"
pdf-goat --agent convert audio "$IN" -o "$W/report.aiff"
```

- `convert docx`: editable paragraphs placed as on the page; ruled tables stay tables.
- `convert xlsx`: `sheets` 1. `convert pptx`: `slides` 3, one per page.
- `convert tables`: `tables` 1 and one CSV per ruled table, named `p<page>_t<n>.csv`: here `$W/tables/p2_t1.csv` holding `Item,Amount`, `Rent,1000`, `Power,250`. Tables without ruling lines are not found.
- `convert audio`: reads the text aloud with `say` into AIFF: `chars`, `duration_sec` and `voice` (null for the system voice); `--voice NAME` picks one.
- Check each file: open the CSV or HTML with Read, or convert the Word file back (section 2) and compare.

## 2. Office to PDF

```bash
pdf-goat --agent convert from-office "$W/report.docx" -o "$W/report-back.pdf"
pdf-goat --agent compare text "$IN" "$W/report-back.pdf"
```

- `engine` `office2pdf`, `pages`, `warnings` (empty, or report them).
- `compare text` shows what survived the round trip; render a page of each to compare the layout.

## 3. LibreOffice scripts

`office run` executes a Python script inside LibreOffice with `document` bound to the open document. Run only scripts you wrote or have read.

```python $W/letter.py
text = document.getText()
text.insertString(text.getEnd(), "Board minutes 2026-09-30. Motion carried.", False)
print(len(text.getString()))
```

```bash
pdf-goat --agent office run "$W/letter.py" --new writer -o "$W/letter.docx"
pdf-goat --agent office export "$W/letter.docx" -o "$W/letter.pdf"
pdf-goat --agent text "$W/letter.pdf"
```

- `office run`: `--new writer|calc|impress` starts an empty document, `--input FILE` opens one; the result is saved to `-o`. It returns the script's `stdout` and `stderr`: here `stdout` is `41`, the text length.
- `office export` converts the document to PDF with LibreOffice; `text` then shows "Board minutes 2026-09-30. Motion carried."
- `--timeout N` (seconds) bounds either command.
