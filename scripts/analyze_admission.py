#!/usr/bin/env python3
"""Compare sampled admission logs with exported, committed usage. Local files only."""
import argparse
from collections import Counter, defaultdict
import csv
import json
import math
from pathlib import Path
import uuid

EXTERNAL_NAMESPACE = uuid.UUID("1966acaf-9f95-5dab-b8f0-03a51c091314")


def records(path):
    """Read JSON, JSONL, Grafana JSON log exports, or flat usage CSV."""
    text = path.read_text(encoding="utf-8")
    if path.suffix.lower() == ".csv":
        yield from csv.DictReader(text.splitlines())
        return
    try:
        value = json.loads(text)
    except json.JSONDecodeError:
        value = [json.loads(line) for line in text.splitlines() if line.strip()]
    if isinstance(value, dict) and isinstance(value.get("data"), list):
        value = value["data"]
    for row in value if isinstance(value, list) else [value]:
        if not isinstance(row, dict):
            raise ValueError("input records must be objects")
        line = row.get("line", row.get("Line"))
        if isinstance(line, str):
            try:
                row = json.loads(line)
            except json.JSONDecodeError:
                continue  # Other services' plain-text log lines are not samples.
        if isinstance(row, dict):
            fields = row.get("fields", row)
            if isinstance(fields, dict):
                yield fields


def amount(value):
    if isinstance(value, bool) or not isinstance(value, (int, str)):
        raise ValueError("costs must be integer nano-USD")
    result = int(value)
    if result < 0:
        raise ValueError("costs must be non-negative")
    return result


def percentile(values, fraction):
    return sorted(values)[max(0, math.ceil(len(values) * fraction) - 1)] if values else None


def statistics(rows):
    paired = [r for r in rows if r["status"] == "matched"]
    differences = [r["actual_nano_usd"] - r["estimated_nano_usd"] for r in paired]
    ratios = [r["actual_nano_usd"] / r["estimated_nano_usd"] for r in paired if r["estimated_nano_usd"]]
    return {
        "samples": len(rows), "matched": len(paired),
        "match_rate": len(paired) / len(rows) if rows else None,
        "statuses": dict(Counter(r["status"] for r in rows)),
        "underestimated_rate": sum(d > 0 for d in differences) / len(paired) if paired else None,
        "underestimated_nano_usd": sum(max(d, 0) for d in differences),
        "underestimated_p95_nano_usd": percentile([max(d, 0) for d in differences], .95),
        "underestimated_max_nano_usd": max(differences, default=0) if any(d > 0 for d in differences) else 0,
        "overestimated_nano_usd": sum(max(-d, 0) for d in differences),
        "actual_to_estimate_p50": percentile(ratios, .5),
        "actual_to_estimate_p95": percentile(ratios, .95),
        "zero_estimate_positive_actual": sum(r["estimated_nano_usd"] == 0 and r["actual_nano_usd"] > 0 for r in paired),
        "estimated_p95_nano_usd": percentile([r["estimated_nano_usd"] for r in paired], .95),
        "actual_p95_nano_usd": percentile([r["actual_nano_usd"] for r in paired], .95),
    }


def analyze(logs, bills, organization_id):
    estimates, links, endings = {}, defaultdict(list), defaultdict(set)
    for event in logs:
        kind, attempt = event.get("event"), event.get("attempt_id")
        if not attempt:
            continue
        if kind == "usage_admission_estimate" and event.get("organization_id") == organization_id:
            estimate = {key: event.get(key) for key in (
                "path", "mode", "model_id", "inference_type", "input_kind", "estimator_version", "sample_rate")}
            estimate["estimated_nano_usd"] = amount(event.get("estimated_nano_usd"))
            if attempt in estimates and estimates[attempt] != estimate:
                raise ValueError("conflicting estimates for an execution")
            estimates[attempt] = estimate
        elif kind == "usage_admission_usage" and event.get("organization_id") == organization_id:
            links[attempt].append(event)
        elif kind == "usage_admission_attempt_end":
            endings[attempt].add(event.get("outcome"))

    ledger, by_inference = {}, defaultdict(set)
    for bill in bills:
        if bill.get("source") == "service":
            continue
        if bill.get("organization_id", organization_id) != organization_id:
            continue
        usage = bill.get("inference", bill)
        row_id = bill.get("id")
        if not row_id:
            raise ValueError("usage rows must include their ledger id")
        normalized = {
            "actual_nano_usd": amount(bill.get("total_cost_nano_usd", bill.get("total_cost"))),
            "model": usage.get("model", ""), "inference_id": usage.get("inference_id"),
        }
        if row_id in ledger and ledger[row_id] != normalized:
            raise ValueError("conflicting copies of a ledger row")
        ledger[row_id] = normalized
        if normalized["inference_id"]:
            by_inference[normalized["inference_id"]].add(row_id)

    rows = []
    for attempt, estimate in sorted(estimates.items()):
        candidates = set()
        for link in links[attempt]:
            if link.get("usage_id") in ledger:
                candidates.add(link["usage_id"])
            elif link.get("admission_id"):
                candidates.update(by_inference[link["admission_id"]])
            elif link.get("provider_request_id"):
                inference_id = str(uuid.uuid5(EXTERNAL_NAMESPACE, link["provider_request_id"]))
                candidates.update(by_inference[inference_id])
        status = "unmatched"
        if len(candidates) > 1:
            status = "ambiguous"
        elif len(candidates) == 1:
            status = "matched"
        elif "not_executed" in endings[attempt]:
            status = "not_executed"
        elif "unknown" in endings[attempt] or "settlement_failed" in endings[attempt]:
            status = "unknown"
        row = {"attempt_id": attempt, **estimate, "status": status, "usage_id": "", "model": "", "actual_nano_usd": None}
        if status == "matched":
            row["usage_id"] = next(iter(candidates))
            row.update({key: ledger[row["usage_id"]][key] for key in ("model", "actual_nano_usd")})
        rows.append(row)

    # A reused provider ID must not make several executions look like one paid bill.
    matched_ids = Counter(r["usage_id"] for r in rows if r["status"] == "matched")
    for row in rows:
        if row["status"] == "matched" and matched_ids[row["usage_id"]] > 1:
            row["status"], row["actual_nano_usd"] = "ambiguous", None

    groups = defaultdict(list)
    for row in rows:
        groups[(row["path"], row["mode"], row["model_id"], row["inference_type"], row["input_kind"], row["estimator_version"])].append(row)
    summary = {
        "organization_id": organization_id, **statistics(rows),
        "groups": [
            {"path": key[0], "mode": key[1], "model_id": key[2], "inference_type": key[3], "input_kind": key[4],
             "estimator_version": key[5], "model": next((r["model"] for r in group if r["model"]), ""), **statistics(group)}
            for key, group in groups.items()
        ],
        "notes": [
            "Matched samples only; amounts are not population totals or an estimate of enforce rejection rates.",
            "Unmatched records may reflect delayed settlement, incomplete exports, cancellation or missing usage; their actual cost is unknown.",
            "Verify complete log windows and all usage export pages; this tool cannot detect server-side export truncation.",
        ],
    }
    return summary, rows


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--logs", type=Path, nargs="+", required=True)
    parser.add_argument("--usage", type=Path, nargs="+", required=True)
    parser.add_argument("--organization-id", type=uuid.UUID, required=True)
    parser.add_argument("--output", type=Path, required=True, help="output filename prefix")
    args = parser.parse_args()
    try:
        summary, rows = analyze(
            (row for path in args.logs for row in records(path)),
            (row for path in args.usage for row in records(path)), str(args.organization_id))
    except (ValueError, TypeError, KeyError, OSError):
        parser.error("invalid or conflicting input; check export formats and integer nano-USD costs")
    args.output.with_suffix(".json").write_text(json.dumps(summary, indent=2) + "\n", encoding="utf-8")
    with args.output.with_suffix(".csv").open("w", newline="", encoding="utf-8") as output:
        writer = csv.DictWriter(output, fieldnames=list(rows[0]) if rows else ["attempt_id", "status"])
        writer.writeheader()
        for row in rows:
            writer.writerow({key: "'" + value if isinstance(value, str) and value.startswith(("=", "+", "-", "@")) else value for key, value in row.items()})
    print(f"Samples: {summary['samples']}; matched: {summary['matched']}; statuses: {summary['statuses']}")


if __name__ == "__main__":
    main()
