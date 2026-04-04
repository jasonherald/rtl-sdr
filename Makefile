.PHONY: all build test clippy fmt fmt-check lint deny audit clean install

all: lint

build:
	cargo build --workspace

test:
	cargo test --workspace

clippy:
	cargo clippy --all-targets --workspace -- -D warnings

fmt:
	cargo fmt --all

fmt-check:
	cargo fmt --all -- --check

deny:
	cargo deny check

audit:
	cargo audit

lint: fmt-check clippy test deny audit

clean:
	cargo clean

install: build
	cargo install --path .
