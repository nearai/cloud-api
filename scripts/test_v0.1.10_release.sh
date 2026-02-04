#!/bin/bash
# Test suite for changes since v0.1.10
# Commits covered:
#   c39101b - fix: add failed conversation item when response initially failed (#410)
#   513e580 - fix: Pass metadata into ConversationItem::Message (#375)
#   77be94b - feat: add credit type and source tracking to organization limits (#318)
#   acfc8ef - feat: return root response ID in metadata when creating conversation (#403)
#   9fa9876 - Improvements for external models (#408)
#   59126ee - feat: add image edit API endpoint (#388)

set -e

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m' # No Color

PASSED=0
FAILED=0
SKIPPED=0

log_section() {
    echo ""
    echo "============================================"
    echo -e "${YELLOW}$1${NC}"
    echo "============================================"
}

log_pass() {
    echo -e "${GREEN}✓ PASS${NC}: $1"
    ((PASSED++))
}

log_fail() {
    echo -e "${RED}✗ FAIL${NC}: $1"
    ((FAILED++))
}

log_skip() {
    echo -e "${YELLOW}⊘ SKIP${NC}: $1"
    ((SKIPPED++))
}

run_test() {
    local test_name="$1"
    local test_cmd="$2"
    local description="$3"

    echo ""
    echo "Running: $description"
    echo "Command: $test_cmd"

    if eval "$test_cmd" > /tmp/test_output_$$.txt 2>&1; then
        log_pass "$test_name"
        return 0
    else
        log_fail "$test_name"
        echo "Output:"
        tail -50 /tmp/test_output_$$.txt
        return 1
    fi
}

# Check prerequisites
log_section "Checking Prerequisites"

if ! command -v cargo &> /dev/null; then
    echo "cargo not found. Please install Rust."
    exit 1
fi

if ! pg_isready -h localhost -p 5432 &> /dev/null; then
    echo -e "${YELLOW}Warning: PostgreSQL not detected on localhost:5432${NC}"
    echo "Some tests may fail. Start PostgreSQL with:"
    echo "  docker run --name test-postgres -e POSTGRES_PASSWORD=postgres -e POSTGRES_DB=platform_api -p 5432:5432 -d postgres:latest"
fi

echo "Prerequisites OK"

# ==============================================================================
log_section "1. Failed Response Conversation Item (#410)"
# Tests: When response fails at start, a failed conversation item is created
# ==============================================================================

run_test \
    "failed_response_creates_item" \
    "cargo test --test e2e_conversations test_response_stream_fails_with_failed_event_when_inference_fails_at_start -- --nocapture" \
    "Test that failed responses create conversation items with error messages"

# ==============================================================================
log_section "2. Message Metadata Preservation (#375)"
# Tests: Input message metadata is preserved through Response API
# ==============================================================================

run_test \
    "metadata_preserved" \
    "cargo test --test e2e_message_metadata test_input_message_metadata_preserved -- --nocapture" \
    "Test input message metadata is preserved"

run_test \
    "metadata_backward_compat" \
    "cargo test --test e2e_message_metadata test_input_message_without_metadata -- --nocapture" \
    "Test backward compatibility without metadata"

run_test \
    "metadata_size_limit" \
    "cargo test --test e2e_message_metadata test_input_message_metadata_size_limit -- --nocapture" \
    "Test metadata size limit validation"

run_test \
    "simple_text_input" \
    "cargo test --test e2e_message_metadata test_simple_text_input_no_metadata -- --nocapture" \
    "Test simple text input works without metadata"

# ==============================================================================
log_section "3. Credit Type & Source Tracking (#318)"
# Tests: Organization limits track credit type and source
# ==============================================================================

run_test \
    "credit_variations" \
    "cargo test --test e2e_credit_types test_add_credits_variations -- --nocapture" \
    "Test adding credits with different types/sources/currencies"

run_test \
    "credits_accumulate" \
    "cargo test --test e2e_credit_types test_credits_accumulate_across_types -- --nocapture" \
    "Test credits accumulate across types"

run_test \
    "credit_type_updates" \
    "cargo test --test e2e_credit_types test_independent_credit_type_updates -- --nocapture" \
    "Test independent credit type updates"

run_test \
    "credit_history" \
    "cargo test --test e2e_credit_types test_history_includes_credit_type_and_source -- --nocapture" \
    "Test history includes credit type and source"

run_test \
    "invalid_credit_type" \
    "cargo test --test e2e_credit_types test_invalid_credit_type_returns_error -- --nocapture" \
    "Test invalid credit type returns error"

# ==============================================================================
log_section "4. Root Response ID in Conversation Metadata (#403)"
# Tests: Creating conversation returns root_response_id in metadata
# ==============================================================================

run_test \
    "root_response_id" \
    "cargo test --test e2e_conversations test_create_and_clone_conversation_return_root_response_id -- --nocapture" \
    "Test create/clone conversation returns root_response_id"

# ==============================================================================
log_section "5. External Model Improvements - Anthropic (#408)"
# Tests: Anthropic temperature clamping and top_p handling (unit tests)
# ==============================================================================

run_test \
    "anthropic_temp_only" \
    "cargo test -p inference_providers test_build_request_temperature_only -- --nocapture" \
    "Test Anthropic with temperature only"

run_test \
    "anthropic_top_p_only" \
    "cargo test -p inference_providers test_build_request_top_p_only -- --nocapture" \
    "Test Anthropic with top_p only"

run_test \
    "anthropic_both_params" \
    "cargo test -p inference_providers test_build_request_both_temperature_and_top_p_prefers_temperature -- --nocapture" \
    "Test Anthropic prefers temperature when both set"

run_test \
    "anthropic_neither_param" \
    "cargo test -p inference_providers test_build_request_neither_temperature_nor_top_p -- --nocapture" \
    "Test Anthropic with neither temperature nor top_p"

run_test \
    "anthropic_temp_clamp" \
    "cargo test -p inference_providers test_build_request_clamps_temperature_to_anthropic_range -- --nocapture" \
    "Test Anthropic clamps temperature to [0, 1]"

# ==============================================================================
log_section "6. Image Edit API (#388)"
# Tests: New /v1/images/edits endpoint
# ==============================================================================

run_test \
    "image_edit_basic" \
    "cargo test --test e2e_audio_image test_image_edit -- --nocapture" \
    "Test basic image edit functionality"

run_test \
    "image_edit_validation" \
    "cargo test --test e2e_audio_image test_image_edit_validation -- --nocapture" \
    "Test image edit validation"

run_test \
    "image_edit_response_format" \
    "cargo test --test e2e_audio_image test_image_edit_verifiable_model_response_format -- --nocapture" \
    "Test image edit response format validation"

# ==============================================================================
log_section "Summary"
# ==============================================================================

echo ""
echo "============================================"
echo "Test Results"
echo "============================================"
echo -e "${GREEN}Passed:${NC}  $PASSED"
echo -e "${RED}Failed:${NC}  $FAILED"
echo -e "${YELLOW}Skipped:${NC} $SKIPPED"
echo "============================================"

if [ $FAILED -gt 0 ]; then
    echo -e "${RED}Some tests failed!${NC}"
    exit 1
else
    echo -e "${GREEN}All tests passed!${NC}"
    exit 0
fi
