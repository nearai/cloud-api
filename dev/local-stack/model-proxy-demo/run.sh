#!/usr/bin/env bash
# model-proxy control-plane demo: drives the REAL model-proxy binary through
# the GLM-5.2 long-context registration flow.
#
#   1. dual probes on one "host": canonical id on :18000, synthetic -long id
#      on :18001, sharing one routed backend (:18444)
#   2. shared-backend dedupe: both domains -> the SAME backend, total_backends=1
#   3. probe-cleanup OWNERSHIP GUARD (model-proxy PR #42): unregistering the
#      :18001 probe must NOT drop the shared backend the :18000 probe still owns
#   4. health-gated stub + circuit breaker: when the "engine" dies, the stub
#      5xxes -> the -long domain drains within a discovery cycle and the
#      breaker opens (non-2xx = unhealthy, also PR #42)
#
# Requires a model-proxy checkout (branch probe-cleanup-ownership for the
# guard steps): MODEL_PROXY_DIR=… ./run.sh
set -euo pipefail
cd "$(dirname "$0")"

MODEL_PROXY_DIR=${MODEL_PROXY_DIR:-$(cd ../../../.. && pwd)/model-proxy}
ADMIN=http://127.0.0.1:19090
PASS=0; FAIL=0

say()   { echo; echo "== $*"; }
check() { # name cond-cmd
  if eval "$2" >/dev/null 2>&1; then PASS=$((PASS+1)); echo "  PASS  $1";
  else FAIL=$((FAIL+1)); echo "  FAIL  $1"; fi
}
reg()   { curl -sf -X POST "$ADMIN/$1" -H 'Content-Type: application/json' -d "$2" >/dev/null; }
registry() { curl -sf "$ADMIN/registry"; }
cleanup_all() {
  [ -n "${MP_PID:-}" ] && kill "$MP_PID" 2>/dev/null || true
  [ -n "${STUB_PID:-}" ] && kill "$STUB_PID" 2>/dev/null || true
  [ -n "${ENGINE_PID:-}" ] && kill "$ENGINE_PID" 2>/dev/null || true
}
trap cleanup_all EXIT

say "building model-proxy from $MODEL_PROXY_DIR"
(cd "$MODEL_PROXY_DIR" && cargo build -q 2>&1 | tail -1 || true)
MP_BIN="$MODEL_PROXY_DIR/target/debug/model-proxy"
[ -x "$MP_BIN" ] || { echo "model-proxy binary not found at $MP_BIN"; exit 1; }

say "starting probe stubs (:18000 canonical, :18001 health-gated synthetic) + fake engine (:18010)"
python3 probe_stub.py > probe_stub.log 2>&1 & 
STUB_PID=$!
sleep 0.5

say "starting model-proxy (discovery 2s, health 2s, breaker threshold 2)"
rm -rf run && mkdir -p run && cp config.yaml run/
(cd run && exec "$MP_BIN" > model-proxy.log 2>&1) &
MP_PID=$!
for _ in $(seq 1 20); do curl -sf "$ADMIN/health" >/dev/null 2>&1 && break; sleep 0.5; done
curl -sf "$ADMIN/health" >/dev/null || { echo "model-proxy admin API not up"; tail -20 run/model-proxy.log; exit 1; }

say "registering dual probes + model->domain mappings"
reg register/endpoint '{"endpoint":"127.0.0.1:18000","routing_port":18444}'
reg register/endpoint '{"endpoint":"127.0.0.1:18001","routing_port":18444}'
reg register/model    '{"model":"z-ai/glm-5.2-e2e","domain":"glm-5-2-e2e.local"}'
reg register/model    '{"model":"z-ai/glm-5.2-e2e-long","domain":"glm-5-2-e2e-long.local"}'
sleep 5   # ≥2 discovery cycles

say "1+2: both domains share ONE routed backend"
REG=$(registry)
check "base domain has the backend"  "echo '$REG' | python3 -c 'import sys,json; d=json.load(sys.stdin); assert d[\"domains\"][\"glm-5-2-e2e.local\"][0][\"address\"]==\"127.0.0.1:18444\"'"
check "long domain has the backend"  "echo '$REG' | python3 -c 'import sys,json; d=json.load(sys.stdin); assert d[\"domains\"][\"glm-5-2-e2e-long.local\"][0][\"address\"]==\"127.0.0.1:18444\"'"
check "backend deduped (total=1)"    "echo '$REG' | python3 -c 'import sys,json; assert json.load(sys.stdin)[\"total_backends\"]==1'"

say "3: ownership guard — unregister the :18001 probe, shared backend must survive"
reg unregister/endpoint '{"endpoint":"127.0.0.1:18001"}'
REG=$(registry)
check "shared backend survives (total=1)" "echo '$REG' | python3 -c 'import sys,json; assert json.load(sys.stdin)[\"total_backends\"]==1'"
check "base domain still routable"        "curl -sf '$ADMIN/backends/count?domain=glm-5-2-e2e.local' | python3 -c 'import sys,json; assert json.load(sys.stdin)[\"total\"]==1'"
sleep 3
check "long domain drained by next rebuild" "! curl -sf '$ADMIN/backends/count?domain=glm-5-2-e2e-long.local' >/dev/null 2>&1 || curl -s '$ADMIN/backends/count?domain=glm-5-2-e2e-long.local' | python3 -c 'import sys,json; assert json.load(sys.stdin)[\"total\"]==0'"

say "4: health-gated stub — kill the engine, breaker must OPEN and long domain drain"
reg register/endpoint '{"endpoint":"127.0.0.1:18001","routing_port":18444}'
sleep 5
touch ENGINE_DOWN     # probe_stub gates :18001 (and :18000) on this file, like the nginx auth_request gate
sleep 8               # a few health cycles at threshold 2
REG=$(registry)
check "circuit breaker opened on engine death" "echo '$REG' | python3 -c 'import sys,json; d=json.load(sys.stdin); assert d[\"healthy_backends\"]==0, d'"
check "long domain drained"                    "echo '$REG' | python3 -c 'import sys,json; d=json.load(sys.stdin); assert \"glm-5-2-e2e-long.local\" not in d[\"domains\"] or not d[\"domains\"][\"glm-5-2-e2e-long.local\"], d'"
rm -f ENGINE_DOWN
sleep 10              # cooldown 5s + half-open recovery
REG=$(registry)
check "breaker recovers when engine returns"   "echo '$REG' | python3 -c 'import sys,json; d=json.load(sys.stdin); assert d[\"healthy_backends\"]==1, d'"

echo
echo "================ model-proxy control-plane demo ================"
echo "PASS=$PASS FAIL=$FAIL"
[ "$FAIL" = 0 ]
