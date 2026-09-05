"""Time pdf-goat CLI verbs on fixed files and record wall time and peak memory.

Each trial launches the installed entry point through `/usr/bin/time -l`, so
the numbers include interpreter start, imports, and the ledger write. Peak
memory is the per-process physical footprint reported by macOS.

    .venv/bin/python benchmarks/cli_benchmark.py --output results.json a.pdf b.pdf
    .venv/bin/python benchmarks/cli_benchmark.py --compare before.json after.json
"""

from __future__ import annotations

import argparse
import json
import os
import shutil
import statistics
import subprocess
import sys
import tempfile
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
CLI = ROOT / ".venv" / "bin" / "pdf-goat-cli"
MANIFEST = ROOT / "benchmarks" / "results" / "viewer-comparison-corpus.json"


def cases(pdf: Path, small: Path, scratch: Path) -> list[tuple[str, list[str]]]:
    out = scratch / f"{pdf.stem}.out.pdf"
    return [
        ("info", ["info", str(pdf)]),
        ("inspect", ["inspect", str(pdf)]),
        ("preflight", ["preflight", str(pdf)]),
        ("count", ["count", str(pdf)]),
        ("text-file", ["text", str(pdf), "-o", str(scratch / f"{pdf.stem}.txt")]),
        ("text-json", ["text", str(pdf)]),
        ("text-layout", ["text", str(pdf), "--layout"]),
        ("search-first", ["search", str(pdf), "the", "--first"]),
        ("search-all", ["search", str(pdf), "the"]),
        ("text-blocks", ["get", "text-blocks", str(pdf)]),
        ("links", ["get", "links", str(pdf)]),
        ("fonts", ["get", "fonts", str(pdf)]),
        ("bookmarks", ["get", "bookmarks", str(pdf)]),
        ("annotations", ["annotate", "list", str(pdf)]),
        ("meta-get", ["meta", "get", str(pdf)]),
        ("access-check", ["accessibility", "check", str(pdf)]),
        ("compare-text", ["compare", "text", str(pdf), str(pdf)]),
        (
            "compare-visual-10p",
            ["compare", "visual", str(small), str(small), "-o", str(scratch / "diff")],
        ),
        (
            "render-p1",
            [
                "render",
                str(pdf),
                "--pages",
                "1",
                "--dpi",
                "150",
                "-o",
                str(scratch / "render"),
            ],
        ),
        ("extract-1-10", ["extract", str(pdf), "--pages", "1-10", "-o", str(out)]),
        ("delete-1", ["delete", str(pdf), "--pages", "1", "-o", str(out)]),
        ("rotate-all", ["rotate", str(pdf), "--deg", "90", "-o", str(out)]),
        ("merge-self", ["merge", str(pdf), str(pdf), "-o", str(out)]),
        ("watermark", ["watermark", str(pdf), "-o", str(out)]),
        ("redact", ["redact", str(pdf), "--find", "zzzzqqqq", "-o", str(out)]),
        ("pages-numbers", ["pages", "numbers", str(pdf), "-o", str(out)]),
        ("meta-set", ["meta", "set", str(pdf), "--set", "title=bench", "-o", str(out)]),
        (
            "sec-encrypt",
            [
                "security",
                "encrypt",
                str(pdf),
                "--password",
                "u",
                "--owner",
                "o",
                "-o",
                str(out),
            ],
        ),
    ]


def run_trial(
    argv: list[str], env: dict[str, str], timeout: float
) -> dict[str, float | int]:
    started = time.perf_counter()
    proc = subprocess.run(
        ["/usr/bin/time", "-l", str(CLI), "--agent", *argv],
        capture_output=True,
        text=True,
        env=env,
        timeout=timeout,
        check=False,
    )
    stats: dict[str, float | int] = {
        "exit": proc.returncode,
        "wall_ms": (time.perf_counter() - started) * 1000,
    }
    for line in proc.stderr.splitlines():
        parts = line.split()
        if len(parts) >= 6 and parts[1] == "real":
            stats["user_ms"] = float(parts[2]) * 1000
            stats["sys_ms"] = float(parts[4]) * 1000
        elif line.endswith("peak memory footprint"):
            stats["peak_mib"] = int(parts[0]) / (1024 * 1024)
        elif line.endswith("maximum resident set size"):
            stats["max_rss_mib"] = int(parts[0]) / (1024 * 1024)
    if proc.returncode != 0:
        stats["stderr"] = proc.stderr[-2000:]
    return stats


def bench(
    files: list[Path], trials: int, timeout: float, only: set[str] | None
) -> dict:
    results = []
    with tempfile.TemporaryDirectory(dir="/private/var/tmp") as tmp:
        scratch = Path(tmp)
        env = {**os.environ, "PDF_GOAT_HOME": str(scratch / "home")}
        for pdf in files:
            small = scratch / f"{pdf.stem}.small.pdf"
            subprocess.run(
                [str(CLI), "extract", str(pdf), "--pages", "1-10", "-o", str(small)],
                capture_output=True,
                env=env,
                check=True,
            )
            for name, argv in cases(pdf, small, scratch):
                if only and name not in only:
                    continue
                samples = []
                for _ in range(trials):
                    for stale in ("diff", "render"):
                        shutil.rmtree(scratch / stale, ignore_errors=True)
                    try:
                        samples.append(run_trial(argv, env, timeout))
                    except subprocess.TimeoutExpired:
                        samples.append({"exit": -1, "timeout": True})
                        break
                ok = [s for s in samples if s.get("exit") == 0]
                row = {
                    "document": pdf.stem,
                    "case": name,
                    "trials": len(samples),
                    "ok": len(ok),
                }
                if ok:
                    row["wall_ms"] = round(
                        statistics.median(s["wall_ms"] for s in ok), 1
                    )
                    row["wall_ms_min"] = round(min(s["wall_ms"] for s in ok), 1)
                    row["peak_mib"] = round(
                        statistics.median(s["peak_mib"] for s in ok), 1
                    )
                else:
                    row["failure"] = samples[-1].get("stderr", "timeout")[-400:]
                results.append(row)
                print(
                    f"{pdf.stem:10} {name:15} {row.get('wall_ms', 'FAIL'):>9} ms"
                    f" {row.get('peak_mib', ''):>8} MiB",
                    file=sys.stderr,
                    flush=True,
                )
    return {"trials": trials, "files": [str(f) for f in files], "results": results}


def compare(before: dict, after: dict) -> None:
    index = {(r["document"], r["case"]): r for r in before["results"]}
    print(
        "| document | case | before ms | after ms | speedup | before MiB | after MiB |"
    )
    print("| --- | --- | --- | --- | --- | --- | --- |")
    ratios = []
    for row in after["results"]:
        old = index.get((row["document"], row["case"]))
        if not old or "wall_ms" not in old or "wall_ms" not in row:
            continue
        ratio = old["wall_ms"] / row["wall_ms"]
        ratios.append(ratio)
        print(
            f"| {row['document']} | {row['case']} | {old['wall_ms']:.0f} | {row['wall_ms']:.0f}"
            f" | {ratio:.2f}x | {old['peak_mib']:.0f} | {row['peak_mib']:.0f} |"
        )
    if ratios:
        print(f"\ngeometric mean speedup: {statistics.geometric_mean(ratios):.2f}x")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.split("\n", 1)[0])
    parser.add_argument("files", nargs="*", type=Path)
    parser.add_argument("--trials", type=int, default=3)
    parser.add_argument("--timeout", type=float, default=600)
    parser.add_argument("--only", help="comma-separated case names")
    parser.add_argument("--output", type=Path)
    parser.add_argument("--compare", nargs=2, type=Path, metavar=("BEFORE", "AFTER"))
    args = parser.parse_args()
    if args.compare:
        compare(*(json.loads(p.read_text()) for p in args.compare))
        return 0
    files = args.files or [
        Path(d["path"])
        for d in json.loads(MANIFEST.read_text())["documents"]
        if Path(d["path"]).exists()
    ]
    only = set(args.only.split(",")) if args.only else None
    report = bench(files, args.trials, args.timeout, only)
    text = json.dumps(report, indent=2)
    if args.output:
        args.output.write_text(text)
    else:
        print(text)
    return 0


if __name__ == "__main__":
    sys.exit(main())
