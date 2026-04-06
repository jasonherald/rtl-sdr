.PHONY: all build test clippy fmt fmt-check lint deny audit clean install

all: lint

build:
	cargo build --workspace --locked

test:
	cargo test --workspace --locked

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

PREFIX ?= /usr/local
BINDIR ?= $(PREFIX)/bin
DATADIR ?= $(PREFIX)/share

install: build
	install -Dm755 target/release/sdr $(DESTDIR)$(BINDIR)/sdr-rs
	install -Dm644 data/com.sdr.rs.desktop $(DESTDIR)$(DATADIR)/applications/com.sdr.rs.desktop
	install -Dm644 data/com.sdr.rs.svg $(DESTDIR)$(DATADIR)/icons/hicolor/scalable/apps/com.sdr.rs.svg

uninstall:
	rm -f $(DESTDIR)$(BINDIR)/sdr-rs
	rm -f $(DESTDIR)$(DATADIR)/applications/com.sdr.rs.desktop
	rm -f $(DESTDIR)$(DATADIR)/icons/hicolor/scalable/apps/com.sdr.rs.svg
