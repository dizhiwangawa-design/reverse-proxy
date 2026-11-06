SHELL := /bin/bash
WEB_DIR := web
BIN := target/release/workbuddy-proxy

.PHONY: all build release web-install web test check fmt clippy smoke smoke-local run clean

all: build

# Build the UI first, then embed it into the release binary.
build: web
	cargo build --release

release: build

web-install:
	cd $(WEB_DIR) && npm ci

# node_modules is only re-installed when package-lock.json changes.
$(WEB_DIR)/node_modules/.package-lock.json: $(WEB_DIR)/package-lock.json
	cd $(WEB_DIR) && npm ci

web: $(WEB_DIR)/node_modules/.package-lock.json
	cd $(WEB_DIR) && npm run build

test:
	cargo test

fmt:
	cargo fmt

clippy:
	cargo clippy --all-targets -- -D warnings

check:
	cargo fmt --check
	cargo clippy --all-targets -- -D warnings
	cd $(WEB_DIR) && npx tsc --noEmit
	cargo test

smoke:
	./scripts/smoke.sh

smoke-local:
	cargo build
	./scripts/local-smoke.sh

run: web
	cargo run

clean:
	cargo clean
