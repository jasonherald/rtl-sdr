# SDR-RS — Software-defined radio application
# Makefile for building, installing, and managing

BINDIR      ?= $(HOME)/.cargo/bin
DATADIR     ?= $(HOME)/.local/share
ICONDIR     ?= $(DATADIR)/icons/hicolor/scalable/apps
DESKTOPDIR  ?= $(DATADIR)/applications
CARGO       ?= cargo
CARGO_FLAGS ?= --release

.PHONY: all build install install-bin install-icon install-desktop \
        uninstall test clippy fmt fmt-check lint deny audit scan clean help \
        ffi-header-check ffi-header-regen

# ─────────────────────────────────────────────────────────────────────
# Default
# ─────────────────────────────────────────────────────────────────────

all: build

help:
	@echo "SDR-RS — Software-defined radio application"
	@echo ""
	@echo "Usage:"
	@echo "  make install      Build release and install (binary + icon + desktop shortcut)"
	@echo "  make uninstall    Remove binary, icon, and desktop shortcut"
	@echo "  make build        Build release binary only"
	@echo "  make test         Run all workspace tests"
	@echo "  make lint         Run all checks (fmt, clippy, test, deny, audit)"
	@echo "  make scan         Run SonarQube scan"
	@echo "  make clean        Remove build artifacts"
	@echo ""
	@echo "Variables:"
	@echo "  BINDIR=<path>    Binary location    (default: ~/.cargo/bin)"
	@echo "  DATADIR=<path>   Data/share prefix  (default: ~/.local/share)"

# ─────────────────────────────────────────────────────────────────────
# Build
# ─────────────────────────────────────────────────────────────────────

build:
	$(CARGO) build --workspace $(CARGO_FLAGS)

# ─────────────────────────────────────────────────────────────────────
# Install
# ─────────────────────────────────────────────────────────────────────

install: build install-bin install-icon install-desktop
	@echo ""
	@echo "SDR-RS installed successfully!"
	@echo "  Binary:   $(BINDIR)/sdr-rs"
	@echo "  Icon:     $(ICONDIR)/com.sdr.rs.svg"
	@echo "  Desktop:  $(DESKTOPDIR)/com.sdr.rs.desktop"
	@echo ""
	@echo "Launch from your app menu or run: sdr-rs"
	@echo ""

install-bin:
	@mkdir -p $(BINDIR)
	install -m 755 target/release/sdr $(BINDIR)/sdr-rs

install-icon:
	@mkdir -p $(ICONDIR)
	cp data/com.sdr.rs.svg $(ICONDIR)/com.sdr.rs.svg
	@for size in 48 64 128 256; do \
		mkdir -p $(DATADIR)/icons/hicolor/$${size}x$${size}/apps; \
		rsvg-convert -w $$size -h $$size data/com.sdr.rs.svg \
			-o $(DATADIR)/icons/hicolor/$${size}x$${size}/apps/com.sdr.rs.png 2>/dev/null || true; \
	done
	@gtk-update-icon-cache $(DATADIR)/icons/hicolor/ 2>/dev/null || true

install-desktop:
	@mkdir -p $(DESKTOPDIR)
	cp data/com.sdr.rs.desktop $(DESKTOPDIR)/com.sdr.rs.desktop
	@update-desktop-database $(DESKTOPDIR) 2>/dev/null || true

# ─────────────────────────────────────────────────────────────────────
# Uninstall
# ─────────────────────────────────────────────────────────────────────

uninstall:
	rm -f $(BINDIR)/sdr-rs
	rm -f $(ICONDIR)/com.sdr.rs.svg
	rm -f $(DESKTOPDIR)/com.sdr.rs.desktop
	@update-desktop-database $(DESKTOPDIR) 2>/dev/null || true
	@echo "SDR-RS uninstalled"

# ─────────────────────────────────────────────────────────────────────
# Quality
# ─────────────────────────────────────────────────────────────────────

test:
	$(CARGO) test --workspace

clippy:
	$(CARGO) clippy --all-targets --workspace -- -D warnings

fmt:
	$(CARGO) fmt --all

fmt-check:
	$(CARGO) fmt --all -- --check

deny:
	$(CARGO) deny check

audit:
	$(CARGO) audit

lint: fmt-check clippy test deny audit ffi-header-check

# ─────────────────────────────────────────────────────────────────────
# sdr-ffi header drift check
# ─────────────────────────────────────────────────────────────────────
#
# `include/sdr_core.h` is the **hand-written** source of truth for the
# C ABI. `cbindgen` is NOT used to generate it — the hand-written file
# can carry explanatory comments, section dividers, and
# human-friendly ordering that a generator would flatten.
#
# However, we still want a machine-checked safety net against drift
# between the Rust source (`crates/sdr-ffi/src/`) and the header.
# `make ffi-header-check` runs cbindgen in check mode against the
# Rust sources and diffs the generated signatures against the
# hand-written header. The check is signature-only — it ignores
# comments and formatting, so the human-friendly structure of the
# hand-written header doesn't break the lint.
#
# cbindgen is an optional developer tool, installed via:
#   cargo install cbindgen
#
# If cbindgen is not available, the target prints a skip warning
# and exits 0. CI installs cbindgen explicitly so the check is
# meaningful there.

CBINDGEN ?= cbindgen
FFI_HEADER := include/sdr_core.h
FFI_GENERATED := target/sdr_core.h.generated

ffi-header-check:
	@if ! command -v $(CBINDGEN) >/dev/null 2>&1; then \
		echo "cbindgen not installed — skipping ffi-header-check"; \
		echo "(install with 'cargo install cbindgen' to enable)"; \
	else \
		echo "==> cbindgen sdr-ffi → $(FFI_GENERATED)"; \
		mkdir -p $(dir $(FFI_GENERATED)); \
		$(CBINDGEN) --config crates/sdr-ffi/cbindgen.toml \
			--crate sdr-ffi \
			--output $(FFI_GENERATED) 2>&1 | \
			grep -v '^WARN:' || true; \
		echo "==> diff $(FFI_HEADER) vs $(FFI_GENERATED) (signature-only)"; \
		./scripts/ffi-header-diff.sh $(FFI_HEADER) $(FFI_GENERATED); \
	fi

# Regenerate the hand-written header from cbindgen output. This is a
# **manual** starting point for writing a new hand-written header,
# not a build step — you'd run this once when adding a new batch of
# FFI functions, then hand-edit the output into the real header.
ffi-header-regen:
	@if ! command -v $(CBINDGEN) >/dev/null 2>&1; then \
		echo "cbindgen not installed — install with 'cargo install cbindgen'"; \
		exit 1; \
	fi
	@mkdir -p $(dir $(FFI_GENERATED))
	@$(CBINDGEN) --config crates/sdr-ffi/cbindgen.toml \
		--crate sdr-ffi \
		--output $(FFI_GENERATED)
	@echo "Regenerated → $(FFI_GENERATED)"
	@echo "(Copy signatures by hand into $(FFI_HEADER); do not commit $(FFI_GENERATED).)"

# ─────────────────────────────────────────────────────────────────────
# SonarQube
# ─────────────────────────────────────────────────────────────────────

scan:
	@if [ -f .env ]; then \
		SONAR_APP_TOKEN=$$(sed -n 's/^SONAR_APP_TOKEN=//p' .env | head -n 1) && \
		SONAR_TOKEN=$$SONAR_APP_TOKEN /opt/sonar-scanner/bin/sonar-scanner \
			-Dsonar.host.url=https://sonar.aaru.network \
			-Dsonar.scanner.truststorePath=/tmp/sonar-truststore.jks \
			-Dsonar.scanner.truststorePassword=changeit; \
	else \
		echo "No .env file found. Create one with SONAR_APP_TOKEN=<token>"; \
	fi

clean:
	$(CARGO) clean
