# Sherpa CUDA: NVIDIA Runtime Sideload

> **tl;dr** — `make install CARGO_FLAGS="... --features sherpa-cuda"` downloads
> a ~1.83 GB set of NVIDIA CUDA 12 / cuDNN 9 runtime libraries from the NVIDIA
> developer redist server, verifies them by SHA-256, and installs them into
> `$(BINDIR)/sdr-rs-libs/` alongside the binary (default
> `~/.cargo/bin/sdr-rs-libs/`; override via `make install BINDIR=<path>`).
> Your system CUDA install (if any) is untouched. You only need a working
> NVIDIA kernel driver on the host. This doc explains why — and how we get
> out of the sideload once upstream catches up.

This document is the deep-dive companion to the short `Sherpa CUDA notes`
section in the [top-level README](../README.md). Read that first for the
user-visible usage; read this when you want to understand the rationale or
are maintaining the install flow.

Tracked by issue [#267](https://github.com/jasonherald/rtl-sdr/issues/267).

---

## The problem

`sherpa-onnx`'s prebuilt CUDA variant is hard-pinned to an internal
`onnxruntime` 1.23.2 build — see
`sherpa-onnx/cmake/onnxruntime-linux-x86_64-gpu.cmake:22`, which fetches
`onnxruntime-linux-x64-gpu-1.23.2-patched.zip` from csukuangfj's
[onnxruntime-libs](https://github.com/csukuangfj/onnxruntime-libs) repo. That
prebuilt is compiled against **CUDA 12.x + cuDNN 9.x**.

CUDA major versions are **not ABI-compatible**. `libcublasLt.so.12` and
`libcublasLt.so.13` share a name root but have incompatible interfaces. A
binary linked against CUDA 12 cannot run against CUDA 13 libraries and
vice versa.

Arch Linux's `cuda` package is at **13.2.0** (as of April 2026). cuDNN is
not in the main repo at all. So on a current Arch install, a naive
`sherpa-cuda` binary fails at recognizer creation with:

```text
Failed to load library libonnxruntime_providers_cuda.so with error:
libcublasLt.so.12: cannot open shared object file
```

The same binary worked fine on Ubuntu 24.04 (still on CUDA 12), so this
isn't a project bug — it's a bleeding-edge-distro bug.

## Options considered

1. **Require users to install CUDA 12 from AUR + cuDNN 9 from an
   out-of-repo source.** Rejected — multi-package AUR dependency, AUR
   packages are community-maintained and occasionally break, and
   "works only if you happen to be on Arch and configured these five
   out-of-repo deps" is a terrible out-of-box experience.
2. **Fork `onnxruntime` and rebuild with CUDA 13 support.** Rejected —
   `onnxruntime` 1.23.2 predates CUDA 13 support upstream, `csukuangfj`
   ships a patched binary we'd have to diff and port, every `sherpa-onnx`
   release bumps the ORT pin, and the result would be a permanent
   maintenance burden that nobody else is going to take over.
3. **Downgrade Arch's `cuda` package to 12.x.** Rejected — breaks
   unrelated tools that depend on CUDA 13, and the same workstation
   daily-drives `whisper-cuda` against CUDA 13 already (that works
   because `whisper-rs` compiles its own kernels at build time rather
   than linking a precompiled runtime).
4. **Sideload the NVIDIA CUDA 12 runtime libs next to our binary.**
   ✅ Chosen.

Option 4 has the nice property that NVIDIA *specifically publishes a
developer redist* containing exactly the runtime libraries we need, in
signed tarballs, with a stable URL. We're not scraping; we're using the
distribution channel NVIDIA built for this purpose.

## What the install flow actually does

When `make install CARGO_FLAGS="... --features sherpa-cuda"` runs:

1. **Makefile detects `sherpa-cuda`** in `CARGO_FLAGS` via `findstring`
   and adds `fetch-cuda-redist` as a prerequisite of
   `install-cuda-redist-libs`.
2. **`scripts/fetch-cuda-redist.sh`** downloads the minimum set of NVIDIA
   tarballs from <https://developer.download.nvidia.com/compute/cuda/redist/>
   (plus cuDNN from `compute/cudnn/redist/`):

   | Package | Version | Size |
   |---|---|---|
   | `cuda_cudart-linux-x86_64` | 12.6.77 | ~1 MB |
   | `libcublas-linux-x86_64` | 12.6.4.1 | ~522 MB |
   | `libcufft-linux-x86_64` | 11.3.0.4 | ~476 MB |
   | `libcurand-linux-x86_64` | 10.3.7.77 | ~82 MB |
   | `cudnn-linux-x86_64` | 9.5.1.17 (cuda12) | ~745 MB |

3. **SHA-256 verification** on each file. Versions and hashes are
   hardcoded in the script — future version bumps require updating both
   together.
4. **Selective extraction** into a staging directory:
   - `stubs/` subdirectories are pruned (they contain build-time-only
     driver shims; shipping them could shadow the real NVIDIA driver at
     runtime, which is obviously bad).
   - `*_static*` files are skipped (we link dynamically).
   - `libnvblas*` and `libcufftw*` are skipped (compat wrappers we don't
     use).
   - Symlink chains (`libfoo.so → libfoo.so.N → libfoo.so.N.M.P`) are
     preserved via `cp -a` — the loader resolves `NEEDED` entries
     against the middle soname, so flattening breaks the lookup.
5. **Staged libs are copied** via `cp -a` into `$(BINDIR)/sdr-rs-libs/`
   alongside the installed binary.
6. **The binary's `DT_RPATH` is `$ORIGIN:$ORIGIN/sdr-rs-libs`** — set
   in the root `build.rs` and forced to old-style `DT_RPATH` (not
   the modern-linker-default `DT_RUNPATH`) via
   `-Wl,--disable-new-dtags`. This is load-bearing for the specific
   way `onnxruntime` resolves its provider stack: it calls
   `dlopen("libonnxruntime_providers_cuda.so")` at recognizer
   creation, and the provider library's own
   `NEEDED libcublasLt.so.12` search has to reach our sideloaded
   libs in `$ORIGIN/sdr-rs-libs`. In this load path the executable's
   `DT_RPATH` is the mechanism that reliably resolves those
   transitive dependencies — the glibc loader inherits the
   executable's `DT_RPATH` into the dependency lookups of
   `dlopen`'d libraries, whereas `DT_RUNPATH` (what modern linkers
   default to) is treated differently and did not resolve the
   provider's CUDA deps in our testing. The `--disable-new-dtags`
   flag is the entire reason the sideload works for this
   deployment model.

## What we ship and why

The set of runtime libs is the **exact `NEEDED` list** of
`libonnxruntime_providers_cuda.so`:

```console
$ readelf -d libonnxruntime_providers_cuda.so | grep NEEDED
  libcublasLt.so.12   <- libcublas archive
  libcublas.so.12     <- libcublas archive (same file, two sonames)
  libcurand.so.10     <- libcurand archive
  libcufft.so.11      <- libcufft archive
  libcudart.so.12     <- cuda_cudart archive
  libcudnn.so.9       <- cudnn archive (plus sublibs dlopen'd by cuDNN 9)
```

We do **not** ship:

- `libcusparse`, `libcusolver`, `libnvrtc`, `libnvjitlink` — not in
  `NEEDED`, `onnxruntime`'s CUDA path doesn't touch them.
- `libonnxruntime_providers_tensorrt.so` — we never request the TensorRT
  provider, and it would pull `libnvinfer` which we don't provision.
- `libcuda.so` from the stubs directory — that's a build-time driver
  stub; at runtime it must come from the installed NVIDIA kernel driver
  package on the host.
- Static libraries (~1 GB of `.a` files in the tarballs).

## Disk cost

Three distinct locations hold CUDA sideload bytes — they're not
additive in the sense of "total RAM use" but each one shows up
independently on `df`:

1. **Download cache — ~1.83 GB** at
   `$HOME/.cache/sdr-rs/cuda-redist/downloads/`. Raw NVIDIA tarballs
   (`.tar.xz`), pre-extraction. Populated once on first build,
   survives `cargo clean` and branch switches.
2. **Staging cache — ~2.1 GB** at
   `$HOME/.cache/sdr-rs/cuda-redist/staging/`. Extracted `.so` files
   and symlink chains, ready to be copied verbatim into the install
   location. Populated once per CUDA redist version bump (gated by
   `$HOME/.cache/sdr-rs/cuda-redist/.sentinel-v1`).
3. **Installed runtime libs — ~2.1 GB** at `$(BINDIR)/sdr-rs-libs/`.
   A direct `cp -a` of the staging cache. This is what the installed
   binary's `DT_RPATH` actually loads from at runtime.

Without symlink preservation each of staging and install would bloat
to **5.6 GB** because cuDNN and cuBLAS ship three copies of their
content for the
`libfoo.so → libfoo.so.N → libfoo.so.N.M.P` chain. `cp -a` preserves
the symlinks so the content lives once and the sonames point at it.

**Total first-run cost: ~6 GB of disk** across download + staging +
install. **Subsequent installs: ~2.1 GB** (only the install location
gets rewritten; download + staging are sentinel-gated).

`make uninstall` removes the install location (`$(BINDIR)/sdr-rs-libs/`)
but intentionally preserves `$HOME/.cache/sdr-rs/cuda-redist/` so the
next install is instant. The uninstall target prints instructions for
deleting the cache manually if you want to reclaim those ~3.9 GB.

## Pre-populating the cache

If you're on a slow or metered connection and want to see the download
progress in isolation (rather than buried inside `make install`'s
output), run:

```bash
make fetch-cuda-redist
```

This downloads and verifies the tarballs into the persistent cache
without doing the build or install step. A subsequent
`make install CARGO_FLAGS="... --features sherpa-cuda"` will then reuse
the cache and skip the download entirely.

## Re-unification plan

This sideload pattern exists to paper over three independent upstream
constraints. The exit ramp is cleanest when any one of them resolves:

1. **k2-fsa ships `sherpa-onnx` CUDA 13 prebuilts.** Then
   `scripts/fetch-cuda-redist.sh` becomes unnecessary and we delete it.
   As of April 2026 (`sherpa-onnx` v1.12.38) they only ship CUDA 12.x
   builds; we watch the
   [k2-fsa/sherpa-onnx releases](https://github.com/k2-fsa/sherpa-onnx/releases)
   page for a CUDA 13 tag.
2. **`csukuangfj` bumps `onnxruntime` to a version with native CUDA 13
   support.** This is harder because `microsoft/onnxruntime` itself has
   to ship CUDA 13 first, and that hadn't landed in a released ORT
   version as of April 2026.
3. **Arch downgrades `cuda` back to 12.x.** Won't happen — Arch is
   bleeding-edge by policy.

The most likely path is (1), which is a matter of k2-fsa's build
pipeline adding a CUDA 13 target. Until then, the sideload is the right
answer.

## Verified working on

- Arch Linux, CUDA 13.2.0 system install, NVIDIA 560+ driver,
  RTX 4080 Super.
- All three sherpa model families confirmed loading on GPU: Streaming
  Zipformer EN, Moonshine Tiny/Base, Parakeet-TDT 0.6b v3. (Note:
  Moonshine no-text issue is tracked separately in
  [#281](https://github.com/jasonherald/rtl-sdr/issues/281) and is a
  model-family issue, not a CUDA sideload issue.)
- VRAM allocation visible via `nvidia-smi` during Parakeet decode.

## Related files

- `Makefile` — `install-cuda-redist-libs`, `fetch-cuda-redist` targets
- `scripts/fetch-cuda-redist.sh` — download / SHA verification /
  extraction / stubs pruning
- `build.rs` — `DT_RPATH=$ORIGIN:$ORIGIN/sdr-rs-libs` +
  `-Wl,--disable-new-dtags`
- `crates/sdr-transcription/Cargo.toml` — `sherpa-cuda` feature gate
