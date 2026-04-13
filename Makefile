# SDR-RS — Software-defined radio application
# Makefile for building, installing, and managing

BINDIR      ?= $(HOME)/.cargo/bin
DATADIR     ?= $(HOME)/.local/share
ICONDIR     ?= $(DATADIR)/icons/hicolor/scalable/apps
DESKTOPDIR  ?= $(DATADIR)/applications
CARGO       ?= cargo
CARGO_FLAGS ?= --release

.PHONY: all build install install-bin install-sherpa-runtime-libs \
        install-icon install-desktop uninstall test clippy fmt fmt-check \
        lint deny audit scan clean help

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

install: build install-bin install-sherpa-runtime-libs install-icon install-desktop
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

# When a sherpa-cuda build is active, sherpa-onnx is linked as a shared
# library (the CUDA prebuilt doesn't ship a static archive). The sys
# crate drops the runtime .so files next to the binary in target/release/
# at build time, and the binary crate's build.rs injects -rpath=$$ORIGIN
# so the dynamic loader finds them relative to the installed binary.
# This target copies those .so files into BINDIR alongside the binary;
# it's a no-op for static-linked builds (sherpa-cpu, whisper-*) because
# the glob matches nothing.
install-sherpa-runtime-libs:
	@mkdir -p $(BINDIR)
	@for so in target/release/libsherpa-onnx-c-api.so \
	           target/release/libsherpa-onnx-cxx-api.so \
	           target/release/libonnxruntime.so \
	           target/release/libonnxruntime_providers_cuda.so \
	           target/release/libonnxruntime_providers_shared.so \
	           target/release/libonnxruntime_providers_tensorrt.so; do \
		if [ -f "$$so" ]; then \
			install -m 644 "$$so" $(BINDIR)/; \
			echo "  installed $$(basename $$so)"; \
		fi; \
	done

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
	rm -f $(BINDIR)/libsherpa-onnx-c-api.so
	rm -f $(BINDIR)/libsherpa-onnx-cxx-api.so
	rm -f $(BINDIR)/libonnxruntime.so
	rm -f $(BINDIR)/libonnxruntime_providers_cuda.so
	rm -f $(BINDIR)/libonnxruntime_providers_shared.so
	rm -f $(BINDIR)/libonnxruntime_providers_tensorrt.so
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

lint: fmt-check clippy test deny audit

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
