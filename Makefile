.PHONY: help seed api dev build test test-unit test-integration lint fmt check-clippy check-fmt preflight clean

help:
	@echo "NEAR AI Cloud API - Development Commands"
	@echo ""
	@echo "Quick Start:"
	@echo "  make dev           Seed database and run API server (recommended)"
	@echo ""
	@echo "Setup & Database:"
	@echo "  make seed          Run database migrations and seed data"
	@echo ""
	@echo "Running Services:"
	@echo "  make api           Run the API server (port 3000)"
	@echo ""
	@echo "Code Quality:"
	@echo "  make preflight     Run all checks before committing (lint, fmt, test, build)"
	@echo "  make build         Build all crates"
	@echo "  make test          Run all tests (unit + integration)"
	@echo "  make test-unit     Run unit tests only"
	@echo "  make test-integration  Run integration/e2e tests (requires database)"
	@echo "  make lint          Run clippy linter (strict mode)"
	@echo "  make fmt           Format code with rustfmt"
	@echo "  make check-clippy  Check clippy without fixing"
	@echo "  make check-fmt     Check formatting without fixing"
	@echo ""
	@echo "Cleanup:"
	@echo "  make clean         Remove build artifacts"

## Database & Seeding

seed:
	@echo "Running database migrations and seeding..."
	cargo run --bin seed -p database

## Services

api:
	@echo "Starting API server on http://localhost:3000"
	@echo "  Documentation: http://localhost:3000/docs"
	@echo "  OpenAPI spec: http://localhost:3000/api-docs/openapi.json"
	@echo ""
	cargo run --bin api

dev:
	@echo "Starting development environment..."
	@echo ""
	cargo run --bin seed -p database && \
	echo "" && \
	echo "Seed complete! Starting API server on http://localhost:3000" && \
	echo "  Documentation: http://localhost:3000/docs" && \
	echo "  OpenAPI spec: http://localhost:3000/api-docs/openapi.json" && \
	echo "" && \
	cargo run --bin api

## Building & Testing

build:
	@echo "Building all crates..."
	cargo build

test: test-unit test-integration
	@echo "All tests completed."

test-unit:
	@echo "Running unit tests..."
	cargo test --lib --bins

test-integration:
	@echo "Running integration/e2e tests..."
	cargo test --test '*'

lint:
	@echo "Running clippy linter (strict mode)..."
	cargo clippy --lib --bins -- -D warnings

fmt:
	@echo "Formatting code with rustfmt..."
	cargo fmt

check-clippy:
	@echo "Checking clippy (without fixing)..."
	cargo clippy --lib --bins -- -D warnings

check-fmt:
	@echo "Checking code formatting (without fixing)..."
	cargo fmt --check

preflight: lint fmt test build
	@echo ""
	@echo "✅ All preflight checks passed! Ready to commit."

## Cleanup

clean:
	@echo "Removing build artifacts..."
	cargo clean

.DEFAULT_GOAL := help
