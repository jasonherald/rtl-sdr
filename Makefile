# SDR-RS — Software-defined radio application
# Makefile for building, installing, and managing

BINDIR      ?= $(HOME)/.cargo/bin
LIBDIR      ?= $(BINDIR)/sdr-rs-libs
DATADIR     ?= $(HOME)/.local/share
ICONDIR     ?= $(DATADIR)/icons/hicolor/scalable/apps
DESKTOPDIR  ?= $(DATADIR)/applications
CARGO       ?= cargo
CARGO_FLAGS ?= --release

.PHONY: all build install install-bin install-sherpa-runtime-libs \
        check-cuda-system-libs install-icon install-desktop uninstall \
        test clippy fmt fmt-check \
        lint deny audit scan clean help

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
# `--all-features` can never produce an installable binary here — it
# enables whisper AND sherpa together (rejected by the transcription
# feature mutex) and static+shared+cuda link modes together (rejected
# by sherpa-onnx-sys's build script) — but the cargo failure is
# cryptic, so fail fast with the real reason. Per CR round 3 on
# PR #859.
# This Makefile installs into $(HOME)/.cargo/bin — a per-user layout
# where root execution is never right: `sudo make install` would
# install into /root, miss the user's running sdr-rs during the
# stop-detection, and relaunch the app as root without the session's
# Wayland/PipeWire sockets. Refuse up front. Per Codacy round 1 on
# PR #862.
ifeq ($(shell id -u),0)
$(error this Makefile installs per-user into $$HOME/.cargo/bin — run it WITHOUT sudo)
endif

ifneq (,$(findstring --all-features,$(CARGO_FLAGS)))
$(error --all-features is not buildable: transcription backends and sherpa link modes are mutually exclusive cargo features — pick exactly one, e.g. CARGO_FLAGS="--release --features whisper-cuda")
endif

INSTALL_RUNTIME_LIB_TARGETS :=
ifneq (,$(findstring sherpa-cuda,$(CARGO_FLAGS)))
INSTALL_RUNTIME_LIB_TARGETS += check-cuda-system-libs install-sherpa-runtime-libs
# Preflight BEFORE compiling, not just before the lib copy — a
# missing package should fail in milliseconds with the pacman hint,
# not after (or midway through) a full cargo build. Per CR round 1
# on PR #859.
build: check-cuda-system-libs
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
	@echo "  make check-cuda-system-libs  Verify the CUDA 13 + cuDNN 9 system packages"
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

# The copy targets depend on `build` explicitly so `make -j install`
# cannot race them against cargo writing target/release — prerequisite
# ORDER in the install list alone doesn't serialize under -j. Per CR
# round 2 on PR #859.
install-bin: build
install-sherpa-runtime-libs: build

# Never install over a RUNNING sdr-rs: replacing the executable (and
# its adjacent sdr-rs-libs) under a live process leaves the dynamic
# loader holding stale state, and the next dlopen — e.g. PipeWire
# lazily loading an SPA plugin on an audio-route change — SIGSEGVs
# inside ld-linux (observed 2026-08-29: bias-T toggle after a
# mid-run install; the core's executable line read "(deleted)").
# The stop runs INSIDE install-bin's recipe (after its `build`
# prerequisite), so even `make -j install` keeps the app running
# through the long compile — it is only down for the copy window.
# Graceful SIGTERM so GTK runs its shutdown path (recordings
# finalized, config flushed), bounded wait, hard error if it will
# not exit. This sentinel records the stop so `install`'s final
# recipe relaunches the app; install-bin clears any stale copy from
# an aborted earlier run before detection.
# Under the user's own cache dir, not /tmp — a predictable /tmp path
# could be pre-created by another local user (CWE-377), and the
# sticky bit would then block our rm. Per CR round 3 on PR #862.
RESTART_SENTINEL := $(HOME)/.cache/sdr-rs/.restart-sentinel

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
	@if [ -f $(RESTART_SENTINEL) ]; then \
		rm -f $(RESTART_SENTINEL); \
		echo "  relaunching sdr-rs"; \
		setsid -f $(BINDIR)/sdr-rs >/dev/null 2>&1 || true; \
	fi
	@echo ""

install-bin:
	@# Sentinel triage (CR round 2 on PR #862): a sentinel with the
	@# app RUNNING is stale (user relaunched manually after an
	@# aborted install) — clear it so this run doesn't relaunch
	@# unexpectedly. A sentinel with the app NOT running means a
	@# previous install died after the stop — KEEP it, so this run's
	@# completion heals the situation by relaunching.
	@if [ -f $(RESTART_SENTINEL) ] && pgrep -u $$(id -u) -x sdr-rs >/dev/null 2>&1; then \
		rm -f $(RESTART_SENTINEL); \
	fi
	@# Stop a running sdr-rs INSIDE this recipe (which already
	@# depends on `build`) so `make -j install` cannot TERM the app
	@# while cargo is still compiling — the app is only down for the
	@# copy window. Graceful SIGTERM so GTK's shutdown path runs;
	@# hard error rather than installing over a live binary. UID
	@# scope (not session scope) is deliberate: any same-user
	@# instance runs THIS binary path and would crash on the
	@# replacement, so stopping them all is the correct radius.
	@if pgrep -u $$(id -u) -x sdr-rs >/dev/null 2>&1; then \
		echo "  sdr-rs is running — stopping it for the install (will relaunch)"; \
		mkdir -p $$(dirname $(RESTART_SENTINEL)); \
		touch $(RESTART_SENTINEL); \
		pkill -u $$(id -u) -x -TERM sdr-rs; \
		for i in $$(seq 1 50); do \
			pgrep -u $$(id -u) -x sdr-rs >/dev/null 2>&1 || break; \
			sleep 0.2; \
		done; \
		if pgrep -u $$(id -u) -x sdr-rs >/dev/null 2>&1; then \
			echo "error: sdr-rs did not exit within 10 s — close it and re-run make install"; \
			rm -f $(RESTART_SENTINEL); \
			exit 1; \
		fi; \
	fi
	@# The copy itself is the failure-guarded step, staged through a
	@# temp file + atomic rename so a failed copy can NEVER leave a
	@# truncated sdr-rs on disk — the old binary stays intact and is
	@# relaunched rather than leaving the user appless. Per CR
	@# round 3 on PR #862.
	@mkdir -p $(BINDIR)
	@if install -m 755 target/release/sdr $(BINDIR)/.sdr-rs.tmp \
		&& mv -f $(BINDIR)/.sdr-rs.tmp $(BINDIR)/sdr-rs; then \
		:; \
	else \
		rm -f $(BINDIR)/.sdr-rs.tmp; \
		if [ -f $(RESTART_SENTINEL) ]; then \
			rm -f $(RESTART_SENTINEL); \
			echo "  binary copy failed — relaunching the previous sdr-rs"; \
			setsid -f $(BINDIR)/sdr-rs >/dev/null 2>&1 || true; \
		fi; \
		exit 1; \
	fi

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
	@# Prune the retired CUDA-12 redist sideload (#855): upgrades from
	@# pre-system-package installs otherwise keep ~1.2 GB of dead
	@# libraries (and a stale libcudnn.so.9 that would shadow the
	@# system one via the binary's $$ORIGIN rpath ordering).
	@if [ -d $(LIBDIR) ]; then \
		rm -f $(LIBDIR)/libcudart.so* $(LIBDIR)/libcublas*.so* \
		      $(LIBDIR)/libcufft.so* $(LIBDIR)/libcurand.so* \
		      $(LIBDIR)/libnvrtc*.so* $(LIBDIR)/libcudnn*.so* \
		      $(LIBDIR)/libcudnn_*.so*; \
	fi
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

# Preflight for sherpa-cuda builds: the CUDA runtime now comes from
# system packages, not the retired redist sideload (#855 — k2-fsa
# publishes a CUDA 13 prebuilt and Arch packages cudnn, so the
# original reason for self-containment is gone; system packages also
# pave the way for a ROCm backend consumed the same way). Verify every
# NEEDED library of the prebuilt's CUDA provider resolves via the
# system loader BEFORE building, so a missing package fails with an
# actionable message instead of a runtime dlopen error.
#
# `libcuda.so.1` is the kernel-driver stub (nvidia-utils) and is
# checked last with its own hint.
check-cuda-system-libs:
	@missing=""; \
	for lib in libcudart.so.13 libcublas.so.13 libcublasLt.so.13 \
	           libcufft.so.12 libcurand.so.10 libnvrtc.so.13 \
	           libcudnn.so.9; do \
		ldconfig -p | grep -q "$$lib " || missing="$$missing $$lib"; \
	done; \
	if [ -n "$$missing" ]; then \
		echo "error: sherpa-cuda needs these system libraries:$$missing"; \
		echo "       install your distribution's CUDA 13 + cuDNN 9 packages"; \
		echo "       (Arch: pacman -S cuda cudnn)"; \
		exit 1; \
	fi; \
	ldconfig -p | grep -q "libcuda.so.1 " || { \
		echo "error: libcuda.so.1 not found — install your distribution's NVIDIA driver (Arch: pacman -S nvidia-utils)"; \
		exit 1; \
	}; \
	echo "  CUDA 13 + cuDNN 9 system libraries present"

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
	@# The redist cache was retired in #855; pre-#855 installs may
	@# still carry it, so point at the literal path once.
	@if [ -d $(HOME)/.cache/sdr-rs/cuda-redist ]; then \
		echo "  (legacy NVIDIA redist cache found — reclaim ~1.9 GB with:"; \
		echo "   rm -rf $(HOME)/.cache/sdr-rs/cuda-redist)"; \
	fi

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
