SHELL := /bin/sh

NAME := $(shell awk 'BEGIN{in_pkg=0} /^\[package\]/{in_pkg=1; next} /^\[/{in_pkg=0} in_pkg && /^name = / {gsub(/"/ , "", $$3); print $$3; exit}' Cargo.toml)
NAME_UNDERSCORE := $(subst -,_,$(NAME))
ABI_VERSION := $(shell awk 'BEGIN{in_meta=0} /^\[package.metadata.greentic\]/{in_meta=1; next} /^\[/{in_meta=0} in_meta && /^abi_version = / {gsub(/"/ , "", $$3); print $$3; exit}' Cargo.toml)
ABI_VERSION_UNDERSCORE := $(subst .,_,$(ABI_VERSION))
DIST_DIR := dist
WASM_OUT := $(DIST_DIR)/$(NAME)__$(ABI_VERSION_UNDERSCORE).wasm
GREENTIC_COMPONENT ?= greentic-component

.PHONY: build test fmt clippy wasm doctor

build:
	cargo build

test:
	cargo test

fmt:
	cargo fmt

clippy:
	cargo clippy --all-targets --all-features -- -D warnings

wasm:
	if ! cargo component --version >/dev/null 2>&1; then \
		echo "cargo-component is required to produce a valid component@0.6.0 wasm"; \
		echo "install with: cargo install cargo-component --locked"; \
		exit 1; \
	fi
	RUSTFLAGS= CARGO_ENCODED_RUSTFLAGS= $(GREENTIC_COMPONENT) build --manifest ./component.manifest.json
	WASM_SRC=""; \
	for cand in \
		"$${CARGO_TARGET_DIR:-target}/wasm32-wasip2/release/$(NAME_UNDERSCORE).wasm" \
		"$${CARGO_TARGET_DIR:-target}/wasm32-wasip2/release/$(NAME).wasm" \
		"target/wasm32-wasip2/release/$(NAME_UNDERSCORE).wasm" \
		"target/wasm32-wasip2/release/$(NAME).wasm"; do \
		if [ -f "$$cand" ]; then WASM_SRC="$$cand"; break; fi; \
	done; \
	if [ -z "$$WASM_SRC" ]; then \
		echo "unable to locate wasm32-wasip2 component build artifact for $(NAME)"; \
		exit 1; \
	fi; \
	mkdir -p $(DIST_DIR); \
	cp "$$WASM_SRC" $(WASM_OUT); \
	$(GREENTIC_COMPONENT) hash ./component.manifest.json --wasm $(WASM_OUT)

doctor:
	$(GREENTIC_COMPONENT) doctor $(WASM_OUT) --manifest ./component.manifest.json
