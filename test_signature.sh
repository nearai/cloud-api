#!/bin/bash
# Test signature fetching on cloud-api
# Usage: ./test_signature.sh <base_url> <api_key>
#
# Reproduces the signature error: makes a completion, waits, then
# checks if the signature was stored. Before the fix, multi-instance
# models (Qwen3.5) fail ~83% of the time (5/6 chance of wrong backend).

BASE_URL="${1:-https://cloud-stg-api.near.ai}"
API_KEY="${2:-sk-75593ebd8f72433b8421e2090e5fb217}"
MODEL="${3:-Qwen/Qwen3.5-122B-A10B}"
ATTEMPTS="${4:-5}"

echo "=== Signature Fetch Test ==="
echo "URL: $BASE_URL"
echo "Model: $MODEL"
echo "Attempts: $ATTEMPTS"
echo ""

OK=0
FAIL=0

for i in $(seq 1 $ATTEMPTS); do
  # Make a completion
  RESP=$(curl -s --max-time 30 \
    "$BASE_URL/v1/chat/completions" \
    -H "Authorization: Bearer $API_KEY" \
    -H "Content-Type: application/json" \
    -d "{\"model\":\"$MODEL\",\"messages\":[{\"role\":\"user\",\"content\":\"Say $i\"}],\"max_tokens\":100}")
  
  CHAT_ID=$(echo "$RESP" | grep -o '"id":"[^"]*"' | head -1 | cut -d'"' -f4)
  
  if [ -z "$CHAT_ID" ]; then
    echo "[$i] SKIP - no chat_id in response"
    continue
  fi
  
  # Wait for signature to be stored
  sleep 3
  
  # Try to fetch signature
  SIG_RESP=$(curl -s -w "\n%{http_code}" --max-time 10 \
    "$BASE_URL/v1/signature/$CHAT_ID" \
    -H "Authorization: Bearer $API_KEY")
  
  SIG_HTTP=$(echo "$SIG_RESP" | tail -1)
  SIG_BODY=$(echo "$SIG_RESP" | head -1)
  
  if [ "$SIG_HTTP" = "200" ]; then
    ALGO=$(echo "$SIG_BODY" | grep -o '"signing_algo":"[^"]*"' | cut -d'"' -f4)
    echo "[$i] OK  chat_id=$CHAT_ID  algo=$ALGO"
    OK=$((OK + 1))
  else
    ERR=$(echo "$SIG_BODY" | grep -o '"error":"[^"]*"' | cut -d'"' -f4 || echo "$SIG_BODY")
    echo "[$i] FAIL chat_id=$CHAT_ID  HTTP=$SIG_HTTP  $ERR"
    FAIL=$((FAIL + 1))
  fi
done

echo ""
echo "=== Results: $OK/$ATTEMPTS OK, $FAIL/$ATTEMPTS FAILED ==="
if [ $FAIL -gt 0 ]; then
  echo "SIGNATURE FETCHING IS BROKEN"
  exit 1
else
  echo "All signatures fetched successfully"
  exit 0
fi
