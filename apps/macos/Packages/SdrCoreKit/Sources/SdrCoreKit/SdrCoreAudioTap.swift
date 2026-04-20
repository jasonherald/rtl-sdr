//
// SdrCoreAudioTap.swift — Swift wrapper for the audio-tap FFI.
//
// Presents the push-style `sdr_core_start_audio_tap` /
// `sdr_core_stop_audio_tap` surface as an `AsyncStream<[Float]>`
// of 16 kHz mono chunks. Primary consumer is the transcription
// panel (issue #314) which feeds the stream into Apple's
// SpeechAnalyzer / SpeechTranscriber.
//
// Lifecycle: `SdrCore.startAudioTap()` returns an
// `AudioTapSession` that keeps the tap alive until `stop()` is
// called or the session is deinited. Drop the session → tap
// stops. Only one tap per `SdrCore` at a time; the FFI
// rejects a second start with `.invalidHandle`.

import Foundation
@preconcurrency import sdr_core_c

/// Active audio-tap session. Hold a reference to keep the tap
/// alive. Call `stop()` explicitly or drop the reference to
/// tear the tap down (the deinit tears down the FFI-side state
/// and finishes the stream).
public final class AudioTapSession: @unchecked Sendable {
    /// 16 kHz mono f32 chunks from the engine's post-demod
    /// audio stream. Each chunk is one DSP-thread audio block
    /// post-resample (~300 samples at 48 kHz input / 1024-sample
    /// blocks — exact size depends on the engine's block size).
    ///
    /// Suitable for feeding directly into Apple's
    /// `SpeechAnalyzer` via an `AVAudioPCMBuffer` wrapper.
    public let samples: AsyncStream<[Float]>

    // Strong ref back to the engine so the handle stays alive
    // at least until stop()/deinit. Without this, a consumer
    // that drops SdrCore first would leave the Rust dispatcher
    // thread running against a freed handle (though
    // `sdr_core_destroy` itself stops any active tap defensively,
    // this ordering guarantee makes teardown deterministic).
    private let owner: SdrCore

    /// Opaque pointer retained for the duration of the session.
    /// The C trampoline dereferences it on every chunk.
    private let box: AudioTapBox

    private let stopLock = NSLock()
    private var stopped = false

    /// Passed to the FFI as `user_data`. Pulled back to `self`
    /// inside `trampoline` via `Unmanaged.fromOpaque` + a
    /// `takeUnretainedValue` — the box is retained by this
    /// session, not by the callback.
    private let boxPtr: UnsafeMutableRawPointer

    fileprivate init(owner: SdrCore, handle: OpaquePointer) throws {
        self.owner = owner

        // Bounded buffer — SpeechAnalyzer consumers can briefly
        // lag (first-invocation model warmup runs ~100-200 ms).
        // `bufferingNewest(32)` matches the Rust-side channel
        // depth so the Swift consumer's backpressure window is
        // the same size as the FFI's (no double-queuing past
        // what the Rust side already allows).
        var continuation: AsyncStream<[Float]>.Continuation! = nil
        self.samples = AsyncStream<[Float]>(
            bufferingPolicy: .bufferingNewest(32)
        ) { continuation = $0 }

        let box = AudioTapBox(continuation: continuation)
        self.box = box
        self.boxPtr = Unmanaged.passUnretained(box).toOpaque()

        let rc = sdr_core_start_audio_tap(
            handle,
            AudioTapSession.trampoline,
            self.boxPtr
        )
        if rc != 0 {
            // Surface the FFI error message before finish() clears
            // the continuation so the caller gets the real cause.
            let error = SdrCoreError.fromCurrentError(rawCode: rc)
            continuation.finish()
            throw error
        }
    }

    /// Stop the tap. Idempotent. Blocks until the FFI dispatcher
    /// thread has joined — by the time this returns, the
    /// `samples` stream has finished and no more callbacks will
    /// fire. Safe to call from `deinit`.
    ///
    /// If the underlying FFI stop returns a failure code other
    /// than `invalidHandle` (which means there was nothing to
    /// stop — treated as success), the session state is left
    /// untouched so the caller can retry. That prevents the
    /// Swift lifecycle from desynchronizing with the native
    /// tap state when, e.g., a mutex is poisoned inside the
    /// dispatcher. Per CodeRabbit round 1 on PR #349.
    public func stop() {
        stopLock.lock()
        defer { stopLock.unlock() }
        guard !stopped else { return }
        // FFI contract: stop joins the dispatcher before
        // returning. Any in-flight trampoline call completes
        // first; the Swift side then safely finishes the stream.
        let rc = sdr_core_stop_audio_tap(owner.handle)
        let code = SdrCoreError.Code(raw: rc)
        // `invalidHandle` on stop means the tap is already gone
        // (handle destroyed, or the engine tore it down via
        // sdr_core_destroy); treat as a successful stop so the
        // session's retry path doesn't wedge.
        let successful = rc == 0 || code == .invalidHandle
        guard successful else {
            // Don't flip `stopped` or finish the stream —
            // leaves the session in a state the caller can
            // retry. deinit will re-attempt.
            assertionFailure("sdr_core_stop_audio_tap failed: rc=\(rc) code=\(code)")
            return
        }
        stopped = true
        box.continuation.finish()
    }

    deinit {
        stop()
    }

    // ------------------------------------------------------
    //  C trampoline
    // ------------------------------------------------------

    /// Box holding just the continuation. Retained by the
    /// enclosing session; `trampoline` pulls it out via
    /// `takeUnretainedValue` so the callback doesn't decrement
    /// the retain count.
    private final class AudioTapBox {
        let continuation: AsyncStream<[Float]>.Continuation
        init(continuation: AsyncStream<[Float]>.Continuation) {
            self.continuation = continuation
        }
    }

    /// C trampoline. Fires from the FFI's
    /// `sdr-ffi-audio-tap-dispatcher` thread. Copies the
    /// borrowed sample buffer into an owned Swift `[Float]`
    /// (so the value is safe to outlive the callback) and
    /// yields it to the stream.
    ///
    /// `yield` on `AsyncStream.Continuation` is thread-safe and
    /// non-blocking — exactly the shape the FFI expects from a
    /// dispatcher-thread callback.
    private static let trampoline: @convention(c) (
        UnsafePointer<Float>?,
        Int,
        UnsafeMutableRawPointer?
    ) -> Void = { samplesPtr, sampleCount, userData in
        guard let samplesPtr, let userData, sampleCount > 0 else { return }

        let box = Unmanaged<AudioTapBox>.fromOpaque(userData).takeUnretainedValue()

        // Copy into an owned array. The borrowed pointer is
        // only valid for the duration of this call; once we
        // return, the Rust side frees the underlying Vec.
        let buffer = UnsafeBufferPointer(start: samplesPtr, count: sampleCount)
        let chunk = Array(buffer)

        box.continuation.yield(chunk)
    }
}

// MARK: - SdrCore convenience

extension SdrCore {
    /// Start the post-demod audio tap. Returns an active session
    /// — keep a reference to it for as long as you want audio
    /// chunks delivered; call `stop()` or drop the reference to
    /// tear it down.
    ///
    /// Only one tap can be active per `SdrCore` at a time.
    /// Starting a second tap while one is active throws
    /// `SdrCoreError.invalidHandle`; the host must
    /// `stop()` the existing session first.
    ///
    /// The callback fires on a dedicated Rust dispatcher
    /// thread. The `AsyncStream` `yield` is thread-safe; hosts
    /// iterating with `for await chunk in session.samples`
    /// receive values on their own task's executor.
    public func startAudioTap() throws -> AudioTapSession {
        try AudioTapSession(owner: self, handle: handle)
    }
}
