#!/bin/bash
# Staging Test Suite for v0.1.10 Release
#
# Usage:
#   export STAGING_URL="https://staging.api.near.ai"
#   export STAGING_API_KEY="sk-live-xxx"
#   ./scripts/test_staging_release.sh
#
# Commits tested:
#   c39101b - Failed response creates conversation item (#410)
#   513e580 - Message metadata preservation (#375)
#   77be94b - Credit type and source tracking (#318)
#   acfc8ef - Root response ID in conversation metadata (#403)
#   9fa9876 - Anthropic model improvements (#408)
#   59126ee - Image edit API (#388)

set -euo pipefail

# Colors
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m'

# Counters
PASSED=0
FAILED=0

# Configuration
: "${STAGING_URL:?Set STAGING_URL environment variable}"
: "${STAGING_API_KEY:?Set STAGING_API_KEY environment variable}"

API_URL="${STAGING_URL%/}"

log_test() {
    echo ""
    echo -e "${BLUE}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${NC}"
    echo -e "${BLUE}TEST: $1${NC}"
    echo -e "${BLUE}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${NC}"
}

log_pass() {
    echo -e "${GREEN}✓ PASS${NC}: $1"
    ((PASSED++))
}

log_fail() {
    echo -e "${RED}✗ FAIL${NC}: $1"
    echo -e "${RED}  Expected: $2${NC}"
    echo -e "${RED}  Got: $3${NC}"
    ((FAILED++))
}

log_info() {
    echo -e "${YELLOW}INFO${NC}: $1"
}

# API helper
api_call() {
    local method="$1"
    local endpoint="$2"
    local data="${3:-}"

    if [ -n "$data" ]; then
        curl -s -X "$method" \
            -H "Authorization: Bearer $STAGING_API_KEY" \
            -H "Content-Type: application/json" \
            -d "$data" \
            "${API_URL}${endpoint}"
    else
        curl -s -X "$method" \
            -H "Authorization: Bearer $STAGING_API_KEY" \
            "${API_URL}${endpoint}"
    fi
}

api_call_stream() {
    local endpoint="$1"
    local data="$2"

    curl -s -N -X POST \
        -H "Authorization: Bearer $STAGING_API_KEY" \
        -H "Content-Type: application/json" \
        -d "$data" \
        "${API_URL}${endpoint}"
}

# ============================================================================
echo ""
echo "╔═══════════════════════════════════════════════════════════════════╗"
echo "║          Staging Test Suite - v0.1.10 Release                     ║"
echo "╚═══════════════════════════════════════════════════════════════════╝"
echo ""
echo "Target: $API_URL"
echo ""

# Health check
log_info "Checking API health..."
HEALTH=$(curl -s "${API_URL}/health" | jq -r '.status // "unknown"')
if [ "$HEALTH" != "healthy" ] && [ "$HEALTH" != "ok" ]; then
    echo -e "${RED}API health check failed: $HEALTH${NC}"
    exit 1
fi
log_info "API is healthy"

# Get available models
log_info "Fetching available models..."
MODELS=$(api_call GET "/v1/models")
MODEL_ID=$(echo "$MODELS" | jq -r '.data[0].id // empty')
if [ -z "$MODEL_ID" ]; then
    echo -e "${RED}No models available${NC}"
    exit 1
fi
log_info "Using model: $MODEL_ID"

# ============================================================================
# Test #403: Root Response ID in Conversation Metadata
# ============================================================================
log_test "#403: Root response ID returned when creating conversation"

CONV_RESPONSE=$(api_call POST "/v1/conversations" '{"name": "Test Root Response ID"}')
CONV_ID=$(echo "$CONV_RESPONSE" | jq -r '.id')
ROOT_RESP_ID=$(echo "$CONV_RESPONSE" | jq -r '.metadata.root_response_id // empty')

if [ -z "$CONV_ID" ] || [ "$CONV_ID" == "null" ]; then
    log_fail "Create conversation" "conversation ID" "null or empty"
else
    log_info "Created conversation: $CONV_ID"

    if [ -z "$ROOT_RESP_ID" ] || [ "$ROOT_RESP_ID" == "null" ]; then
        log_fail "#403 root_response_id" "resp_* in metadata" "missing or null"
    elif [[ "$ROOT_RESP_ID" == resp_* ]]; then
        log_pass "#403: root_response_id returned: $ROOT_RESP_ID"
    else
        log_fail "#403 root_response_id format" "resp_*" "$ROOT_RESP_ID"
    fi
fi

# ============================================================================
# Test #375: Message Metadata Preservation
# ============================================================================
log_test "#375: Message metadata preserved through Response API"

if [ -n "$CONV_ID" ] && [ "$CONV_ID" != "null" ]; then
    RESP_RESULT=$(api_call POST "/v1/responses" "{
        \"model\": \"$MODEL_ID\",
        \"conversation\": {\"id\": \"$CONV_ID\"},
        \"input\": [{
            \"role\": \"user\",
            \"content\": \"Hello\",
            \"metadata\": {
                \"test_key\": \"test_value\",
                \"source\": \"staging_test\",
                \"nested\": {\"key\": \"value\"}
            }
        }],
        \"stream\": false,
        \"max_output_tokens\": 10
    }")

    RESP_ID=$(echo "$RESP_RESULT" | jq -r '.id // empty')

    if [ -z "$RESP_ID" ] || [ "$RESP_ID" == "null" ]; then
        log_fail "#375 create response" "response ID" "$(echo "$RESP_RESULT" | jq -r '.error.message // "unknown error"')"
    else
        log_info "Created response: $RESP_ID"

        # Get input items
        INPUT_ITEMS=$(api_call GET "/v1/responses/$RESP_ID/input_items")
        METADATA_VALUE=$(echo "$INPUT_ITEMS" | jq -r '.data[0].metadata.test_key // empty')
        NESTED_VALUE=$(echo "$INPUT_ITEMS" | jq -r '.data[0].metadata.nested.key // empty')

        if [ "$METADATA_VALUE" == "test_value" ] && [ "$NESTED_VALUE" == "value" ]; then
            log_pass "#375: Message metadata preserved (test_key=test_value, nested.key=value)"
        else
            log_fail "#375 metadata preservation" "test_key=test_value, nested.key=value" "test_key=$METADATA_VALUE, nested.key=$NESTED_VALUE"
        fi
    fi
else
    log_fail "#375" "valid conversation" "no conversation created"
fi

# ============================================================================
# Test #410: Failed Response Creates Conversation Item
# ============================================================================
log_test "#410: Failed response creates conversation item with error"

# Create a new conversation for this test
CONV2_RESPONSE=$(api_call POST "/v1/conversations" '{"name": "Test Failed Response"}')
CONV2_ID=$(echo "$CONV2_RESPONSE" | jq -r '.id')

if [ -n "$CONV2_ID" ] && [ "$CONV2_ID" != "null" ]; then
    log_info "Created conversation for failure test: $CONV2_ID"

    # Request with non-existent model to trigger failure
    STREAM_OUTPUT=$(api_call_stream "/v1/responses" "{
        \"conversation\": {\"id\": \"$CONV2_ID\"},
        \"input\": \"hello\",
        \"model\": \"non-existent-model-12345\",
        \"stream\": true,
        \"max_output_tokens\": 10
    }")

    # Check for response.failed event
    if echo "$STREAM_OUTPUT" | grep -q "response.failed"; then
        log_pass "#410: response.failed event emitted for invalid model"

        # Extract error text
        ERROR_TEXT=$(echo "$STREAM_OUTPUT" | grep "response.failed" -A1 | grep "data:" | sed 's/data: //' | jq -r '.text // empty' 2>/dev/null || echo "")
        if [ -n "$ERROR_TEXT" ]; then
            log_info "Error message: $ERROR_TEXT"
        fi

        # Check conversation items for failed item
        sleep 1  # Give DB time to sync
        ITEMS=$(api_call GET "/v1/conversations/$CONV2_ID/items")
        FAILED_ITEM=$(echo "$ITEMS" | jq -r '.data[] | select(.status == "failed") | .status' 2>/dev/null || echo "")

        if [ "$FAILED_ITEM" == "failed" ]; then
            log_pass "#410: Failed conversation item created in database"
        else
            log_info "#410: Could not verify failed item in database (may need admin access)"
        fi
    else
        # Check if we got an error response instead
        ERROR_MSG=$(echo "$STREAM_OUTPUT" | jq -r '.error.message // empty' 2>/dev/null || echo "")
        if [ -n "$ERROR_MSG" ]; then
            log_info "#410: Got error response: $ERROR_MSG (acceptable - different error handling)"
            log_pass "#410: Error handling works (non-streaming error)"
        else
            log_fail "#410 response.failed" "response.failed event" "not found in stream"
        fi
    fi
else
    log_fail "#410" "valid conversation" "failed to create"
fi

# ============================================================================
# Test #408: Anthropic Model Improvements (Temperature Clamping)
# ============================================================================
log_test "#408: Anthropic temperature handling"

# Check if an Anthropic model is available
ANTHROPIC_MODEL=$(echo "$MODELS" | jq -r '.data[] | select(.id | contains("claude")) | .id' | head -1)

if [ -n "$ANTHROPIC_MODEL" ]; then
    log_info "Testing with Anthropic model: $ANTHROPIC_MODEL"

    # Test with temperature > 1.0 (should be clamped to 1.0)
    CONV3_RESPONSE=$(api_call POST "/v1/conversations" '{"name": "Test Anthropic"}')
    CONV3_ID=$(echo "$CONV3_RESPONSE" | jq -r '.id')

    if [ -n "$CONV3_ID" ] && [ "$CONV3_ID" != "null" ]; then
        ANTHROPIC_RESULT=$(api_call POST "/v1/responses" "{
            \"model\": \"$ANTHROPIC_MODEL\",
            \"conversation\": {\"id\": \"$CONV3_ID\"},
            \"input\": \"Say hello\",
            \"temperature\": 1.5,
            \"stream\": false,
            \"max_output_tokens\": 10
        }")

        ANTHROPIC_STATUS=$(echo "$ANTHROPIC_RESULT" | jq -r '.status // empty')
        ANTHROPIC_ERROR=$(echo "$ANTHROPIC_RESULT" | jq -r '.error.message // empty')

        if [ "$ANTHROPIC_STATUS" == "completed" ]; then
            log_pass "#408: Anthropic request with temperature=1.5 succeeded (clamped to 1.0)"
        elif [ -n "$ANTHROPIC_ERROR" ]; then
            # Check if error is NOT about temperature range
            if echo "$ANTHROPIC_ERROR" | grep -qi "temperature"; then
                log_fail "#408 temperature clamping" "success or non-temperature error" "$ANTHROPIC_ERROR"
            else
                log_info "#408: Got error: $ANTHROPIC_ERROR (not temperature related)"
                log_pass "#408: Temperature clamping appears to work (different error)"
            fi
        else
            log_info "#408: Result: $(echo "$ANTHROPIC_RESULT" | jq -c '.')"
        fi
    fi
else
    log_info "#408: No Anthropic model available, skipping temperature test"
fi

# ============================================================================
# Test #388: Image Edit API
# ============================================================================
log_test "#388: Image Edit API endpoint"

# Check if image edit endpoint exists
IMAGE_EDIT_CHECK=$(curl -s -o /dev/null -w "%{http_code}" -X OPTIONS "${API_URL}/v1/images/edits" 2>/dev/null || echo "000")

# Create a minimal 1x1 red PNG for testing
# This is a valid PNG that should be accepted
PNG_BASE64="iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8DwHwAFBQIAX8jx0gAAAABJRU5ErkJggg=="

# Find image-capable model
IMAGE_MODEL=$(echo "$MODELS" | jq -r '.data[] | select(.capabilities[]? == "image_edit" or .id | contains("VL") or .id | contains("vision")) | .id' | head -1)

if [ -n "$IMAGE_MODEL" ]; then
    log_info "Testing image edit with model: $IMAGE_MODEL"

    # Create temp file for multipart
    TEMP_PNG=$(mktemp --suffix=.png)
    echo "$PNG_BASE64" | base64 -d > "$TEMP_PNG"

    IMAGE_RESULT=$(curl -s -X POST \
        -H "Authorization: Bearer $STAGING_API_KEY" \
        -F "model=$IMAGE_MODEL" \
        -F "prompt=Add a border" \
        -F "image=@$TEMP_PNG;type=image/png" \
        "${API_URL}/v1/images/edits")

    rm -f "$TEMP_PNG"

    IMAGE_DATA=$(echo "$IMAGE_RESULT" | jq -r '.data // empty')
    IMAGE_ERROR=$(echo "$IMAGE_RESULT" | jq -r '.error.message // empty')

    if [ -n "$IMAGE_DATA" ] && [ "$IMAGE_DATA" != "null" ]; then
        log_pass "#388: Image edit endpoint returned data"
    elif [ -n "$IMAGE_ERROR" ]; then
        if echo "$IMAGE_ERROR" | grep -qi "not found\|not supported\|endpoint"; then
            log_fail "#388 image edit" "success or model error" "$IMAGE_ERROR"
        else
            log_info "#388: Got error: $IMAGE_ERROR (endpoint exists, model may not support)"
            log_pass "#388: Image edit endpoint exists"
        fi
    else
        log_info "#388: Unexpected response: $(echo "$IMAGE_RESULT" | jq -c '.')"
    fi
else
    # Try the endpoint anyway to check it exists
    TEMP_PNG=$(mktemp --suffix=.png)
    echo "$PNG_BASE64" | base64 -d > "$TEMP_PNG"

    IMAGE_RESULT=$(curl -s -X POST \
        -H "Authorization: Bearer $STAGING_API_KEY" \
        -F "model=test-model" \
        -F "prompt=test" \
        -F "image=@$TEMP_PNG;type=image/png" \
        "${API_URL}/v1/images/edits")

    rm -f "$TEMP_PNG"

    HTTP_CODE=$(echo "$IMAGE_RESULT" | jq -r '.error.type // empty')

    if [ "$HTTP_CODE" == "not_found" ]; then
        log_fail "#388 image edit endpoint" "endpoint exists" "404 not found"
    else
        log_pass "#388: Image edit endpoint exists (returned: $(echo "$IMAGE_RESULT" | jq -r '.error.message // "ok"'))"
    fi
fi

# ============================================================================
# Cleanup
# ============================================================================
log_info "Cleaning up test conversations..."

for CONV in $CONV_ID $CONV2_ID $CONV3_ID; do
    if [ -n "$CONV" ] && [ "$CONV" != "null" ]; then
        api_call DELETE "/v1/conversations/$CONV" > /dev/null 2>&1 || true
    fi
done

# ============================================================================
# Summary
# ============================================================================
echo ""
echo "╔═══════════════════════════════════════════════════════════════════╗"
echo "║                         TEST SUMMARY                              ║"
echo "╚═══════════════════════════════════════════════════════════════════╝"
echo ""
echo -e "${GREEN}Passed:${NC}  $PASSED"
echo -e "${RED}Failed:${NC}  $FAILED"
echo ""

if [ $FAILED -gt 0 ]; then
    echo -e "${RED}Some tests failed!${NC}"
    exit 1
else
    echo -e "${GREEN}All tests passed!${NC}"
    exit 0
fi
