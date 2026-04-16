# SDR-RS — Software-defined radio application
# Makefile for building, installing, and managing

BINDIR      ?= $(HOME)/.cargo/bin
LIBDIR      ?= $(BINDIR)/sdr-rs-libs
DATADIR     ?= $(HOME)/.local/share
ICONDIR     ?= $(DATADIR)/icons/hicolor/scalable/apps
DESKTOPDIR  ?= $(DATADIR)/applications
CARGO       ?= cargo
CARGO_FLAGS ?= --release

# Persistent cache for downloaded NVIDIA CUDA 12 redistributables
# (see the `fetch-cuda-redist` target). Sits outside the cargo target
# dir on purpose so `cargo clean` doesn't nuke ~1.8 GB of blobs.
CUDA_REDIST_CACHE     ?= $(HOME)/.cache/sdr-rs/cuda-redist
CUDA_REDIST_DOWNLOADS := $(CUDA_REDIST_CACHE)/downloads
CUDA_REDIST_STAGING   := $(CUDA_REDIST_CACHE)/staging
CUDA_REDIST_SENTINEL  := $(CUDA_REDIST_CACHE)/.sentinel-v1

.PHONY: all build install install-bin install-sherpa-runtime-libs \
        install-cuda-redist-libs install-icon install-desktop uninstall \
        fetch-cuda-redist test clippy fmt fmt-check \
        lint deny audit scan clean help \
        ffi-header-check ffi-header-regen swift-test mac-app mac-app-debug

# Runtime library copy targets are conditionally chained into `install`
# only when the user asked for a sherpa-cuda build. This is important
# because cargo does NOT clean `target/release/*.so*` or the persistent
# NVIDIA redist staging cache when switching feature sets — so if a
# user built with sherpa-cuda once, then later ran
#
#     make install CARGO_FLAGS="--release --features whisper-cuda"
#
# an unconditional copy step would happily repopulate $(LIBDIR) from
# the stale sherpa/CUDA artifacts left behind in target/release/ and
# ~/.cache/sdr-rs/cuda-redist/staging/, producing a whisper binary
# with a 2 GB subdirectory of dead CUDA libraries sitting next to it.
#
# `findstring` returns the matched substring on hit, empty on miss,
# so `ifneq (,...)` is "if the flag is present". Whisper and
# sherpa-cpu builds skip the runtime-lib plumbing entirely.
INSTALL_RUNTIME_LIB_TARGETS :=
ifneq (,$(findstring sherpa-cuda,$(CARGO_FLAGS)))
INSTALL_RUNTIME_LIB_TARGETS += install-sherpa-runtime-libs install-cuda-redist-libs
# Chain the fetch dep onto install-cuda-redist-libs (NOT onto `install`
# directly) so the fetch runs BEFORE the copy from staging into
# $(LIBDIR). Adding it to `install` would just append it to the
# existing prereq list and run the fetch after the copy — which is
# the bug that bit us the first time around.
install-cuda-redist-libs: fetch-cuda-redist
endif

# ─────────────────────────────────────────────────────────────────────
# Default
# ─────────────────────────────────────────────────────────────────────

all: build

help:
	@echo "SDR-RS — Software-defined radio application"
	@echo ""
	@echo "Usage:"
	@echo "  make install             Build release and install (binary + icon + desktop shortcut)"
	@echo "  make uninstall           Remove binary, icon, and desktop shortcut"
	@echo "  make build               Build release binary only"
	@echo "  make test                Run all workspace tests"
	@echo "  make lint                Run all checks (fmt, clippy, test, deny, audit)"
	@echo "  make scan                Run SonarQube scan"
	@echo "  make clean               Remove build artifacts"
	@echo "  make fetch-cuda-redist   Pre-populate the NVIDIA CUDA 12 redist cache"
	@echo "                           (only needed for sherpa-cuda builds; runs"
	@echo "                           transparently during 'make install' otherwise)"
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

install: build install-bin $(INSTALL_RUNTIME_LIB_TARGETS) install-icon install-desktop
	@echo ""
	@echo "SDR-RS installed successfully!"
	@echo "  Binary:   $(BINDIR)/sdr-rs"
	@if [ -d $(LIBDIR) ] && [ -n "$$(ls -A $(LIBDIR) 2>/dev/null)" ]; then \
		echo "  Libs:     $(LIBDIR)/"; \
	fi
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
# at build time, and the binary crate's build.rs injects an rpath of
# `$ORIGIN:$ORIGIN/sdr-rs-libs` so the loader finds them either in the
# cargo target/release layout (dev builds) or in the adjacent
# sdr-rs-libs/ subdirectory (installed builds).
#
# This target copies those .so files into $(LIBDIR). It's a no-op for
# static-linked builds (sherpa-cpu, whisper-*) because the glob matches
# nothing.
#
# `libonnxruntime_providers_tensorrt.so` is deliberately excluded — it
# needs libnvinfer/libnvonnxparser which we don't provision, and
# onnxruntime only ever dlopens it when a consumer asks for the
# TensorRT provider. sdr-rs only asks for "cuda", so the tensorrt
# provider is never loaded and shipping it would be dead weight.
install-sherpa-runtime-libs:
	@if ls target/release/libsherpa-onnx-c-api.so >/dev/null 2>&1; then \
		mkdir -p $(LIBDIR); \
		for so in target/release/libsherpa-onnx-c-api.so \
		          target/release/libsherpa-onnx-cxx-api.so \
		          target/release/libonnxruntime.so \
		          target/release/libonnxruntime_providers_cuda.so \
		          target/release/libonnxruntime_providers_shared.so; do \
			if [ -f "$$so" ] || [ -L "$$so" ]; then \
				cp -a "$$so" $(LIBDIR)/; \
				echo "  installed $$(basename $$so)"; \
			fi; \
		done; \
	fi

# Copy NVIDIA CUDA 12 runtime libs from the persistent cache into
# $(LIBDIR). The cache is populated by `fetch-cuda-redist`, which the
# `install` target pulls in automatically when `sherpa-cuda` is in
# CARGO_FLAGS. No-op for non-cuda builds because the staging dir
# doesn't exist.
#
# `cp -a` (not `install -m 644`!) is required here to preserve the
# symlink chain from the staging dir. The libraries form sonames like
# `libfoo.so -> libfoo.so.12 -> libfoo.so.12.6.4.1`; the loader looks
# up NEEDED entries against the middle link, and a plain `install`
# dereferences the symlinks and produces three identical full-size
# copies with different names, wasting gigabytes and breaking the
# soname resolution.
install-cuda-redist-libs:
	@if [ -d $(CUDA_REDIST_STAGING) ] && [ -n "$$(ls -A $(CUDA_REDIST_STAGING) 2>/dev/null)" ]; then \
		mkdir -p $(LIBDIR); \
		cp -a $(CUDA_REDIST_STAGING)/. $(LIBDIR)/; \
		echo "  installed $$(find $(CUDA_REDIST_STAGING) -maxdepth 1 \( -type f -o -type l \) -name 'lib*.so*' | wc -l) files from NVIDIA redist cache"; \
	fi

# Download and stage NVIDIA CUDA 12 + cuDNN 9 runtime libraries so
# that a `sherpa-cuda` build runs on hosts that do not have CUDA 12
# installed system-wide (notably Arch Linux, which ships CUDA 13).
# The actual download/verify/extract logic lives in
# `scripts/fetch-cuda-redist.sh` — see its header for the full
# rationale and the list of libraries we pull. A sentinel file at
# $(CUDA_REDIST_SENTINEL) short-circuits the target once the cache is
# fully populated, so subsequent `make install` runs are instant.
fetch-cuda-redist: $(CUDA_REDIST_SENTINEL)

$(CUDA_REDIST_SENTINEL):
	@./scripts/fetch-cuda-redist.sh \
	    $(CUDA_REDIST_DOWNLOADS) \
	    $(CUDA_REDIST_STAGING) \
	    $(CUDA_REDIST_SENTINEL)

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
	cp data/com.sdr.rs.splash.desktop $(DESKTOPDIR)/com.sdr.rs.splash.desktop
	@update-desktop-database $(DESKTOPDIR) 2>/dev/null || true

# ─────────────────────────────────────────────────────────────────────
# Uninstall
# ─────────────────────────────────────────────────────────────────────

uninstall:
	rm -f $(BINDIR)/sdr-rs
	rm -rf $(LIBDIR)
	rm -f $(ICONDIR)/com.sdr.rs.svg
	rm -f $(DESKTOPDIR)/com.sdr.rs.desktop
	rm -f $(DESKTOPDIR)/com.sdr.rs.splash.desktop
	@update-desktop-database $(DESKTOPDIR) 2>/dev/null || true
	@echo "SDR-RS uninstalled"
	@echo "  (NVIDIA redist cache at $(CUDA_REDIST_CACHE) preserved;"
	@echo "   remove manually with: rm -rf $(CUDA_REDIST_CACHE))"

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
		cbindgen_log=$$(mktemp); \
		if ! $(CBINDGEN) --config crates/sdr-ffi/cbindgen.toml \
			--crate sdr-ffi \
			--output $(FFI_GENERATED) >"$$cbindgen_log" 2>&1; then \
			cat "$$cbindgen_log"; \
			rm -f "$$cbindgen_log"; \
			echo "cbindgen failed — aborting ffi-header-check"; \
			exit 1; \
		fi; \
		grep -v '^WARN:' "$$cbindgen_log" || true; \
		rm -f "$$cbindgen_log"; \
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
# SwiftPM (SdrCoreKit) tests
# ─────────────────────────────────────────────────────────────────────
#
# `swift test` in `apps/macos/Packages/SdrCoreKit` needs
# `target/debug/libsdr_ffi.a` to exist before it can link. `make
# swift-test` does both in the right order: build the Rust static
# lib first, then invoke `swift test` with cwd set to the
# SdrCoreKit package directory.
#
# Only meaningful on macOS — Linux users don't have the
# Xcode/SwiftPM toolchain and the target would skip with a
# friendly message there.

SWIFT ?= swift
SDR_CORE_KIT := apps/macos/Packages/SdrCoreKit
SDR_MAC_APP  := apps/macos

# Build the SwiftUI macOS app as a dev `.app` bundle:
#   1. cargo build --workspace (produces libsdr_ffi.a with feature
#      unification — see swift-test comment for why --workspace)
#   2. swift build from apps/macos/ (the `sdr-rs` executable
#      product built from the SDRMac Swift target/module)
#   3. wrap the Mach-O binary into a minimal .app bundle with
#      Info.plist + entitlements + rasterized icon + ad-hoc codesign
#
# NOT the production shipping flow — that's M6 (Xcode project +
# Developer-ID signing + notarization). This target is purely for
# developer iteration so `open apps/macos/build/sdr-rs.app` shows
# the actual UI.
mac-app:
	@if [ "$$(uname -s)" != "Darwin" ]; then \
		echo "mac-app: skipping (not macOS)"; \
		exit 0; \
	fi
	@if ! command -v $(SWIFT) >/dev/null 2>&1; then \
		echo "mac-app: $(SWIFT) not found — install Xcode or Swift toolchain"; \
		exit 1; \
	fi
	@# Release is the default: debug builds of the Rust DSP chain
	@# can't keep up with 2 MSps on macOS (seen during M5 sub-PR 2
	@# testing — debug CPU overhead dropped USB throughput to ~45%
	@# and produced garbled audio). For live RTL-SDR work you
	@# always want release. Use `make mac-app-debug` only for
	@# non-streaming UI / lifecycle iteration.
	@echo "==> cargo build --workspace --release"
	@$(CARGO) build --workspace --release
	@# Delete the stale Mach-O so SwiftPM is forced to relink.
	@# SwiftPM's incremental builder caches the linked binary based
	@# on Swift source change detection and doesn't notice when the
	@# Rust static archive it links against has been rebuilt.
	@rm -f $(SDR_MAC_APP)/.build/release/sdr-rs
	@echo "==> swift build -c release (sdr-rs)"
	@cd $(SDR_MAC_APP) && $(SWIFT) build -c release
	@echo "==> bundle sdr-rs.app"
	@./$(SDR_MAC_APP)/scripts/bundle-mac-app.sh release

# Debug variant for iterating on non-streaming code paths. Will
# NOT keep up with live RTL-SDR streaming — don't use this for
# audio testing. See `mac-app` comment for why.
mac-app-debug:
	@if [ "$$(uname -s)" != "Darwin" ]; then \
		echo "mac-app-debug: skipping (not macOS)"; \
		exit 0; \
	fi
	@if ! command -v $(SWIFT) >/dev/null 2>&1; then \
		echo "mac-app-debug: $(SWIFT) not found — install Xcode or Swift toolchain"; \
		exit 1; \
	fi
	@echo "==> cargo build --workspace (debug)"
	@$(CARGO) build --workspace
	@rm -f $(SDR_MAC_APP)/.build/debug/sdr-rs
	@echo "==> swift build (sdr-rs, debug)"
	@cd $(SDR_MAC_APP) && $(SWIFT) build
	@echo "==> bundle sdr-rs.app"
	@./$(SDR_MAC_APP)/scripts/bundle-mac-app.sh debug

swift-test:
	@if [ "$$(uname -s)" != "Darwin" ]; then \
		echo "swift-test: skipping (not macOS)"; \
		exit 0; \
	fi
	@if ! command -v $(SWIFT) >/dev/null 2>&1; then \
		echo "swift-test: $(SWIFT) not found — install Xcode or Swift toolchain"; \
		exit 1; \
	fi
	@echo "==> cargo build --workspace (debug)"
	@# Build the whole workspace rather than `-p sdr-ffi` so feature
	@# unification picks up the transcription backend (`whisper-cpu`
	@# default) from the `sdr` binary. Building `-p sdr-ffi` in
	@# isolation would not forward any backend feature through
	@# sdr-core → sdr-transcription and trip the compile_error guard
	@# in sdr-transcription/src/lib.rs. The artifact we care about
	@# (`target/debug/libsdr_ffi.a`) is produced either way — the
	@# workspace build just happens to also compile the GTK UI,
	@# which we don't need but which is cheap once cached.
	@$(CARGO) build --workspace
	@echo "==> cd $(SDR_CORE_KIT) && swift test"
	@cd $(SDR_CORE_KIT) && $(SWIFT) test

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
