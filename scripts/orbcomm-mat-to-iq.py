#!/usr/bin/env python3
"""Convert an ORBCOMM-receiver .mat capture to raw interleaved f32 IQ.

The reference receiver (https://github.com/fbieberly/ORBCOMM-receiver) ships two
off-air RTL-SDR captures as MATLAB v5 files. This converts one of them into the
pair of files `crates/sdr-orbcomm/tests/real_capture.rs` expects:

  <out>.iq    complex64 samples as little-endian f32 pairs (re, im, re, im, ...)
  <out>.json  the capture's metadata, so the test can build the ChannelBank
              with the recording's own centre frequency and sample rate

Usage:
    uv run --with scipy --with numpy scripts/orbcomm-mat-to-iq.py \
        original/ORBCOMM-receiver/data/1552071892p6.mat \
        /tmp/orbcomm-fixtures/1552071892p6

    ORBCOMM_IQ_FIXTURE=/tmp/orbcomm-fixtures/1552071892p6 \
        cargo test -p sdr-orbcomm --test real_capture -- --ignored --nocapture

Dev-side tool: stdlib + numpy + scipy only, never built or run by CI.

.mat keys (verified against both shipped captures, and documented in the
reference project's README):
    samples    complex64, shape (1, N)   the IQ record
    fc, fs     float64,   shape (1, 1)   centre frequency and sample rate, Hz
    timestamp  float64,   shape (1, 1)   Unix time of the first sample
    sats       <U..,      shape (K,)     satellite names overhead
    tles       <U..,      shape (1, 3)   the TLE lines for sats[0]
    lat, lon   float64,   shape (1, 1)   receiver position, degrees
    alt        int64,     shape (1, 1)   receiver elevation, metres

Samples are scaled by 1 / median(|s|) — the same normalisation
`file_decoder.py` applies — so the two captures, whose raw magnitudes differ by
about 2x, land on a common scale. Every stage of the Rust chain is
amplitude-independent (the FLL takes an argument, the Gardner detector
normalises by a tracked power estimate), so this is cosmetic; it just keeps the
f32 fixture comfortably away from both ends of the exponent range.
"""

import json
import sys

import numpy as np
from scipy.io import loadmat


def flat(mat, key):
    """First element of a (1, 1) / (1,) MATLAB array."""
    return mat[key].flatten()[0]


def main(argv):
    if len(argv) != 3:
        print(f"usage: {argv[0]} <capture.mat> <output-prefix>", file=sys.stderr)
        return 2
    mat_path, out_prefix = argv[1], argv[2]

    mat = loadmat(mat_path)
    samples = mat["samples"].flatten().astype(np.complex64)
    scale = float(np.median(np.abs(samples)))
    if scale > 0.0:
        samples = (samples / scale).astype(np.complex64)

    meta = {
        "source": mat_path,
        "center_hz": float(flat(mat, "fc")),
        "sample_rate": float(flat(mat, "fs")),
        "timestamp": float(flat(mat, "timestamp")),
        "sample_count": int(samples.size),
        "median_abs": scale,
        "sats": [str(s).strip() for s in mat["sats"].flatten()],
        "tles": [str(t).strip() for t in mat["tles"].flatten()],
        "rx_lat_deg": float(flat(mat, "lat")),
        "rx_lon_deg": float(flat(mat, "lon")),
        "rx_alt_m": float(flat(mat, "alt")),
    }

    # complex64 viewed as f32 is already interleaved (re, im) in memory order.
    samples.view(np.float32).tofile(out_prefix + ".iq")
    with open(out_prefix + ".json", "w", encoding="utf-8") as f:
        json.dump(meta, f, indent=2, sort_keys=True)

    print(
        f"{meta['sample_count']} samples @ {meta['sample_rate'] / 1e6:.4f} Msps, "
        f"center {meta['center_hz'] / 1e6:.4f} MHz, "
        f"{meta['sample_count'] / meta['sample_rate']:.2f} s, "
        f"sats {meta['sats']}"
    )
    print(f"wrote {out_prefix}.iq and {out_prefix}.json")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
