#!/usr/bin/env bash
# Tier-routing demo against the local stack: shows which tier served each
# request (the shims tag the response `.model` with +base/+long) and prints a
# pass/fail table. Requires seed.sh to have run.
set -uo pipefail
cd "$(dirname "$0")"

API=${API:-http://127.0.0.1:13000}
MODEL_ID=${MODEL_ID:-z-ai/glm-5.2-local}
API_KEY=${API_KEY:-$(cat .api-key 2>/dev/null || true)}
[ -n "$API_KEY" ] || { echo "no API key — run seed.sh first"; exit 1; }

PASS=0; FAIL=0; ROWS=""

chat() { # prompt max_tokens stream
  curl -s -w '\n%{http_code}' "$API/v1/chat/completions" \
    -H "Authorization: Bearer $API_KEY" -H "Content-Type: application/json" \
    -d "{\"model\":\"$MODEL_ID\",\"messages\":[{\"role\":\"user\",\"content\":\"$1\"}],\"max_tokens\":$2,\"stream\":$3}"
}

check() { # name expect_pattern got
  local name=$1 expect=$2 got=$3
  if echo "$got" | grep -Eq "$expect"; then
    PASS=$((PASS+1)); ROWS+="  PASS  $name\n"
  else
    FAIL=$((FAIL+1)); ROWS+="  FAIL  $name  (wanted /$expect/, got: $(echo "$got" | head -c 200))\n"
  fi
}

# Word-repeat prompts give predictable real token counts (~1 token per word).
words() { python3 -c "print('hello '*$1)"; }

echo "== 1. small request -> base tier"
OUT=$(chat "Say hi in one word." 16 false)
check "small -> +base tag"        '"model": ?"[^"]*\+base"'   "$OUT"
check "small -> HTTP 200"         '^200$'                  "$(echo "$OUT" | tail -1)"

echo "== 2. oversize request (~3000 real tokens > base 1000) -> long tier"
OUT=$(chat "$(words 3000)" 16 false)
check "oversize -> +long tag"     '"model": ?"[^"]*\+long"'   "$OUT"
check "oversize -> HTTP 200"      '^200$'                  "$(echo "$OUT" | tail -1)"

echo "== 3. boundary request (~800 real tokens, heuristic ambiguous) -> exact /v1/tokenize decides -> base"
OUT=$(chat "$(words 800)" 16 false)
check "boundary -> +base tag"     '"model": ?"[^"]*\+base"'   "$OUT"

echo "== 4. streaming oversize -> long tier serves (SSE)"
OUT=$(chat "$(words 3000)" 16 true)
check "stream -> HTTP 200"        '^200$'                  "$(echo "$OUT" | tail -1)"
check "stream -> content"         'data:'                  "$OUT"

echo "== 5. saturate the long tier -> oversize gets a RETRYABLE error (not a context-400)"
touch SATURATE-long
OUT=$(chat "$(words 3000)" 16 false)
CODE=$(echo "$OUT" | tail -1)
rm -f SATURATE-long
if [ "$CODE" = "429" ] || [ "${CODE:0:1}" = "5" ]; then
  PASS=$((PASS+1)); ROWS+="  PASS  saturated long -> retryable $CODE\n"
else
  FAIL=$((FAIL+1)); ROWS+="  FAIL  saturated long -> got $CODE (must be 429/5xx, never 400)\n"
fi

echo
echo "================ local-stack tier-routing demo ================"
printf "%b" "$ROWS"
echo "==============================================================="
echo "PASS=$PASS FAIL=$FAIL"
[ "$FAIL" = 0 ]
