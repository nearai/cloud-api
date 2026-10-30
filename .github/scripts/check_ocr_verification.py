#!/usr/bin/env python3
"""Allow publication only for complete, privately verified OCR reviews."""

import json
import sys


def check_review(result, exit_code):
    if exit_code != 0:
        raise ValueError(f"OpenCodeReview exited with status {exit_code}")
    if not isinstance(result, dict):
        raise ValueError("Missing OCR result object")

    identity = result.get("llm") or {}
    report = identity.get("verification") or {}
    if identity.get("provider") != "nearai":
        raise ValueError("Review did not use the NEAR AI provider")
    if report.get("e2ee") is not True or report.get("verification_level") != "model-tee":
        raise ValueError("Review did not enforce E2EE and model-TEE verification")

    counts = {}
    for name in ("verified", "failed", "request_failed", "cancelled", "pending"):
        value = report.get(name)
        if type(value) is not int or value < 0:
            raise ValueError(f"Missing or invalid verification count: {name}")
        counts[name] = value
    if any(counts[name] for name in ("failed", "request_failed", "cancelled", "pending")):
        raise ValueError("Review contains failed, cancelled, or unfinished requests")

    if result.get("status") == "skipped":
        if counts["verified"] != 0 or report.get("status") != "not_requested" or result.get("comments"):
            raise ValueError("Inconsistent skipped review")
        return "Review skipped: no items selected; no inference was requested."

    if result.get("status") != "complete":
        raise ValueError("Review did not complete; partial results cannot be published")
    if report.get("status") != "verified" or counts["verified"] < 1:
        raise ValueError("Review has no successfully verified completions")
    return f"NEAR AI: {counts['verified']} verified completions; E2EE=true, policy=model-tee."


if __name__ == "__main__":
    try:
        with open(sys.argv[1], encoding="utf-8") as source:
            result = json.load(source)
        print(check_review(result, int(sys.argv[2])))
    except (OSError, ValueError, TypeError, AttributeError) as error:
        sys.exit(f"OCR publication blocked: {error}")
