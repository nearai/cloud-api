#!/usr/bin/env bash
# Seed the local stack: mock admin user, two-tier model (base shim 1k ctx,
# long shim 8k ctx), org with credits, API key. Prints the API key.
set -euo pipefail
cd "$(dirname "$0")"

API=${API:-http://127.0.0.1:13000}
MODEL_ID=${MODEL_ID:-z-ai/glm-5.2-local}
BASE_SHIM=${BASE_SHIM:-http://127.0.0.1:18100}
LONG_SHIM=${LONG_SHIM:-http://127.0.0.1:18101}
BASE_CTX=${BASE_CTX:-1000}
LONG_CTX=${LONG_CTX:-8000}

MOCK_USER_ID="11111111-1111-1111-1111-111111111111"
SESSION="rt_${MOCK_USER_ID}"
UA="Mock User Agent"

req() { # method path [json]
  local method=$1 path=$2 body=${3:-}
  curl -sf -X "$method" "$API$path" \
    -H "Authorization: Bearer $SESSION" \
    -H "User-Agent: $UA" \
    -H "Content-Type: application/json" \
    ${body:+-d "$body"}
}

echo "== waiting for cloud-api at $API"
for _ in $(seq 1 60); do curl -sf "$API/v1/health" >/dev/null 2>&1 && break; sleep 1; done
curl -sf "$API/v1/health" >/dev/null || { echo "cloud-api not reachable"; exit 1; }

echo "== seeding mock admin user (admin@test.com)"
docker exec local-stack-postgres psql -U postgres -d platform_api -q -c "
  INSERT INTO users (id, email, username, display_name, avatar_url, auth_provider, provider_user_id, created_at, updated_at)
  VALUES ('${MOCK_USER_ID}', 'admin@test.com', 'localdev', 'Local Dev', NULL, 'mock', 'mock_123', NOW(), NOW())
  ON CONFLICT (id) DO UPDATE SET email = EXCLUDED.email;"

echo "== upserting two-tier model ${MODEL_ID} (base ${BASE_CTX} @ ${BASE_SHIM}, long ${LONG_CTX} @ ${LONG_SHIM})"
req PATCH /v1/admin/models "$(cat <<EOF
{
  "${MODEL_ID}": {
    "inputCostPerToken":  {"amount": 1000, "currency": "USD"},
    "outputCostPerToken": {"amount": 2000, "currency": "USD"},
    "modelDisplayName": "GLM 5.2 (local two-tier)",
    "modelDescription": "Local dev stack model: base tier ${BASE_CTX} ctx, long tier ${LONG_CTX} ctx, served by llama.cpp",
    "contextLength": ${LONG_CTX},
    "maxOutputLength": 512,
    "verifiable": true,
    "isActive": true,
    "providerType": "vllm",
    "inferenceUrl": "${BASE_SHIM}",
    "providerConfig": {
      "long_context": {
        "inference_url": "${LONG_SHIM}",
        "max_context_tokens": ${LONG_CTX},
        "base_max_context_tokens": ${BASE_CTX}
      }
    }
  }
}
EOF
)" >/dev/null

echo "== creating org + credits + API key"
ORG_ID=$(req POST /v1/organizations '{"name":"local-stack-'"$RANDOM"'","description":"local dev"}' | python3 -c 'import sys,json;print(json.load(sys.stdin)["id"])')
req PATCH "/v1/admin/organizations/${ORG_ID}/limits" \
  '{"type":"payment","spendLimit":{"amount":10000000000,"currency":"USD"},"changedBy":"admin@test.com","changeReason":"local dev credits"}' >/dev/null
WS_ID=$(req GET "/v1/organizations/${ORG_ID}/workspaces" | python3 -c 'import sys,json;print(json.load(sys.stdin)["workspaces"][0]["id"])')
API_KEY=$(req POST "/v1/workspaces/${WS_ID}/api-keys" '{"name":"local-stack"}' | python3 -c 'import sys,json;print(json.load(sys.stdin)["key"])')

echo
echo "model:   ${MODEL_ID}"
echo "api key: ${API_KEY}"
echo "${API_KEY}" > .api-key
echo "(saved to dev/local-stack/.api-key)"
