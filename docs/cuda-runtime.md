# Sherpa CUDA: System Runtime Packages

> **tl;dr** — a `sherpa-cuda` build resolves the CUDA runtime from your
> system packages. On Arch: `pacman -S cuda cudnn` (plus the NVIDIA
> driver you already have). `make install` verifies the libraries are
> present before building and fails with an actionable message if not.
> The former ~1.9 GB NVIDIA redist sideload is retired — this doc
> records what replaced it and why.

Tracked by issues
[#267](https://github.com/jasonherald/rtl-sdr/issues/267) (the original
sideload) and [#855](https://github.com/jasonherald/rtl-sdr/issues/855)
(the switch to system packages).

---

## What a sherpa-cuda build needs

The k2-fsa sherpa-onnx GPU prebuilt (v1.13.6, bundling onnxruntime
1.27.1) is compiled against **CUDA 13.x + cuDNN 9.x**. Its CUDA
provider's `NEEDED` list, verified by `readelf -d
libonnxruntime_providers_cuda.so` (as of sherpa-onnx v1.13.6, August
2026):

```console
libcublasLt.so.13   <- cuda (pacman)
libcublas.so.13     <- cuda
libcurand.so.10     <- cuda
libcufft.so.12      <- cuda
libcudart.so.13     <- cuda
libnvrtc.so.13      <- cuda
libcudnn.so.9       <- cudnn (pacman, plus its dlopen'd sublibs)
libcuda.so.1        <- nvidia-utils (the kernel-driver stub)
```

Arch's `cuda` package installs to `/opt/cuda` and registers
`/opt/cuda/lib64` with the dynamic loader via `/etc/ld.so.conf.d/`;
`cudnn` installs to `/usr/lib`. Both are plain `ldconfig` lookups at
runtime — no rpath tricks needed for the CUDA layer.

`make install CARGO_FLAGS="... --features sherpa-cuda"` runs the
`check-cuda-system-libs` preflight, which walks that exact list through
`ldconfig -p` and aborts with the `pacman -S` command line if anything
is missing.

The sherpa-onnx libraries themselves (`libsherpa-onnx-c-api.so`,
`libonnxruntime.so`, the providers) still ship alongside the binary in
`~/.cargo/bin/sdr-rs-libs/`, resolved via the binary's `DT_RPATH`
(`$ORIGIN:$ORIGIN/sdr-rs-libs`, forced old-style via
`-Wl,--disable-new-dtags` so the rpath cascades into onnxruntime's
`dlopen` of the CUDA provider — the one place deprecated ELF behavior
is exactly what we want).

## Why the sideload existed, and why it is gone

When sherpa-cuda first landed, two ecosystem gaps forced
self-containment (the full war story is in this file's git history):

1. k2-fsa's GPU prebuilt was **CUDA-12-only** while Arch shipped CUDA
   13 — and CUDA majors are not ABI-compatible.
2. **cuDNN was not packaged by Arch at all.**

So `make install` downloaded the exact CUDA 12 runtime set from
NVIDIA's redist server (SHA-256-pinned), staged it under
`~/.cache/sdr-rs/cuda-redist/`, and installed it next to the binary.
It worked, but it was ~1.9 GB of downloads, a script full of symlink
subtleties, and a parallel CUDA universe on disk.

Both gaps closed by August 2026: k2-fsa publishes a
`cuda-13.x-cudnn-9.x` prebuilt flavor, and Arch ships `extra/cudnn`.
With the reason-for-being gone, #855 retired the sideload in favor of
system packages — which also sets the pattern for a future ROCm
backend (ROCm is only sanely consumed as system packages, so all GPU
backends now resolve their runtimes the same way).

`install-sherpa-runtime-libs` prunes any leftover sideloaded CUDA
libraries from `sdr-rs-libs/` on upgrade, so pre-#855 installs shed
the dead ~1.2 GB automatically (and a stale sideloaded
`libcudnn.so.9` can never shadow the system one through the rpath).

## The trade we accepted

System packages couple the GPU path to Arch's rolling CUDA major:
when Arch moves to CUDA 14, `sherpa-cuda` breaks until k2-fsa ships a
CUDA 14 prebuilt (the preflight makes this a clear error, not a
mystery). That is the standard deal for every CUDA-using package on a
rolling distro, and the escape hatch — holding `cuda`/`cudnn` from the
Arch Linux Archive for a while — is well-trodden. The retired
sideload's design lives in git history if self-containment ever needs
to come back.
