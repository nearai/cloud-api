#!/usr/bin/env python3

"""
Simple API test script for NEAR AI Cloud API.
Exercises the main data-plane endpoints using an API key.

pip install requests
"""

import requests
import os
import json
import sys

# --- Configuration ---
API_BASE_URL = os.environ.get("API_BASE_URL", "https://cloud-stg-api.near.ai/v1")
API_KEY = os.environ.get("API_KEY", "")
MODEL = os.environ.get("MODEL", "zai-org/GLM-4.7")
FILE_TO_UPLOAD = "test_payload.json"

passed = 0
failed = 0


def report(name, response, expect_status=None):
    """Print result and track pass/fail."""
    global passed, failed
    status = response.status_code if response else 0
    ok = True
    if expect_status and status != expect_status:
        ok = False
    if status >= 500:
        ok = False

    tag = "PASS" if ok else "FAIL"
    if ok:
        passed += 1
    else:
        failed += 1

    print(f"  [{tag}] {name} — {status}")
    if not ok and response is not None and response.text:
        print(f"         {response.text[:200]}")
    return response


def headers(content_type=None):
    h = {"Authorization": f"Bearer {API_KEY}"}
    if content_type:
        h["Content-Type"] = content_type
    return h


def get(path, params=None, **kwargs):
    return requests.get(f"{API_BASE_URL}{path}", headers=headers(), params=params, **kwargs)


def post(path, json_payload=None, **kwargs):
    return requests.post(f"{API_BASE_URL}{path}", headers=headers("application/json"), json=json_payload, **kwargs)


def delete(path):
    return requests.delete(f"{API_BASE_URL}{path}", headers=headers())


def post_file(path, file_path, data=None):
    h = {"Authorization": f"Bearer {API_KEY}"}
    with open(file_path, "rb") as f:
        files = {"file": (os.path.basename(file_path), f)}
        return requests.post(f"{API_BASE_URL}{path}", headers=h, data=data, files=files)


def body(response):
    """Safely parse JSON body."""
    try:
        return response.json()
    except Exception:
        return {}


# --- Test Sections ---

def test_health():
    print("\n=== Health ===")
    report("GET /health", get("/health"), 200)


def test_check_api_key():
    print("\n=== Check API Key ===")
    report("POST /check_api_key", post("/check_api_key", {"api_key": API_KEY}), 200)


def test_models():
    print("\n=== Models ===")
    r = report("GET /models", get("/models"), 200)
    data = body(r)
    if data.get("data"):
        print(f"         Found {len(data['data'])} model(s)")

    report("GET /model/list", get("/model/list"), 200)


def test_chat_completions():
    print("\n=== Chat Completions ===")

    # Non-streaming
    payload = {
        "model": MODEL,
        "messages": [{"role": "user", "content": "Say hello in one word."}],
        "max_tokens": 10,
        "stream": False,
    }
    r = report("POST /chat/completions", post("/chat/completions", payload), 200)
    data = body(r)
    if data.get("choices"):
        content = (data["choices"][0].get("message") or {}).get("content") or ""
        print(f"         Response: {content[:80]}")

    # Streaming
    payload["stream"] = True
    r = requests.post(
        f"{API_BASE_URL}/chat/completions",
        headers=headers("application/json"),
        json=payload,
        stream=True,
    )
    chunks = 0
    for line in r.iter_lines():
        if line:
            chunks += 1
    report("POST /chat/completions (stream)", r, 200)
    print(f"         Received {chunks} SSE chunk(s)")


def test_responses_basic():
    print("\n=== Responses: Basic ===")

    # Non-streaming
    payload = {
        "model": MODEL,
        "input": "Say hello in one word.",
        "max_output_tokens": 20,
        "stream": False,
    }
    r = report("POST /responses (non-stream)", post("/responses", payload), 200)
    data = body(r)
    response_id = data.get("id")
    assert_field(data, "status", "completed")
    assert_field(data, "object", "response")
    if data.get("output"):
        print(f"         Output items: {len(data['output'])}")

    if response_id:
        report("GET /responses/{id}", get(f"/responses/{response_id}"))
        report("GET /responses/{id}/input_items", get(f"/responses/{response_id}/input_items"), 200)
        report("DELETE /responses/{id}", delete(f"/responses/{response_id}"))


def test_responses_streaming():
    print("\n=== Responses: Streaming ===")

    payload = {
        "model": MODEL,
        "input": "Say hello in one word.",
        "max_output_tokens": 20,
        "stream": True,
    }
    r = requests.post(
        f"{API_BASE_URL}/responses",
        headers=headers("application/json"),
        json=payload,
        stream=True,
    )
    report("POST /responses (stream)", r, 200)

    events = {}
    for line in r.iter_lines(decode_unicode=True):
        if line and line.startswith("event: "):
            event_type = line[len("event: "):]
            events[event_type] = events.get(event_type, 0) + 1

    print(f"         SSE events: {dict(events)}")
    if "response.completed" in events or "response.created" in events:
        print("         Stream lifecycle OK")


def test_responses_with_instructions():
    print("\n=== Responses: Instructions (system prompt) ===")

    payload = {
        "model": MODEL,
        "instructions": "You are a pirate. Always respond with 'Arrr'.",
        "input": "Hello",
        "max_output_tokens": 30,
        "stream": False,
    }
    r = report("POST /responses (instructions)", post("/responses", payload), 200)
    data = body(r)
    text = extract_output_text(data)
    if text:
        print(f"         Output: {text[:80]}")


def test_responses_with_metadata():
    print("\n=== Responses: Metadata ===")

    payload = {
        "model": MODEL,
        "input": "Hi",
        "max_output_tokens": 10,
        "stream": False,
        "metadata": {"test_key": "test_value", "session": "smoke-test"},
    }
    r = report("POST /responses (metadata)", post("/responses", payload), 200)
    data = body(r)
    meta = data.get("metadata") or {}
    if meta.get("test_key") == "test_value":
        print("         Metadata round-tripped OK")
    else:
        print(f"         Metadata returned: {meta}")


def test_responses_with_conversation():
    print("\n=== Responses: Conversation-linked ===")

    # Create a conversation first
    r = post("/conversations", {})
    conv_data = body(r)
    conv_id = conv_data.get("id")
    if not conv_id:
        print("         Skipping (could not create conversation)")
        return

    # Create response linked to the conversation
    payload = {
        "model": MODEL,
        "conversation": {"id": conv_id},
        "input": "What is 2+2?",
        "max_output_tokens": 30,
        "stream": False,
    }
    r = report("POST /responses (conversation)", post("/responses", payload), 200)
    data = body(r)
    resp_id = data.get("id")

    # Verify conversation items were created
    r2 = report("GET /conversations/{id}/items", get(f"/conversations/{conv_id}/items"), 200)
    items_data = body(r2)
    item_count = len(items_data.get("data", []))
    print(f"         Conversation now has {item_count} item(s)")

    # Cleanup
    delete(f"/conversations/{conv_id}")


def test_responses_multi_turn():
    print("\n=== Responses: Multi-turn (previous_response_id) ===")

    # Turn 1
    payload = {
        "model": MODEL,
        "input": "Remember the number 42.",
        "max_output_tokens": 50,
        "stream": False,
    }
    r = report("POST /responses (turn 1)", post("/responses", payload), 200)
    data = body(r)
    resp1_id = data.get("id")

    if not resp1_id:
        print("         Skipping (no response_id from turn 1)")
        return

    # Turn 2 — references turn 1
    payload2 = {
        "model": MODEL,
        "previous_response_id": resp1_id,
        "input": "What number did I ask you to remember?",
        "max_output_tokens": 50,
        "stream": False,
    }
    r2 = report("POST /responses (turn 2)", post("/responses", payload2), 200)
    data2 = body(r2)
    assert_field(data2, "previous_response_id", resp1_id)
    text = extract_output_text(data2)
    if text:
        print(f"         Turn 2 output: {text[:80]}")


def test_responses_function_calling():
    print("\n=== Responses: Function Calling (tool use) ===")

    # Define a function tool and ask the model to use it
    tools = [
        {
            "type": "function",
            "name": "get_weather",
            "description": "Get the current weather for a location.",
            "parameters": {
                "type": "object",
                "properties": {
                    "location": {"type": "string", "description": "City name"},
                },
                "required": ["location"],
            },
        }
    ]

    payload = {
        "model": MODEL,
        "input": "What is the weather in Tokyo?",
        "tools": tools,
        "tool_choice": "required",
        "max_output_tokens": 200,
        "stream": False,
    }
    r = report("POST /responses (function call)", post("/responses", payload), 200)
    data = body(r)
    resp_id = data.get("id")

    # Find the function_call in output
    call_id = None
    fn_name = None
    fn_args = None
    for item in data.get("output", []):
        if item.get("type") == "function_call":
            call_id = item.get("call_id")
            fn_name = item.get("name")
            fn_args = item.get("arguments")
            break

    if call_id:
        print(f"         Function called: {fn_name}({fn_args})")

        # Resume with function output
        resume_payload = {
            "model": MODEL,
            "previous_response_id": resp_id,
            "input": [
                {
                    "type": "function_call_output",
                    "call_id": call_id,
                    "output": json.dumps({"temperature": "22°C", "condition": "Sunny"}),
                }
            ],
            "tools": tools,
            "max_output_tokens": 200,
            "stream": False,
        }
        r2 = report("POST /responses (function resume)", post("/responses", resume_payload), 200)
        data2 = body(r2)
        text = extract_output_text(data2)
        if text:
            print(f"         After resume: {text[:100]}")
    else:
        print(f"         Model did not call function. Status: {data.get('status')}")
        for item in data.get("output", []):
            print(f"         Output item type: {item.get('type')}")


def test_responses_web_search():
    print("\n=== Responses: Web Search Tool ===")

    payload = {
        "model": MODEL,
        "tools": [{"type": "web_search"}],
        "input": "What is the current population of France?",
        "max_output_tokens": 200,
        "stream": False,
    }
    r = report("POST /responses (web_search)", post("/responses", payload), 200)
    data = body(r)

    # Check for web_search_call in output
    search_calls = [i for i in data.get("output", []) if i.get("type") == "web_search_call"]
    messages = [i for i in data.get("output", []) if i.get("type") == "message"]
    print(f"         Web search calls: {len(search_calls)}, Messages: {len(messages)}")


def test_responses_tool_choice():
    print("\n=== Responses: Tool Choice Modes ===")

    tools = [
        {
            "type": "function",
            "name": "lookup",
            "description": "Look up a fact.",
            "parameters": {
                "type": "object",
                "properties": {"query": {"type": "string"}},
                "required": ["query"],
            },
        }
    ]

    # tool_choice = "none" — model should NOT call any tool
    payload = {
        "model": MODEL,
        "input": "What is 1+1?",
        "tools": tools,
        "tool_choice": "none",
        "max_output_tokens": 30,
        "stream": False,
    }
    r = report("POST /responses (tool_choice=none)", post("/responses", payload), 200)
    data = body(r)
    fn_calls = [i for i in data.get("output", []) if i.get("type") == "function_call"]
    print(f"         Function calls with none: {len(fn_calls)} (expect 0)")


# --- Helpers ---

def extract_output_text(data):
    """Pull text from the first message output item."""
    for item in data.get("output", []):
        if item.get("type") == "message":
            for c in item.get("content", []):
                if c.get("type") == "output_text":
                    return c.get("text", "")
    return ""


def assert_field(data, field, expected):
    """Soft-assert a field value and print if mismatch."""
    actual = data.get(field)
    if actual != expected:
        print(f"         NOTE: {field}={actual!r}, expected {expected!r}")


def test_conversations():
    print("\n=== Conversations ===")

    # Create conversation
    r = report("POST /conversations", post("/conversations", {}), 201)
    data = body(r)
    conv_id = data.get("id")

    if not conv_id:
        print("         Skipping conversation tests (no conversation_id)")
        return

    # Get conversation
    report("GET /conversations/{id}", get(f"/conversations/{conv_id}"), 200)

    # Update conversation
    report(
        "POST /conversations/{id} (update)",
        post(f"/conversations/{conv_id}", {"metadata": {"test": "true"}}),
        200,
    )

    # List items
    report("GET /conversations/{id}/items", get(f"/conversations/{conv_id}/items"), 200)

    # Create items
    items_payload = {
        "items": [
            {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "test"}]}
        ]
    }
    report("POST /conversations/{id}/items", post(f"/conversations/{conv_id}/items", items_payload), 200)

    # Pin / Unpin
    report("POST /conversations/{id}/pin", post(f"/conversations/{conv_id}/pin"), 200)
    report("DELETE /conversations/{id}/pin", delete(f"/conversations/{conv_id}/pin"), 200)

    # Archive / Unarchive
    report("POST /conversations/{id}/archive", post(f"/conversations/{conv_id}/archive"), 200)
    report("DELETE /conversations/{id}/archive", delete(f"/conversations/{conv_id}/archive"), 200)

    # Clone
    report("POST /conversations/{id}/clone", post(f"/conversations/{conv_id}/clone"), 201)

    # Batch retrieve
    report("POST /conversations/batch", post("/conversations/batch", {"ids": [conv_id]}), 200)

    # Delete conversation
    report("DELETE /conversations/{id}", delete(f"/conversations/{conv_id}"), 200)


def test_files():
    print("\n=== Files ===")

    # Create dummy file
    dummy = {"test": True, "data": [1, 2, 3]}
    with open(FILE_TO_UPLOAD, "w") as f:
        json.dump(dummy, f)

    # Upload
    upload_data = {"purpose": "user_data"}
    r = report("POST /files", post_file("/files", FILE_TO_UPLOAD, upload_data), 201)
    data = body(r)
    file_id = data.get("id")

    # List files
    report("GET /files", get("/files"), 200)

    if file_id:
        # Get metadata
        report("GET /files/{id}", get(f"/files/{file_id}"), 200)

        # Get content
        report("GET /files/{id}/content", get(f"/files/{file_id}/content"), 200)

        # Delete
        report("DELETE /files/{id}", delete(f"/files/{file_id}"), 200)
    else:
        print("         Skipping file detail tests (no file_id)")

    # Cleanup
    if os.path.exists(FILE_TO_UPLOAD):
        os.remove(FILE_TO_UPLOAD)


def test_billing():
    print("\n=== Billing ===")
    # POST with requestIds (UUIDs); empty array just validates the endpoint is reachable
    report("POST /billing/costs", post("/billing/costs", {"requestIds": []}), 200)


def main():
    print(f"Target: {API_BASE_URL}")
    print(f"API Key: {API_KEY[:10]}...")

    test_health()
    test_check_api_key()
    test_models()
    test_chat_completions()
    test_responses_basic()
    test_responses_streaming()
    test_responses_with_instructions()
    test_responses_with_metadata()
    test_responses_with_conversation()
    test_responses_multi_turn()
    test_responses_function_calling()
    test_responses_web_search()
    test_responses_tool_choice()
    test_conversations()
    test_files()
    test_billing()

    print(f"\n{'='*40}")
    print(f"Results: {passed} passed, {failed} failed, {passed + failed} total")
    if failed:
        sys.exit(1)


if __name__ == "__main__":
    main()
