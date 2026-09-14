# Maestro — ergonomic Make targets for the native Rust product
# Auto-loads .env when present (falls back to shell env otherwise).
# Only the vars below are exported — bare `export` would leak MAKEFLAGS etc.
-include .env
export ANTHROPIC_API_KEY OPENAI_API_KEY GEMINI_API_KEY GROQ_API_KEY \
       OPENROUTER_API_KEY XAI_API_KEY EXA_API_KEY \
       MAESTRO_CEREBRO_URL CEREBRO_URL CEREBRO_SERVICE_URL \
       MAESTRO_CEREBRO_TOKEN CEREBRO_TOKEN \
       MAESTRO_EVALOPS_ACCESS_TOKEN EVALOPS_TOKEN \
       MAESTRO_CEREBRO_WORKSPACE_ID CEREBRO_WORKSPACE_ID \
       MAESTRO_AGENT_RUNTIME_WORKSPACE_ID AGENT_RUNTIME_WORKSPACE_ID \
       MAESTRO_WORKSPACE_ID MAESTRO_EVALOPS_WORKSPACE_ID EVALOPS_WORKSPACE_ID \
       MAESTRO_REMOTE_RUNNER_WORKSPACE_ID \
       MAESTRO_CEREBRO_TIMEOUT_MS CEREBRO_TIMEOUT_MS \
       MAESTRO_CEREBRO_MAX_ATTEMPTS CEREBRO_MAX_ATTEMPTS \
       MAESTRO_CEREBRO_SEARCH_LIMIT CEREBRO_SEARCH_LIMIT \
       MAESTRO_CEREBRO_CHANGE_LIMIT CEREBRO_CHANGE_LIMIT \
       LOCAL_HTTP_PORT LOCAL_ADDR LOCAL_BASE_URL \
       MAESTRO_PLATFORM_MCP_ENABLED MAESTRO_AGENT_MCP_ENABLED \
       MAESTRO_PLATFORM_MCP_NAME MAESTRO_AGENT_MCP_NAME \
       MAESTRO_PLATFORM_MCP_URL MAESTRO_AGENT_MCP_URL MAESTRO_EVALOPS_AGENT_MCP_URL \
       EVALOPS_AGENT_MCP_URL MAESTRO_PLATFORM_MCP_MANIFEST_URL \
       MAESTRO_AGENT_MCP_MANIFEST_URL MAESTRO_EVALOPS_AGENT_MCP_MANIFEST_URL \
       MAESTRO_PLATFORM_MCP_TOKEN MAESTRO_AGENT_MCP_TOKEN \
       MAESTRO_CEREBRO_MCP_SCOPES MAESTRO_PLATFORM_MCP_SCOPES \
       MAESTRO_AGENT_MCP_SCOPES MAESTRO_EVALOPS_AGENT_MCP_SCOPES \
       MAESTRO_EVALOPS_MEMORY_MODE EVALOPS_MEMORY_MODE

# GNU Make exports an undefined variable as an empty string. An absent model
# override must stay absent so configuration files can select the model.
ifneq ($(origin MAESTRO_MODEL),undefined)
export MAESTRO_MODEL
endif
ifneq ($(origin MAESTRO_MODEL_PROVIDER),undefined)
export MAESTRO_MODEL_PROVIDER
endif

LOCAL_CEREBRO_REPO ?= ../cerebro

.PHONY: help setup build build-all run-rs run-rs-debug \
        web test lint check fmt \
        smoke cerebro-dev cerebro-env cerebro-e2e cerebro-e2e-doctor cerebro-e2e-trace evals verify clean db-up db-down

help: ## Show this help
	@grep -E '^[a-zA-Z_-]+:.*?## .*$$' $(MAKEFILE_LIST) | sort | \
		awk 'BEGIN {FS = ":.*?## "}; {printf "  \033[36m%-15s\033[0m %s\n", $$1, $$2}'

setup: ## First-time project bootstrap
	@test -f .env || { test -f .env.example || { echo "error: .env.example not found — is this a complete checkout?" >&2; exit 1; }; \
		cp .env.example .env && echo "Created .env from .env.example — add your API keys"; }
	npm run build:all
	@echo "\nReady! Run 'make run-rs' or 'make help' for all targets."

build: ## Build the native Rust CLI
	npm run build

build-all: ## Build the native product
	npm run build:all

run-rs: build ## Launch Rust TUI (release)
	./target/release/maestro

run-rs-debug: ## Launch Rust TUI (debug build)
	cargo build -p maestro && \
	./target/debug/maestro

web: ## Launch the native web runtime gateway
	npm run web

test: ## Full test suite
	npm test

lint: ## Rust formatting + clippy checks
	npm run lint

check: lint test ## Full CI check (lint + test)

fmt: ## Format Rust sources
	npm run format

smoke: build ## Smoke-test the built CLI
	npm run smoke

cerebro-e2e-doctor: ## Preflight the cross-repo Cerebro local E2E lane
	LOCAL_CEREBRO_REPO="$(LOCAL_CEREBRO_REPO)" node scripts/check-cerebro-e2e.mjs

cerebro-env: ## Print env exports for running this Maestro checkout against local Cerebro
	@LOCAL_CEREBRO_REPO="$(LOCAL_CEREBRO_REPO)" node scripts/check-cerebro-e2e.mjs --print-maestro-env

cerebro-dev: cerebro-e2e-doctor ## Start a usable local Cerebro stack with Maestro event ingestion enabled
	@env_exports="$$(LOCAL_CEREBRO_REPO="$(LOCAL_CEREBRO_REPO)" node scripts/check-cerebro-e2e.mjs --print-env)" && \
		eval "$$env_exports" && \
		LOCAL_MAESTRO_REPO="$(CURDIR)" \
		LOCAL_HTTP_PORT="$$LOCAL_HTTP_PORT" \
		LOCAL_ADDR="$$LOCAL_ADDR" \
		LOCAL_BASE_URL="$$LOCAL_BASE_URL" \
		MAESTRO_CEREBRO_URL="$$MAESTRO_CEREBRO_URL" \
		MAESTRO_CEREBRO_WORKSPACE_ID="$$MAESTRO_CEREBRO_WORKSPACE_ID" \
		MAESTRO_WORKSPACE_ID="$$MAESTRO_WORKSPACE_ID" \
		MAESTRO_PLATFORM_MCP_URL="$$MAESTRO_PLATFORM_MCP_URL" \
		MAESTRO_AGENT_MCP_URL="$$MAESTRO_AGENT_MCP_URL" \
		MAESTRO_CEREBRO_MCP_SCOPES="$$MAESTRO_CEREBRO_MCP_SCOPES" \
		MAESTRO_PLATFORM_MCP_SCOPES="$$MAESTRO_PLATFORM_MCP_SCOPES" \
		MAESTRO_AGENT_MCP_SCOPES="$$MAESTRO_AGENT_MCP_SCOPES" \
		LOCAL_MAESTRO_GENERATE_REPLAY="$$LOCAL_MAESTRO_GENERATE_REPLAY" \
		LOCAL_MAESTRO_DOCTOR_REPLAY="$$LOCAL_MAESTRO_DOCTOR_REPLAY" \
		$(MAKE) -C "$(LOCAL_CEREBRO_REPO)" local-maestro-dev

cerebro-e2e: cerebro-e2e-doctor ## Run the cross-repo Cerebro local E2E using this Maestro checkout
	@env_exports="$$(LOCAL_CEREBRO_REPO="$(LOCAL_CEREBRO_REPO)" node scripts/check-cerebro-e2e.mjs --print-env)" && \
		eval "$$env_exports" && \
		LOCAL_MAESTRO_REPO="$(CURDIR)" \
		LOCAL_HTTP_PORT="$$LOCAL_HTTP_PORT" \
		LOCAL_ADDR="$$LOCAL_ADDR" \
		LOCAL_BASE_URL="$$LOCAL_BASE_URL" \
		MAESTRO_CEREBRO_URL="$$MAESTRO_CEREBRO_URL" \
		MAESTRO_CEREBRO_WORKSPACE_ID="$$MAESTRO_CEREBRO_WORKSPACE_ID" \
		MAESTRO_WORKSPACE_ID="$$MAESTRO_WORKSPACE_ID" \
		MAESTRO_PLATFORM_MCP_URL="$$MAESTRO_PLATFORM_MCP_URL" \
		MAESTRO_AGENT_MCP_URL="$$MAESTRO_AGENT_MCP_URL" \
		MAESTRO_CEREBRO_MCP_SCOPES="$$MAESTRO_CEREBRO_MCP_SCOPES" \
		MAESTRO_PLATFORM_MCP_SCOPES="$$MAESTRO_PLATFORM_MCP_SCOPES" \
		MAESTRO_AGENT_MCP_SCOPES="$$MAESTRO_AGENT_MCP_SCOPES" \
		LOCAL_MAESTRO_GENERATE_REPLAY="$$LOCAL_MAESTRO_GENERATE_REPLAY" \
		LOCAL_MAESTRO_DOCTOR_REPLAY="$$LOCAL_MAESTRO_DOCTOR_REPLAY" \
		$(MAKE) -C "$(LOCAL_CEREBRO_REPO)" local-maestro-e2e

cerebro-e2e-trace: export LOCAL_REQUIRE_CEREBRO_TRACE_TARGET=true
cerebro-e2e-trace: cerebro-e2e-doctor ## Run the trace-backed Maestro/Cerebro local E2E and prove it in Jaeger
	@env_exports="$$(LOCAL_CEREBRO_REPO="$(LOCAL_CEREBRO_REPO)" node scripts/check-cerebro-e2e.mjs --print-env)" && \
		eval "$$env_exports" && \
		LOCAL_MAESTRO_REPO="$(CURDIR)" \
		LOCAL_HTTP_PORT="$$LOCAL_HTTP_PORT" \
		LOCAL_ADDR="$$LOCAL_ADDR" \
		LOCAL_BASE_URL="$$LOCAL_BASE_URL" \
		MAESTRO_CEREBRO_URL="$$MAESTRO_CEREBRO_URL" \
		MAESTRO_CEREBRO_WORKSPACE_ID="$$MAESTRO_CEREBRO_WORKSPACE_ID" \
		MAESTRO_WORKSPACE_ID="$$MAESTRO_WORKSPACE_ID" \
		MAESTRO_PLATFORM_MCP_URL="$$MAESTRO_PLATFORM_MCP_URL" \
		MAESTRO_AGENT_MCP_URL="$$MAESTRO_AGENT_MCP_URL" \
		MAESTRO_CEREBRO_MCP_SCOPES="$$MAESTRO_CEREBRO_MCP_SCOPES" \
		MAESTRO_PLATFORM_MCP_SCOPES="$$MAESTRO_PLATFORM_MCP_SCOPES" \
		MAESTRO_AGENT_MCP_SCOPES="$$MAESTRO_AGENT_MCP_SCOPES" \
		LOCAL_MAESTRO_GENERATE_REPLAY="$$LOCAL_MAESTRO_GENERATE_REPLAY" \
		LOCAL_MAESTRO_DOCTOR_REPLAY="$$LOCAL_MAESTRO_DOCTOR_REPLAY" \
		$(MAKE) -C "$(LOCAL_CEREBRO_REPO)" local-e2e-trace

evals: ## Run eval scenarios
	npm run check:scenario-replay-gate

verify: fmt lint test build smoke ## Full verification (format + lint + test + build + smoke)

clean: ## Remove build artifacts
	npm run clean

db-up: ## Start Redis + PostgreSQL (Docker)
	docker compose up -d

db-down: ## Stop Docker services
	docker compose down
