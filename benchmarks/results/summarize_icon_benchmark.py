#!/usr/bin/env python3
import json
import statistics
from collections import defaultdict
from pathlib import Path

INPUT = Path(__file__).with_name("icon-benchmark-runs.jsonl")
OFFSETS = [0.25, 0.5, 0.75, 1.0, 2.0, 5.0, 10.0]
records = [json.loads(line) for line in INPUT.read_text().splitlines() if line]
by_arm = defaultdict(list)
by_round = defaultdict(dict)
for record in records:
    by_arm[record["arm"]].append(record)
    by_round[record["round"]][record["arm"]] = record


def sample(record: dict, offset: float) -> float:
    value = next(item["physFootprintBytes"] for item in record["footprint"] if item["targetAfterReadySeconds"] == offset)
    return value / 1_048_576


def values(items: list[dict], metric: str) -> list[float]:
    if metric == "launch":
        return [item["launchToVisibleMilliseconds"] for item in items]
    return [sample(item, float(metric)) for item in items]


print("RUNS")
print("run round pos arm launch_ms " + " ".join(f"mib_{offset:g}s" for offset in OFFSETS))
for record in sorted(records, key=lambda item: item["run"]):
    series = " ".join(f"{sample(record, offset):.3f}" for offset in OFFSETS)
    print(f"{record['run']:02d} {record['round']} {record['position']} {record['arm']} {record['launchToVisibleMilliseconds']:.3f} {series}")

print("\nSUMMARY")
for arm in ["baseline", "no_runtime_icon", "bundle_icon"]:
    items = by_arm[arm]
    launch = values(items, "launch")
    print(f"{arm} launch_ms median={statistics.median(launch):.3f} range={min(launch):.3f}..{max(launch):.3f} values=" + ",".join(f"{v:.3f}" for v in launch))
    for offset in OFFSETS:
        series = values(items, str(offset))
        print(f"{arm} footprint_{offset:g}s_mib median={statistics.median(series):.3f} range={min(series):.3f}..{max(series):.3f} values=" + ",".join(f"{v:.3f}" for v in series))
    peaks = [max(point["peakPhysFootprintBytes"] for point in item["footprint"]) / 1_048_576 for item in items]
    print(f"{arm} peak_mib median={statistics.median(peaks):.3f} range={min(peaks):.3f}..{max(peaks):.3f} values=" + ",".join(f"{v:.3f}" for v in peaks))

print("\nPAIRED ROUND DELTAS (baseline minus candidate)")
for candidate in ["no_runtime_icon", "bundle_icon"]:
    for metric in ["launch", "0.75", "2.0"]:
        deltas = []
        for round_number in sorted(by_round):
            baseline = values([by_round[round_number]["baseline"]], metric)[0]
            comparison = values([by_round[round_number][candidate]], metric)[0]
            deltas.append(baseline - comparison)
        unit = "ms" if metric == "launch" else "MiB"
        print(f"{candidate} {metric} median_delta={statistics.median(deltas):.3f}{unit} values=" + ",".join(f"{value:.3f}" for value in deltas))
