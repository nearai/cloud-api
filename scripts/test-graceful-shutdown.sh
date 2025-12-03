#!/bin/bash

# Simple curl test commands for the graceful shutdown fix

SERVER_HOST="${SERVER_HOST:-localhost}"
SERVER_PORT="${SERVER_PORT:-3000}"
BASE_URL="http://${SERVER_HOST}:${SERVER_PORT}"

echo "================================"
echo "CURL Test Commands"
echo "================================"
echo "Server: $BASE_URL"
echo ""

# Test 1: Health check
echo "1. Test health endpoint:"
echo "   curl -v $BASE_URL/health"
echo ""
curl -v "$BASE_URL/health" 2>&1 | head -20
echo ""
echo ""

# Test 2: List models (if available)
echo "2. Test models endpoint:"
echo "   curl -v $BASE_URL/v1/models"
echo ""
curl -v "$BASE_URL/v1/models" 2>&1 | head -20
echo ""
echo ""

# Test 3: Continuous load test (press Ctrl+C to trigger shutdown)
echo "3. Run continuous load test (30 requests, press Ctrl+C to stop):"
echo ""
echo "   for i in {1..30}; do"
echo "     curl -s $BASE_URL/health > /dev/null && echo \"Request \$i: OK\" || echo \"Request \$i: FAILED\""
echo "     sleep 0.5"
echo "   done"
echo ""

# Execute continuous load
echo "Starting continuous requests..."
for i in {1..30}; do
    if curl -s "$BASE_URL/health" > /dev/null 2>&1; then
        echo "Request $i: ✓ OK"
    else
        echo "Request $i: ✗ FAILED"
    fi
    sleep 0.5
done

echo ""
echo "================================"
echo "Test Complete"
echo "================================"
echo ""
echo "To test graceful shutdown manually:"
echo ""
echo "1. Start the server:"
echo "   cargo run --bin api"
echo ""
echo "2. In another terminal, run continuous requests:"
echo "   while true; do curl -s http://localhost:3000/health; sleep 0.1; done"
echo ""
echo "3. Press Ctrl+C on the server terminal to trigger graceful shutdown"
echo ""
echo "4. Check logs for:"
echo "   - 'Model discovery refresh task cancelled'"
echo "   - 'Inference provider pool shutdown completed'"
echo "   - 'SHUTDOWN COMPLETE'"
echo ""
