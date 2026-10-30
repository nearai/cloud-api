"""Regression tests for failing closed before publishing an OCR review."""

import copy
import unittest

from check_ocr_verification import check_review


class CheckReviewTests(unittest.TestCase):
    def setUp(self):
        self.result = {
            "status": "complete",
            "comments": [],
            "llm": {
                "provider": "nearai",
                "verification": {
                    "status": "verified",
                    "verified": 2,
                    "failed": 0,
                    "request_failed": 0,
                    "cancelled": 0,
                    "pending": 0,
                    "e2ee": True,
                    "verification_level": "model-tee",
                },
            },
        }

    def test_complete_review(self):
        self.assertIn("2 verified completions", check_review(self.result, 0))

    def test_nonzero_exit_even_with_successful_json(self):
        with self.assertRaises(ValueError):
            check_review(self.result, 1)

    def test_failed_or_partial_zero_findings_cannot_look_clean(self):
        for status in ("failed", "partial", "success", None):
            with self.subTest(status=status):
                self.result["status"] = status
                with self.assertRaises(ValueError):
                    check_review(self.result, 0)

    def test_rejects_weakened_or_missing_policy(self):
        for field, value in (("e2ee", False), ("e2ee", "true"),
                             ("verification_level", "gateway"), ("status", "pending"),
                             ("verified", 0), ("verified", True)):
            with self.subTest(field=field, value=value):
                result = copy.deepcopy(self.result)
                result["llm"]["verification"][field] = value
                with self.assertRaises(ValueError):
                    check_review(result, 0)
        for identity in ({}, {"provider": "openai"}, {"provider": "nearai"}):
            self.result["llm"] = identity
            with self.assertRaises(ValueError):
                check_review(self.result, 0)

    def test_rejects_every_unsuccessful_count(self):
        for field in ("failed", "request_failed", "cancelled", "pending"):
            for value in (1, -1, None):
                with self.subTest(field=field, value=value):
                    result = copy.deepcopy(self.result)
                    result["llm"]["verification"][field] = value
                    with self.assertRaises(ValueError):
                        check_review(result, 0)

    def test_skip_is_explicit_and_cannot_carry_findings(self):
        self.result["status"] = "skipped"
        report = self.result["llm"]["verification"]
        report.update(status="not_requested", verified=0)
        self.assertIn("no inference", check_review(self.result, 0))

        self.result["comments"] = [{"content": "unexpected finding"}]
        with self.assertRaises(ValueError):
            check_review(self.result, 0)


if __name__ == "__main__":
    unittest.main()
