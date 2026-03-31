#!/usr/bin/env bash
# Reproduction script for "All providers failed for model with public key" bug
#
# When an inference-proxy backend restarts, it generates new signing keys.
# Clients with cached attestation reports still send the old public key,
# which no longer matches any active provider.
#
# Before the fix: returns HTTP 502 "The model is currently unavailable"
#   (indistinguishable from a real backend outage)
# After the fix: returns HTTP 421 "The encryption key is no longer valid.
#   Please refresh your attestation report and retry."
#
# Prerequisites: cloud-api running locally or access to prod
# Usage: API_KEY=sk-... API_URL=https://cloud-api.near.ai ./repro_e2ee_stale_pubkey.sh

set -euo pipefail

API_KEY="${API_KEY:-sk-32c0476395fd40c795725fc101f33304}"
API_URL="${API_URL:-https://cloud-api.near.ai}"

echo "=== Step 1: Get a valid attestation report (current public keys) ==="
ATTESTATION=$(curl -s --max-time 15 "${API_URL}/v1/attestation/report?model=zai-org/GLM-5-FP8" \
  -H "Authorization: Bearer ${API_KEY}")

VALID_KEY=$(echo "$ATTESTATION" | python3 -c "
import json, sys
data = json.load(sys.stdin)
reports = data.get('model_attestations', [])
if reports:
    print(reports[0].get('signing_address', ''))
else:
    print('')
" 2>/dev/null)
echo "Valid public key prefix: ${VALID_KEY:0:32}..."
echo

echo "=== Step 2: Use a FAKE (stale) public key to trigger the error ==="
FAKE_KEY="0000000000000000000000000000000000000000000000000000000000000000"
echo "Using fake key: ${FAKE_KEY:0:32}..."

RESPONSE=$(curl -s --max-time 15 -X POST "${API_URL}/v1/chat/completions" \
  -H "Authorization: Bearer ${API_KEY}" \
  -H "Content-Type: application/json" \
  -H "X-Model-Pub-Key: ${FAKE_KEY}" \
  -d '{
    "model": "zai-org/GLM-5-FP8",
    "messages": [{"role": "user", "content": "hi"}],
    "max_tokens": 5
  }')

echo "Response:"
echo "$RESPONSE" | python3 -m json.tool 2>/dev/null || echo "$RESPONSE"
echo
echo "=== Expected result ==="
echo "  Before fix: {\"error\": {\"message\": \"The model is currently unavailable...\", \"type\": \"bad_gateway\"}}"
echo "  After fix:  {\"error\": {\"message\": \"The encryption key is no longer valid...\", \"type\": \"provider_error\"}}"
echo
echo "The 421 status code and specific message allow E2EE clients to"
echo "detect stale attestation and auto-refresh without user intervention."
