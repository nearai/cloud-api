SHELL := /bin/bash
STACK_DIR := $(dir $(abspath $(lastword $(MAKEFILE_LIST))))
REPO_ROOT := $(abspath $(STACK_DIR)/../..)
MODEL_PROXY_DIR ?= $(REPO_ROOT)/../model-proxy

.PHONY: up shims api seed demo proxy-demo down logs

## Bring up the containers (Postgres + llama.cpp engine; first start downloads
## the ~400MB GGUF into ./cache) and the two tier shims.
up:
	cd $(STACK_DIR) && docker compose up -d --wait
	cd $(STACK_DIR) && (python3 tier_shim.py --tier base --port 18100 --ctx-window 1000 --model-id z-ai/glm-5.2-local > shim-base.log 2>&1 & echo $$! > .shim-base.pid)
	cd $(STACK_DIR) && (python3 tier_shim.py --tier long --port 18101 --ctx-window 8000 --model-id z-ai/glm-5.2-local > shim-long.log 2>&1 & echo $$! > .shim-long.pid)
	@sleep 1 && curl -sf http://127.0.0.1:18100/health >/dev/null && curl -sf http://127.0.0.1:18101/health >/dev/null \
		&& echo "engine + shims up (base :18100 ctx=1000, long :18101 ctx=8000)"

## Run cloud-api in the foreground against the stack (debug build; DEV=1
## ephemeral signing keys). Ctrl-C to stop.
api:
	cd $(REPO_ROOT) && set -a && source $(STACK_DIR)/env.local-stack && set +a && cargo run --bin api

## Seed model + org + API key (needs `make api` running in another terminal).
seed:
	$(STACK_DIR)/seed.sh

## Tier-routing demo (small/oversize/boundary/streaming/saturated).
demo:
	$(STACK_DIR)/demo.sh

## Model-proxy control-plane demo (dual-probe registration, shared-backend
## ownership guard, health-gated stub + circuit breaker). Needs a model-proxy
## checkout at $(MODEL_PROXY_DIR) (override: make proxy-demo MODEL_PROXY_DIR=…).
proxy-demo:
	MODEL_PROXY_DIR=$(MODEL_PROXY_DIR) $(STACK_DIR)/model-proxy-demo/run.sh

down:
	-cd $(STACK_DIR) && for f in .shim-base.pid .shim-long.pid; do [ -f $$f ] && kill $$(cat $$f) 2>/dev/null; rm -f $$f; done
	-cd $(STACK_DIR) && rm -f SATURATE-base SATURATE-long
	cd $(STACK_DIR) && docker compose down

logs:
	@cd $(STACK_DIR) && tail -n 40 shim-base.log shim-long.log 2>/dev/null || true
