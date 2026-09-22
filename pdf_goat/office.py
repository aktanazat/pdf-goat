"""Standalone LibreOffice jobs; never attach to the interactive office profile."""

import json
import os
import shutil
import signal
import subprocess
import tempfile
from contextlib import ExitStack
from pathlib import Path


class OfficeError(Exception):
    pass


def _stop(process):
    """Reap the child and stop descendants in this job's private process group."""
    try:
        os.killpg(process.pid, signal.SIGTERM)
    except ProcessLookupError:
        pass
    try:
        process.wait(timeout=3)
    except subprocess.TimeoutExpired:
        pass
    finally:
        try:
            os.killpg(process.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        process.wait()


def _cancel(signum, frame):
    raise SystemExit(128 + signum)


def run_office(*, source, kind, script, output, timeout):
    if timeout <= 0:
        raise OfficeError("timeout must be a positive number of seconds")
    soffice = Path("/Applications/LibreOffice.app/Contents/MacOS/soffice")
    if not soffice.is_file():
        raise OfficeError(
            "office commands require LibreOffice for macOS in /Applications/LibreOffice.app"
        )
    protected = [path for path in (source, script) if path is not None]
    if output is not None:
        for path in protected:
            if output == path or output.exists() and output.samefile(path):
                raise OfficeError("output must differ from the input document and script")
        output.parent.mkdir(parents=True, exist_ok=True)

    with ExitStack() as resources:
        previous = signal.signal(signal.SIGTERM, _cancel)
        resources.callback(signal.signal, signal.SIGTERM, previous)
        work = Path(resources.enter_context(tempfile.TemporaryDirectory(
            prefix="pdf-goat-office-", dir="/private/var/tmp"
        )))
        if source is not None:
            snapshot = work / ("input" + source.suffix)
            shutil.copyfile(source, snapshot)
            url = snapshot.as_uri()
        else:
            url = {
                "writer": "private:factory/swriter",
                "calc": "private:factory/scalc",
                "impress": "private:factory/simpress",
            }[kind]
        staged = None
        if output is not None:
            staging = Path(resources.enter_context(tempfile.TemporaryDirectory(
                prefix=".pdf-goat-office-", dir=output.parent
            )))
            staged = staging / output.name
        profile = work / "profile"
        macros = profile / "user/Scripts/python"
        macros.mkdir(parents=True)
        shutil.copyfile(Path(__file__).with_name("office_worker.py"), macros / "pdf_goat_job.py")
        request = work / "job.json"
        request.write_text(json.dumps({
            "url": url, "script": str(script) if script else "",
            "output": str(staged) if staged else "",
        }), encoding="utf-8")
        log = resources.enter_context((work / "soffice.log").open("w+"))
        with ExitStack() as processes:
            office = subprocess.Popen(
                [str(soffice), f"-env:UserInstallation={profile.as_uri()}",
                 "--headless", "--norestore", "--nologo", "--nodefault",
                 "vnd.sun.star.script:pdf_goat_job.py$main?language=Python&location=user"],
                env=dict(os.environ, PDF_GOAT_OFFICE_JOB=str(request)),
                stdin=subprocess.DEVNULL, stdout=log, stderr=log, start_new_session=True,
            )
            processes.callback(_stop, office)
            try:
                office.wait(timeout=timeout)
            except subprocess.TimeoutExpired as error:
                raise OfficeError(f"LibreOffice job exceeded {timeout} seconds") from error
            receipt = work / "result.json"
            if office.returncode != 0 or not receipt.is_file():
                log.seek(0)
                raise OfficeError(
                    f"LibreOffice job failed (exit {office.returncode}): {log.read().strip()}"
                )
            result = json.loads(receipt.read_text(encoding="utf-8"))
            if "error" in result:
                raise OfficeError(f"LibreOffice job failed: {result['error']}")
        if staged is not None:
            if not staged.is_file():
                raise OfficeError("LibreOffice did not produce the requested output")
            os.replace(staged, output)
    return result
