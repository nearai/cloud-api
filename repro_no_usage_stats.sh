#!/usr/bin/env bash
# Reproduction script for "Stream ended but no usage stats available" bug
#
# This demonstrates that when a client disconnects mid-stream before the
# final usage chunk arrives, cloud-api logs an ERROR instead of a WARN.
#
# External providers (OpenAI, Anthropic) only send usage in the final SSE chunk.
# If the client disconnects before that chunk, no usage is captured.
#
# Before the fix: these produce ERROR-level logs
# After the fix: client disconnects produce WARN, only truly completed streams with
# no usage produce ERROR
#
# Prerequisites: cloud-api running locally or access to prod
# Usage: API_KEY=sk-... API_URL=https://cloud-api.near.ai ./repro_no_usage_stats.sh

set -euo pipefail

API_KEY="${API_KEY:-sk-32c0476395fd40c795725fc101f33304}"
API_URL="${API_URL:-https://cloud-api.near.ai}"

echo "=== Test 1: Normal streaming request (should have usage) ==="
echo "Sending a normal streaming request that completes fully..."
curl -s --max-time 30 -X POST "${API_URL}/v1/chat/completions" \
  -H "Authorization: Bearer ${API_KEY}" \
  -H "Content-Type: application/json" \
  -d '{
    "model": "openai/gpt-5.2",
    "messages": [{"role": "user", "content": "Say hello"}],
    "max_tokens": 10,
    "stream": true
  }' 2>&1 | grep -E 'usage|DONE' | tail -3
echo
echo "^ Should show usage stats in final chunk"
echo

echo "=== Test 2: Client disconnect mid-stream (triggers the bug) ==="
echo "Sending streaming request but disconnecting after 1 second..."
echo "(This simulates a user closing the browser tab mid-response)"
timeout 1 curl -s -X POST "${API_URL}/v1/chat/completions" \
  -H "Authorization: Bearer ${API_KEY}" \
  -H "Content-Type: application/json" \
  -d '{
    "model": "openai/gpt-5.2",
    "messages": [{"role": "user", "content": "Write a very long essay about the history of mathematics, covering all major developments from ancient civilizations through modern times."}],
    "max_tokens": 500,
    "stream": true
  }' 2>&1 | tail -3 || true
echo
echo "^ Client disconnected. Check cloud-api logs:"
echo "  Before fix: ERROR 'Stream ended but no usage stats available'"
echo "  After fix:  WARN 'Stream interrupted before usage stats received (client disconnect or provider error)'"
echo

echo "=== Test 3: Verify stream_options include_usage behavior ==="
echo "Without include_usage, OpenAI does not send usage in streaming responses..."
echo "Test 3a: stream_options.include_usage = false"
curl -s --max-time 15 -X POST "${API_URL}/v1/chat/completions" \
  -H "Authorization: Bearer ${API_KEY}" \
  -H "Content-Type: application/json" \
  -d '{
    "model": "openai/gpt-5.2",
    "messages": [{"role": "user", "content": "Say hi"}],
    "max_tokens": 5,
    "stream": true,
    "stream_options": {"include_usage": false}
  }' 2>&1 | grep -c '"usage"' || echo "0 chunks with usage (cloud-api should override this)"
echo

echo "Test 3b: stream_options.include_usage = true"
curl -s --max-time 15 -X POST "${API_URL}/v1/chat/completions" \
  -H "Authorization: Bearer ${API_KEY}" \
  -H "Content-Type: application/json" \
  -d '{
    "model": "openai/gpt-5.2",
    "messages": [{"role": "user", "content": "Say hi"}],
    "max_tokens": 5,
    "stream": true,
    "stream_options": {"include_usage": true}
  }' 2>&1 | grep -c '"usage"' || echo "0 chunks with usage"
echo "^ Should show >0 chunks with usage"

echo
echo "Done. Check Datadog logs for cloud-api ERROR vs WARN level changes."
