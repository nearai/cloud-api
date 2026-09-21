"""Synthetic regression cases for rollout false positives; no network or API key needed."""

import contextlib
import copy
import io
import json
import os
import sys
import unittest
import urllib.request
from pathlib import Path
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import verify_anthropic_beta_relay as verify


def message():
    return {
        "type": "message",
        "role": "assistant",
        "id": "msg_synthetic",
        "content": [{"type": "text", "text": "synthetic output"}],
        "usage": {"input_tokens": 5, "output_tokens": 2},
        "stop_reason": "end_turn",
    }


def events():
    start = message()
    start.update(
        content=[], stop_reason=None, usage={"input_tokens": 5, "output_tokens": 0}
    )
    return [
        {"type": "message_start", "message": start},
        {"type": "ping"},
        {
            "type": "content_block_start",
            "index": 0,
            "content_block": {"type": "text", "text": ""},
        },
        {
            "type": "content_block_delta",
            "index": 0,
            "delta": {"type": "text_delta", "text": "synthetic output"},
        },
        {"type": "content_block_stop", "index": 0},
        {
            "type": "message_delta",
            "delta": {"stop_reason": "end_turn"},
            "usage": {"output_tokens": 2},
        },
        {"type": "message_stop"},
    ]


def sse(items):
    return "".join(
        f"event: {event['type']}\ndata: {json.dumps(event)}\n\n" for event in items
    ).encode()


def reply(probe):
    headers = {"content-type": "application/json", "request-id": "req_synthetic"}
    if probe.kind in ("router_error", "upstream_error"):
        if probe.kind == "router_error":
            del headers["request-id"]
        return verify.Reply(
            400,
            headers,
            json.dumps(
                {
                    "type": "error",
                    "error": {
                        "type": "invalid_request_error",
                        "message": probe.error_text,
                    },
                }
            ).encode(),
        )
    if probe.kind == "count":
        return verify.Reply(200, headers, b'{"input_tokens": 5}')
    headers["inference-id"] = "00000000-0000-4000-8000-000000000001"
    if probe.kind == "stream":
        headers["content-type"] = "text/event-stream; charset=utf-8"
        return verify.Reply(200, headers, sse(events()))
    return verify.Reply(200, headers, json.dumps(message()).encode())


class RelayVerificationTests(unittest.TestCase):
    def setUp(self):
        self.checks = verify.probes("anthropic/synthetic", "relay")
        self.stream = next(probe for probe in self.checks if probe.kind == "stream")

    def test_valid_relay_and_denylist_fixtures(self):
        for phase in ("relay", "denylist"):
            for probe in verify.probes("anthropic/synthetic", phase):
                with self.subTest(phase=phase, probe=probe.label):
                    verify.validate(reply(probe), probe)

    def test_stream_requires_terminal_event_final_usage_and_closed_blocks(self):
        cases = {
            "no terminal": events()[:-1],
            "no final usage": [e for e in events() if e["type"] != "message_delta"],
            "no block stop": [e for e in events() if e["type"] != "content_block_stop"],
            "no start": events()[1:],
            "duplicate start": [events()[0], *events()],
            "duplicate stop": [*events(), events()[-1]],
            "late error": [
                *events(),
                {"type": "error", "error": {"message": "private body"}},
            ],
            "mid-stream error": [*events()[:4], {"type": "error"}, *events()[4:]],
        }
        for label, items in cases.items():
            with self.subTest(label=label):
                response = reply(self.stream)
                response.body = sse(items)
                with self.assertRaises(verify.CheckFailed):
                    verify.validate(response, self.stream)

    def test_stream_handles_crlf_comments_and_multiline_data(self):
        response = reply(self.stream)
        response.body = response.body.replace(
            b'data: {"type":', b'data: {\ndata: "type":'
        )
        response.body = b": heartbeat\n\n" + response.body
        response.body = response.body.replace(b"\n", b"\r\n")
        verify.validate(response, self.stream)

    def test_stream_rejects_partial_frame_bad_json_and_mismatched_event(self):
        body = reply(self.stream).body
        for data in (
            body[:-1],
            body + b"data: {\n\n",
            body.replace(b"event: message_stop", b"event: error"),
        ):
            with self.subTest(data_length=len(data)):
                response = reply(self.stream)
                response.body = data
                with self.assertRaises(verify.CheckFailed):
                    verify.validate(response, self.stream)

    def test_usage_rejects_missing_negative_boolean_and_fractional_counts(self):
        for value in (None, -1, True, 1.5, "2", 0):
            with self.subTest(value=value):
                payload = message()
                payload["usage"]["output_tokens"] = value
                response = reply(self.checks[0])
                response.body = json.dumps(payload).encode()
                with self.assertRaises(verify.CheckFailed):
                    verify.validate(response, self.checks[0])
                items = events()
                items[-2]["usage"]["output_tokens"] = value
                response = reply(self.stream)
                response.body = sse(items)
                with self.assertRaises(verify.CheckFailed):
                    verify.validate(response, self.stream)

    def test_cached_input_can_have_zero_uncached_tokens(self):
        payload = message()
        payload["usage"].update(input_tokens=0, cache_read_input_tokens=5)
        response = reply(self.checks[0])
        response.body = json.dumps(payload).encode()
        verify.validate(response, self.checks[0])

    def test_errors_require_json_reason_and_correct_origin(self):
        for probe in self.checks:
            if not probe.kind.endswith("error"):
                continue
            good = reply(probe)
            for change in ("origin", "category", "reason", "envelope"):
                with self.subTest(probe=probe.label, change=change):
                    bad = copy.deepcopy(good)
                    if change == "origin":
                        if "request-id" in bad.headers:
                            del bad.headers["request-id"]
                        else:
                            bad.headers["request-id"] = "req_wrong_origin"
                    else:
                        payload = json.loads(bad.body)
                        if change == "category":
                            payload["error"]["type"] = "authentication_error"
                        elif change == "reason":
                            payload["error"]["message"] = "old allowlist rejection"
                        else:
                            payload["type"] = "message"
                        bad.body = json.dumps(payload).encode()
                    with self.assertRaises(verify.CheckFailed):
                        verify.validate(bad, probe)

    def test_http_200_alone_never_passes(self):
        for probe in self.checks[:3]:
            for body in (b"{}", b"null", b"[]", b"<html>OK</html>"):
                with self.subTest(probe=probe.label, body=body):
                    response = reply(probe)
                    response.body = body
                    with self.assertRaises(verify.CheckFailed):
                        verify.validate(response, probe)

    def test_missing_headers_fail_successful_response(self):
        for key in ("request-id", "inference-id", "content-type"):
            response = reply(self.checks[0])
            del response.headers[key]
            with self.subTest(key=key), self.assertRaises(verify.CheckFailed):
                verify.validate(response, self.checks[0])

    def test_cli_fails_if_an_earlier_probe_fails_and_hides_content(self):
        def respond(url, key, timeout, probe):
            response = reply(probe)
            if probe.kind == "stream":
                response.body = sse(events()[:-1])
            return response

        output = io.StringIO()
        with (
            patch.dict(os.environ, {"API_KEY": "synthetic-test-credential"}),
            patch.object(verify, "request", side_effect=respond) as request,
            contextlib.redirect_stdout(output),
        ):
            self.assertEqual(verify.main([]), 1)
        self.assertEqual(request.call_count, len(self.checks))
        self.assertIn("missing message_stop", output.getvalue())
        for secret in (
            "synthetic-test-credential",
            "synthetic output",
            "msg_synthetic",
            "req_synthetic",
        ):
            self.assertNotIn(secret, output.getvalue())

    def test_cli_success_and_transport_failure(self):
        with (
            patch.dict(os.environ, {"API_KEY": "synthetic-test-credential"}),
            contextlib.redirect_stdout(io.StringIO()) as output,
        ):
            with patch.object(
                verify, "request", side_effect=lambda u, k, t, p: reply(p)
            ):
                self.assertEqual(verify.main([]), 0)
            with patch.object(
                verify, "request", side_effect=OSError("sensitive detail")
            ):
                self.assertEqual(verify.main([]), 1)
            self.assertNotIn("sensitive detail", output.getvalue())

    def test_configuration_guards_run_before_network(self):
        for args in (
            ["--api-url", "https://cloud-api.near.ai"],
            [
                "--api-url",
                "https://cloud-api.near.ai",
                "--allow-production",
                "--phase",
                "denylist",
            ],
            ["--api-url", "https://user:password@example.com"],
            ["--timeout", "nan"],
        ):
            with (
                self.subTest(args=args),
                patch.object(verify, "request") as request,
                contextlib.redirect_stderr(io.StringIO()),
                self.assertRaises(SystemExit) as error,
            ):
                verify.main(args)
            self.assertEqual(error.exception.code, 2)
            request.assert_not_called()

    def test_redirects_are_not_followed_with_credentials(self):
        req = urllib.request.Request(
            "https://example.com", headers={"Authorization": "Bearer synthetic"}
        )
        self.assertIsNone(
            verify.NoRedirect().redirect_request(
                req, None, 302, "", {}, "https://other.example"
            )
        )


if __name__ == "__main__":
    unittest.main()
