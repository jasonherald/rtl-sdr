//
// SyntheticFftSource.swift — test-only FFT frame generator.
//
// Produces a moving pattern of sine "bumps" on top of noise so
// the spectrum / waterfall shaders have visible input without
// needing a real `SdrCore` instance running. Used by sub-PR 1
// and 2 to validate the Metal pipeline in isolation; sub-PR 3
// removes the reference here and swaps `SpectrumMTKView` to
// pull from `SdrCore.withLatestFftFrame` instead.
//
// Intentionally bare — no threading, no allocation after init,
// no dependencies on the engine. Tests and the renderer drive
// `next()` at whatever cadence they want.

import Foundation

/// A reusable buffer of synthetic FFT magnitudes in dB.
///
/// One instance per renderer; call `next()` each frame to
/// advance the pattern by one tick.
final class SyntheticFftSource {
    /// Current bin count. Must be a power of two to match what
    /// the real engine emits, but the synthetic side doesn't
    /// care — it scales the pattern to whatever size we ask.
    var binCount: Int

    /// Output buffer. Reused across calls; `next()` mutates it
    /// in place so callers can copy into a vertex buffer
    /// without a per-frame allocation.
    private(set) var magnitudes: [Float]

    /// Monotonic tick counter. Advances by 1 each `next()`.
    private var tick: UInt64 = 0

    /// Floor of the generated dB range. Keeps the noise floor
    /// below the visible range so peaks stand out.
    private let noiseFloorDb: Float = -100

    /// Peak height above the noise floor for a centered bump.
    private let peakHeightDb: Float = 60

    init(binCount: Int = 2048) {
        self.binCount = binCount
        self.magnitudes = [Float](repeating: 0, count: binCount)
    }

    /// Resize the internal buffer to a new bin count and reset
    /// the tick counter. The user changing FFT size shouldn't
    /// happen during a single frame; this is a cold-reconfig
    /// path only.
    func resize(to binCount: Int) {
        guard binCount != self.binCount, binCount > 0 else { return }
        self.binCount = binCount
        self.magnitudes = [Float](repeating: 0, count: binCount)
        self.tick = 0
    }

    /// Advance the pattern by one tick and write the new
    /// magnitudes into `magnitudes`. The result has:
    ///   - noise floor around -95 dB with ±2 dB jitter
    ///   - three moving peaks at different frequencies and
    ///     speeds so the waterfall (sub-PR 2) shows visible
    ///     vertical stripes at different angles
    func next() {
        tick &+= 1
        let t = Float(tick) * 0.03
        let n = Float(binCount)

        for i in 0..<binCount {
            let x = Float(i) / n  // 0..1 across bins

            // Cheap pseudo-noise — deterministic but
            // varied per-bin and per-tick.
            let noise = Float.pseudoNoise(seed: UInt64(i), tick: tick)

            // Three moving peaks. Centers drift at different
            // speeds; widths vary. The `expf(-...)` gives a
            // Gaussian-looking bump; scaling inside the exp
            // sets the visual width.
            let peak1 = gaussian(x: x, center: 0.5 + 0.2 * sin(t), width: 0.015)
            let peak2 = gaussian(x: x, center: 0.3 + 0.1 * sin(t * 1.7 + 1.0), width: 0.008)
            let peak3 = gaussian(x: x, center: 0.75 + 0.05 * sin(t * 2.3 + 2.0), width: 0.02)
            let peaksTotal = peak1 + peak2 * 0.7 + peak3 * 0.5

            magnitudes[i] = noiseFloorDb + 2.0 * noise + peakHeightDb * peaksTotal
        }
    }

    /// Gaussian-shaped peak. Returns 1.0 at the center and
    /// falls off with `width` (≈stddev in normalized units).
    private func gaussian(x: Float, center: Float, width: Float) -> Float {
        let d = (x - center) / max(width, 1e-4)
        return exp(-0.5 * d * d)
    }
}

private extension Float {
    /// Deterministic [-1, 1) pseudo-noise from (seed, tick).
    /// Not a quality PRNG — good enough to look like noise.
    static func pseudoNoise(seed: UInt64, tick: UInt64) -> Float {
        var h = seed &* 0x9E3779B97F4A7C15
        h ^= tick &* 0xBF58476D1CE4E5B9
        h ^= h >> 30
        h &*= 0x94D049BB133111EB
        h ^= h >> 27
        // Map 32 bits of hash to [-1, 1)
        let u32 = UInt32(truncatingIfNeeded: h)
        return Float(Int32(bitPattern: u32)) / Float(Int32.max)
    }
}
