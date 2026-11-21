# Word, Excel, PowerPoint, HTML, tables, audio

`IN` a PDF, `W` an empty work dir. Helpers are listed under SKILL.md limits; a missing one fails naming it.

```bash
pdf-goat --agent convert docx "$IN" -o "$W/report.docx"
pdf-goat --agent convert xlsx "$IN" -o "$W/report.xlsx"
pdf-goat --agent convert pptx "$IN" -o "$W/report.pptx"
pdf-goat --agent convert html "$IN" -o "$W/report.html"
pdf-goat --agent convert tables "$IN" -o "$W/tables"
pdf-goat --agent convert audio "$IN" -o "$W/report.aiff"
```

- `convert tables` writes `p<page>_t<n>.csv` per ruled table; unruled tables are missed. Open outputs to check.

```bash
pdf-goat --agent convert from-office "$W/report.docx" -o "$W/report-back.pdf"
pdf-goat --agent compare text "$IN" "$W/report-back.pdf"
```

`office run` executes a Python script inside LibreOffice with `document` bound; run only scripts you wrote or read.

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

- `--new writer|calc|impress` or `--input FILE`; returns the script's `stdout`/`stderr`; `--timeout N` seconds.
