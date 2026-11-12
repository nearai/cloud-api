import importlib.util
import json
from pathlib import Path
import tempfile
import unittest
import uuid

SPEC = importlib.util.spec_from_file_location("analyze_admission", Path(__file__).parents[1] / "analyze_admission.py")
MODULE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MODULE)
ORG = str(uuid.UUID(int=1))


def estimate(attempt="a", amount=10, **extra):
    return {"event": "usage_admission_estimate", "attempt_id": attempt, "organization_id": ORG,
            "estimated_nano_usd": amount, "path": "cloud_api", "mode": "audit",
            "model_id": "model", "inference_type": "chat_completion", "input_kind": "text",
            "estimator_version": 2, "sample_rate": .01, **extra}


def link(attempt="a", **extra):
    return {"event": "usage_admission_usage", "attempt_id": attempt, "organization_id": ORG, **extra}


class AdmissionAnalysisTests(unittest.TestCase):
    def test_gateway_and_direct_join_committed_usage_without_double_counting(self):
        provider = "response_" + "long" * 100
        inference = str(uuid.uuid5(MODULE.EXTERNAL_NAMESPACE, provider))
        logs = [estimate(), link(usage_id="bill-a"), estimate("b", 20, path="direct_proxy"),
                link("b", provider_request_id=provider), estimate("c", 30, path="direct_proxy", mode="enforce"),
                link("c", admission_id="reservation")]
        bills = [
            {"id": "bill-a", "total_cost": 15, "model": "m"},
            {"id": "bill-b", "source": "inference", "total_cost_nano_usd": 5,
             "inference": {"inference_id": inference, "model": "m"}},
            {"id": "bill-c", "total_cost": 50, "inference_id": "reservation", "model": "m"},
        ]
        summary, rows = MODULE.analyze(logs + logs, bills + bills, ORG)
        self.assertEqual(summary["matched"], 3)
        self.assertEqual(summary["underestimated_nano_usd"], 25)
        self.assertEqual(summary["overestimated_nano_usd"], 15)
        self.assertEqual(summary["underestimated_rate"], 2 / 3)
        self.assertEqual([r["actual_nano_usd"] for r in rows], [15, 5, 50])

    def test_pending_unknown_and_unexecuted_are_not_zero_cost(self):
        logs = [estimate("a"), estimate("b"), estimate("c"),
                {"event": "usage_admission_attempt_end", "attempt_id": "b", "outcome": "unknown"},
                {"event": "usage_admission_attempt_end", "attempt_id": "c", "outcome": "not_executed"}]
        summary, rows = MODULE.analyze(logs, [], ORG)
        self.assertEqual(summary["statuses"], {"unmatched": 1, "unknown": 1, "not_executed": 1})
        self.assertIsNone(summary["underestimated_rate"])
        self.assertTrue(all(r["actual_nano_usd"] is None for r in rows))

    def test_reused_bill_is_ambiguous_and_foreign_organization_is_ignored(self):
        logs = [estimate("a"), estimate("b"), link("a", usage_id="bill"), link("b", usage_id="bill"),
                estimate("foreign", organization_id="other")]
        summary, _ = MODULE.analyze(logs, [{"id": "bill", "total_cost": 10}], ORG)
        self.assertEqual(summary["statuses"], {"ambiguous": 2})
        self.assertEqual(summary["matched"], 0)

    def test_conflicting_receipts_and_non_integer_costs_fail(self):
        for bills in [
            [{"id": "bill", "total_cost": 10}, {"id": "bill", "total_cost": 11}],
            [{"id": "bill", "total_cost": .01}],
            [{"id": "bill", "total_cost": -1}],
            [{"id": "bill", "total_cost": True}],
        ]:
            with self.subTest(bills=bills), self.assertRaises(ValueError):
                MODULE.analyze([estimate()], bills, ORG)

    def test_zero_estimate_and_completed_usage_win_over_unknown_outcome(self):
        logs = [estimate(amount=0), link(usage_id="bill"),
                {"event": "usage_admission_attempt_end", "attempt_id": "a", "outcome": "unknown"}]
        summary, _ = MODULE.analyze(logs, [{"id": "bill", "total_cost": 1}], ORG)
        self.assertEqual(summary["zero_estimate_positive_actual"], 1)
        self.assertEqual(summary["underestimated_rate"], 1)
        self.assertIsNone(summary["actual_to_estimate_p95"])

    def test_grafana_json_jsonl_usage_pages_and_csv_formats(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "export.json"
            path.write_text(json.dumps([{"timestamp": "0", "fields": {}, "line": json.dumps({"fields": estimate()})}]))
            self.assertEqual(list(MODULE.records(path)), [estimate()])
            path.write_text(json.dumps({"fields": estimate()}) + "\n" + json.dumps({"fields": link(usage_id="b")}))
            self.assertEqual(len(list(MODULE.records(path))), 2)
            path.write_text(json.dumps({"data": [{"id": "b", "total_cost": 9}], "offset": 0}))
            self.assertEqual(list(MODULE.records(path)), [{"id": "b", "total_cost": 9}])
            csv = path.with_suffix(".csv")
            csv.write_text("id,total_cost\nb,9\n")
            self.assertEqual(list(MODULE.records(csv)), [{"id": "b", "total_cost": "9"}])


if __name__ == "__main__":
    unittest.main()
