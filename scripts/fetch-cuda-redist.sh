#!/usr/bin/env bash
# Fetch NVIDIA CUDA 12 + cuDNN 9 runtime libraries for sherpa-cuda builds.
#
# Downloads a minimal set of NVIDIA redistributable tarballs from the
# developer.download.nvidia.com server, verifies SHA-256, and extracts
# just the runtime .so files we need into a staging directory. The
# caller (`make fetch-cuda-redist`) then copies the staged libs into
# the install prefix's `sdr-rs-libs/` subdirectory.
#
# This dance is necessary because the sherpa-onnx project ships a
# pre-patched onnxruntime 1.23.2 binary (see
# `sherpa-onnx/cmake/onnxruntime-linux-x86_64-gpu.cmake`) that is
# hard-linked against CUDA 12.x + cuDNN 9.x. CUDA major versions are
# NOT ABI-compatible, so hosts that have newer CUDA (e.g. Arch Linux,
# which ships CUDA 13 by default) can't run sherpa-cuda without these
# libraries provided separately. Rather than require users to install
# a parallel CUDA 12 toolkit, we sideload the exact runtime libs that
# `libonnxruntime_providers_cuda.so` needs.
#
# Selected components are determined by inspecting the NEEDED entries
# of the provider .so:
#
#     $ readelf -d libonnxruntime_providers_cuda.so | grep NEEDED
#       libcublasLt.so.12   <- libcublas archive
#       libcublas.so.12     <- libcublas archive (same file, two sonames)
#       libcurand.so.10     <- libcurand archive
#       libcufft.so.11      <- libcufft archive
#       libcudart.so.12     <- cuda_cudart archive
#       libcudnn.so.9       <- cudnn archive (plus cuDNN 9's dlopen'd sublibs)
#
# Components NOT fetched (not in NEEDED, not used by the sherpa path):
#
#     libcusparse, libcusolver, libnvrtc, libnvjitlink, libnvinfer (TensorRT)
#
# Static libraries inside each archive are filtered out during extract
# to avoid shipping ~1 GB of .a files we never link against.
#
# Usage:
#     scripts/fetch-cuda-redist.sh <downloads-dir> <staging-dir> <sentinel-path>
#
# The script is idempotent: if the sentinel file exists, it exits 0
# immediately. To force a re-download, delete the sentinel (or the
# entire cache directory).

set -euo pipefail

if [ "$#" -ne 3 ]; then
    echo "usage: $0 <downloads-dir> <staging-dir> <sentinel-path>" >&2
    exit 2
fi

DOWNLOADS_DIR="$1"
STAGING_DIR="$2"
SENTINEL="$3"

# The sentinel file alone isn't enough to short-circuit — if the
# staging directory got wiped (e.g. the user ran `rm -rf ~/.cache/sdr-rs`
# selectively, or a distro upgrade pruned XDG caches) while the
# sentinel survived, we'd skip the extract step and leave install-
# cuda-redist-libs with nothing to copy. Validate that staging still
# has at least one library file before trusting the cache; otherwise
# fall through to the fetch/extract path, which itself is idempotent
# on the already-downloaded archives.
if [ -f "$SENTINEL" ] \
    && [ -d "$STAGING_DIR" ] \
    && [ -n "$(find "$STAGING_DIR" -maxdepth 1 \( -type f -o -type l \) -name 'lib*.so*' -print -quit 2>/dev/null)" ]; then
    echo "  [cached] NVIDIA CUDA redist libs already fetched and staged"
    exit 0
fi

CUDA_BASE="https://developer.download.nvidia.com/compute/cuda/redist"
CUDNN_BASE="https://developer.download.nvidia.com/compute/cudnn/redist"

# url sha256 — one pair per line
# shellcheck disable=SC2034
ARCHIVES=(
    "${CUDA_BASE}/cuda_cudart/linux-x86_64/cuda_cudart-linux-x86_64-12.6.77-archive.tar.xz"
    "f74689258a60fd9c5bdfa7679458527a55e22442691ba678dcfaeffbf4391ef9"

    "${CUDA_BASE}/libcublas/linux-x86_64/libcublas-linux-x86_64-12.6.4.1-archive.tar.xz"
    "ec682bac6387f9cdfd0c20b25a16cd6ed0b8b3b7ff42be9eaeb41828e3a72572"

    "${CUDA_BASE}/libcufft/linux-x86_64/libcufft-linux-x86_64-11.3.0.4-archive.tar.xz"
    "63a046d51a45388e10612c3fd423bb7fa5127496aa9bb3951a609e8b9d996852"

    "${CUDA_BASE}/libcurand/linux-x86_64/libcurand-linux-x86_64-10.3.7.77-archive.tar.xz"
    "981339cc86d7b8779e9a3c17e72d8c5e1a8a2d06c24db692eecabed8e746a3c7"

    "${CUDNN_BASE}/cudnn/linux-x86_64/cudnn-linux-x86_64-9.5.1.17_cuda12-archive.tar.xz"
    "35dd20b9c68324ae1288ac36f66ab1f318d2bfecfafb703a82617aa283272be4"
)

echo ""
echo "Fetching NVIDIA CUDA 12 + cuDNN 9 runtime libs for sherpa-cuda"
echo "  Downloads: $DOWNLOADS_DIR"
echo "  Staging:   $STAGING_DIR"
echo "  Size:      ~1.83 GB download, ~1.2 GB staged"
echo ""

mkdir -p "$DOWNLOADS_DIR"

# Walk pairs (url, sha) from the array
i=0
while [ "$i" -lt "${#ARCHIVES[@]}" ]; do
    url="${ARCHIVES[$i]}"
    sha="${ARCHIVES[$((i + 1))]}"
    name="$(basename "$url")"
    dest="$DOWNLOADS_DIR/$name"

    if [ -f "$dest" ]; then
        echo "  [cached] $name"
    else
        echo "  [fetch ] $name"
        curl -fL --progress-bar "$url" -o "$dest.part"
        mv "$dest.part" "$dest"
    fi

    # Verify checksum (stdin format: "<sha>  <path>")
    if ! echo "$sha  $dest" | sha256sum -c --status -; then
        echo "  sha256 mismatch for $name; discarding and aborting" >&2
        rm -f "$dest"
        exit 1
    fi

    i=$((i + 2))
done

echo ""
echo "Extracting runtime .so files to $STAGING_DIR"
rm -rf "$STAGING_DIR"
mkdir -p "$STAGING_DIR"

# Per-archive extract. A few subtleties matter here:
#
#   1. Each NVIDIA redist archive contains a `lib/` tree AND a
#      `lib/stubs/` subdirectory. The stubs are build-time linker
#      shims (e.g. `lib/stubs/libcuda.so` is a shim for the real
#      NVIDIA driver library that lives in /usr/lib on installed
#      systems). Shipping these as runtime libs is dangerous — the
#      libcuda stub in particular would shadow the real driver — so
#      we `-prune` the stubs/ subtree out of the find traversal.
#
#   2. The runtime .so files in lib/ form a symlink chain like
#        libfoo.so         -> libfoo.so.N
#        libfoo.so.N       -> libfoo.so.N.M.P
#        libfoo.so.N.M.P   (real file)
#      The ELF loader resolves NEEDED entries against the soname
#      (`libfoo.so.N`), which is the middle link. If we flatten the
#      chain to just the real file we lose the soname entry and the
#      loader fails. So we preserve symlinks as symlinks via `cp -a`,
#      and match both regular files and symlinks with `find \( -type
#      f -o -type l \)`.
#
#   3. We filter out:
#        - static libraries (*.a, *_static.so*) — we link dynamically
#        - libnvblas, libcufftw — compat wrappers we don't use
#        - unversioned `lib*.so` at the top of the symlink chain IS
#          kept, because stripping it breaks nothing and the size
#          cost is just the symlink node itself
for archive in "$DOWNLOADS_DIR"/*.tar.xz; do
    tmp="$(mktemp -d)"
    trap 'rm -rf "$tmp"' EXIT
    tar -xf "$archive" -C "$tmp"

    find "$tmp" \
        -type d -name stubs -prune -o \
        \( -type f -o -type l \) \
        \( -name 'lib*.so' -o -name 'lib*.so.*' \) \
        ! -name '*_static*' \
        ! -name 'libnvblas*' \
        ! -name 'libcufftw*' \
        -print0 \
        | xargs -0 --no-run-if-empty cp -a -t "$STAGING_DIR/"

    rm -rf "$tmp"
    trap - EXIT
done

count="$(find "$STAGING_DIR" -maxdepth 1 \( -type f -o -type l \) -name 'lib*.so*' | wc -l)"
echo "  staged $count library files"

# Atomic-ish sentinel: only touched when everything above succeeded,
# so a partially-completed run leaves the next invocation to retry
# from the beginning (downloads already present will hit the cache).
touch "$SENTINEL"
echo ""
