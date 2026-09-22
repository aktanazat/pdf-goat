"""Private Python macro copied into a disposable LibreOffice user profile."""

import contextlib
import io
import json
import os
import sys
import traceback
from pathlib import Path

import uno


FILTERS = {
    "com.sun.star.text.TextDocument": {
        ".docx": "MS Word 2007 XML",
        ".odt": "writer8",
        ".pdf": "writer_pdf_Export",
    },
    "com.sun.star.sheet.SpreadsheetDocument": {
        ".xlsx": "Calc MS Excel 2007 XML",
        ".ods": "calc8",
        ".pdf": "calc_pdf_Export",
    },
    "com.sun.star.presentation.PresentationDocument": {
        ".pptx": "Impress MS PowerPoint 2007 XML",
        ".odp": "impress8",
        ".pdf": "impress_pdf_Export",
    },
}


def prop(name, value):
    item = uno.createUnoStruct("com.sun.star.beans.PropertyValue")
    item.Name = name
    item.Value = value
    return item


def edit_document(desktop, request):
    document = desktop.loadComponentFromURL(request["url"], "_blank", 0, (
        prop("Hidden", True), prop("MacroExecutionMode", 0),
        prop("UpdateDocMode", 0), prop("ReadOnly", not bool(request["output"])),
    ))
    if document is None:
        raise RuntimeError("LibreOffice could not open the document")
    try:
        formats = next((filters for service, filters in FILTERS.items()
                        if document.supportsService(service)), None)
        if formats is None:
            raise RuntimeError("only Writer, Calc, and Impress documents are supported")
        output = request["output"]
        suffix = Path(output).suffix.lower()
        if output and suffix not in formats:
            raise RuntimeError("output format for this document must be " + ", ".join(formats))
        if request["script"]:
            previous = sys.argv
            sys.argv = [request["script"]]
            try:
                # PyUNO keeps its runtime in sys.modules["__main__"]; do not replace it.
                exec(compile(Path(request["script"]).read_bytes(), request["script"], "exec"), {
                    "__name__": "__main__", "__file__": request["script"],
                    "document": document, "desktop": desktop, "uno": uno, "prop": prop,
                })
            finally:
                sys.argv = previous
        if output:
            document.storeToURL(Path(output).as_uri(), (
                prop("FilterName", formats[suffix]), prop("Overwrite", True),
            ))
    finally:
        document.dispose()


def main(*args):
    desktop = XSCRIPTCONTEXT.getDesktop()
    job = Path(os.environ["PDF_GOAT_OFFICE_JOB"])
    stdout, stderr = io.StringIO(), io.StringIO()
    result = {}
    try:
        with contextlib.redirect_stdout(stdout), contextlib.redirect_stderr(stderr):
            edit_document(desktop, json.loads(job.read_text(encoding="utf-8")))
    except BaseException:
        # SystemExit from a supplied script is a failed job, not an office shutdown.
        result["error"] = traceback.format_exc()
    finally:
        result.update(stdout=stdout.getvalue(), stderr=stderr.getvalue())
        try:
            job.with_name("result.json").write_text(json.dumps(result), encoding="utf-8")
        finally:
            desktop.terminate()


g_exportedScripts = (main,)
