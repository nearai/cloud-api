#!/usr/bin/env bash
# Reproduction script for "All providers failed for model" connection errors
#
# Most models have only 1 provider (via model-proxy). When the connection
# fails transiently (QEMU SLIRP backlog=1, brief nginx reload), the request
# fails immediately with no retry.
#
# Before the fix: single attempt, immediate failure
# After the fix: retries once after 500ms on connection/server errors
#
# Prerequisites: cloud-api running locally or access to prod
# Usage: API_KEY=sk-... API_URL=https://cloud-api.near.ai ./repro_connection_retry.sh

set -euo pipefail

API_KEY="${API_KEY:-sk-32c0476395fd40c795725fc101f33304}"
API_URL="${API_URL:-https://cloud-api.near.ai}"

echo "=== Test 1: Rapid concurrent requests to trigger connection failures ==="
echo "Sending 20 concurrent requests to stress the model-proxy connection..."
echo "(QEMU SLIRP has hardcoded listen backlog of 1, so concurrent connects can fail)"
echo

for i in $(seq 1 20); do
  curl -s --max-time 30 -X POST "${API_URL}/v1/chat/completions" \
    -H "Authorization: Bearer ${API_KEY}" \
    -H "Content-Type: application/json" \
    -d '{
      "model": "zai-org/GLM-5-FP8",
      "messages": [{"role": "user", "content": "hi"}],
      "max_tokens": 5,
      "stream": false
    }' 2>&1 | python3 -c "
import json, sys
try:
    d = json.load(sys.stdin)
    if 'error' in d:
        print(f'  Request $i: ERROR - {d[\"error\"][\"message\"][:80]}')
    else:
        print(f'  Request $i: OK')
except:
    print(f'  Request $i: PARSE ERROR')
" &
done
wait

echo
echo "=== Test 2: Check Datadog for retry behavior ==="
echo "After deploying the fix, check Datadog logs:"
echo "  service:cloud-api 'Retrying after transient connection failure'"
echo "  This confirms the retry is happening before giving up."
echo
echo "=== Expected improvement ==="
echo "  Before: transient connection failures return error immediately"
echo "  After: retries once after 500ms, recovering from brief SLIRP/nginx blips"
echo "  Most of the ~9,500 daily 'All providers failed' errors are transient and should resolve on retry"
