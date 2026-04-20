//
// TranscriptionDriver.swift — orchestrates Apple SpeechAnalyzer
// against the engine's post-demod audio tap.
//
// Issue #314. macOS 26+ only — `SpeechAnalyzer` and
// `SpeechTranscriber` are Apple-native frameworks that ship with
// the OS (no model download, no binary bloat) and run on the
// Neural Engine. Linux keeps the Whisper / Sherpa-onnx backends
// via `sdr-transcription`; this driver is entirely parallel and
// handle-free on the engine side.
//
// Lifecycle:
//   toggle(true) → requestAuthorization → install locale asset
//     → start audio tap → start analyzer → stream results
//   toggle(false) → finalize analyzer → stop audio tap
//
// State model is `@Observable`. The UI reads
// `status`/`partialText`/`finalizedLines` and calls `toggle()`
// / `clearTranscript()` as side-effect-free view actions.

@preconcurrency import AVFoundation
import Foundation
import Observation
import SdrCoreKit
import Speech

@Observable
@MainActor
final class TranscriptionDriver {
    /// Auth state for the Speech framework. Mirrors
    /// `SFSpeechRecognizerAuthorizationStatus` so the UI doesn't
    /// have to import `Speech` itself.
    enum PermissionStatus: Sendable, Equatable {
        case notDetermined
        case authorized
        case denied
        case restricted
    }

    /// High-level driver status. One of these at a time.
    enum Status: Sendable, Equatable {
        case idle
        case preparing(String) // "Requesting permission…", "Downloading model…", …
        case listening
        case error(String)
    }

    /// One finalized transcript utterance. Stored in a list so
    /// the UI can render a scrollable history.
    struct Line: Identifiable, Hashable, Sendable {
        let id = UUID()
        let timestamp: Date
        let text: String
    }

    // MARK: - Observable state (read by the view)

    /// User toggle — what the toggle row in the panel reflects.
    /// Flipping this triggers `start()` / `stop()` via `toggle(_:)`;
    /// setting it directly doesn't start the driver, so the view
    /// should route through `toggle`.
    var enabled: Bool = false

    var permission: PermissionStatus = .notDetermined

    var status: Status = .idle

    /// 0…1 when downloading a locale asset, nil otherwise.
    /// Bound to a `ProgressView` in the panel.
    var downloadProgress: Double? = nil

    /// Finalized utterances, in arrival order. Cleared only by
    /// an explicit `clearTranscript()` call — matches GTK.
    var finalizedLines: [Line] = []

    /// Current partial hypothesis, or empty when no utterance is
    /// in flight. The UI renders this below the finalized-lines
    /// list (dimmed italic) — same pattern as the GTK panel's
    /// live-line label.
    var partialText: String = ""

    // MARK: - Private wiring

    private weak var core: CoreModel?
    private var audioTap: AudioTapSession?
    private var feederTask: Task<Void, Never>?
    private var resultsTask: Task<Void, Never>?
    private var downloadProgressTask: Task<Void, Never>?

    /// The input-stream continuation for the active analyzer
    /// session. `nil` when idle. We retain it so `stop()` can
    /// cleanly finish the stream before the analyzer teardown.
    private var inputContinuation: AsyncStream<AnalyzerInput>.Continuation?

    /// Last-built analyzer. Retained so `stop()` can call
    /// `finalizeAndFinishThroughEndOfInput()`.
    private var analyzer: SpeechAnalyzer?

    /// Locale we transcribe in. en-US default; a locale picker
    /// could land later alongside `SpeechTranscriber.supportedLocales`.
    private let locale = Locale(identifier: "en-US")

    // MARK: - View-facing API

    /// Wire in the engine handle. Called once at bootstrap.
    func attach(core: CoreModel) {
        self.core = core
    }

    /// Toggle transcription on/off. Routes to `start()` / `stop()`
    /// and updates the observable `enabled` mirror.
    func toggle(_ on: Bool) {
        if on {
            enabled = true
            Task { await self.start() }
        } else {
            enabled = false
            Task { await self.stop() }
        }
    }

    /// Drop the current transcript. Doesn't affect the running
    /// session — matches the GTK Clear button.
    func clearTranscript() {
        finalizedLines.removeAll()
        partialText = ""
    }

    // MARK: - Session lifecycle

    private func start() async {
        guard let core = self.core else {
            status = .error("engine not attached")
            enabled = false
            return
        }

        // 1. Authorization.
        status = .preparing("Requesting permission…")
        let authStatus = await Self.requestSpeechAuthorization()
        permission = Self.mapAuth(authStatus)
        switch permission {
        case .authorized:
            break
        case .notDetermined:
            // Shouldn't happen after requestAuthorization resolves,
            // but surface it rather than hanging.
            status = .error("permission indeterminate")
            enabled = false
            return
        case .denied:
            status = .error("Speech recognition denied — enable in System Settings → Privacy")
            enabled = false
            return
        case .restricted:
            status = .error("Speech recognition restricted by system policy")
            enabled = false
            return
        }

        // 2. Ensure the locale asset is installed.
        let supported = await SpeechTranscriber.supportedLocales
        guard supported.contains(where: { $0.identifier(.bcp47) == locale.identifier(.bcp47) }) else {
            status = .error("Locale \(locale.identifier) not supported by SpeechTranscriber")
            enabled = false
            return
        }

        let installed = await SpeechTranscriber.installedLocales
        let alreadyInstalled = installed.contains {
            $0.identifier(.bcp47) == locale.identifier(.bcp47)
        }

        // Build the transcriber with the option set matching
        // GTK's live-captions behavior: volatile (partial) +
        // final results, no extra attributes.
        let transcriber = SpeechTranscriber(
            locale: locale,
            transcriptionOptions: [],
            reportingOptions: [.volatileResults],
            attributeOptions: []
        )

        if !alreadyInstalled {
            status = .preparing("Downloading model…")
            do {
                if let downloader = try await AssetInventory.assetInstallationRequest(
                    supporting: [transcriber]
                ) {
                    // Poll the `Progress` fractionCompleted at
                    // ~5 Hz until the downloadAndInstall() call
                    // returns. KVO observation on Foundation's
                    // Progress is awkward from Swift 6 strict
                    // concurrency (the key-path closure wants a
                    // concrete root type and Progress is not
                    // Sendable); a short poll loop is simpler,
                    // and this runs only during the one-shot
                    // first-install.
                    let progress = downloader.progress
                    downloadProgressTask = Task { [weak self] in
                        while !Task.isCancelled {
                            let fraction = progress.fractionCompleted
                            await MainActor.run {
                                self?.downloadProgress = fraction
                            }
                            try? await Task.sleep(nanoseconds: 200_000_000)
                        }
                    }
                    try await downloader.downloadAndInstall()
                    downloadProgressTask?.cancel()
                    downloadProgressTask = nil
                    downloadProgress = nil
                }
            } catch {
                downloadProgressTask?.cancel()
                downloadProgressTask = nil
                downloadProgress = nil
                status = .error("Model download failed: \(error.localizedDescription)")
                enabled = false
                return
            }
        }

        // 3. Build analyzer and its input stream.
        let analyzer = SpeechAnalyzer(modules: [transcriber])
        let (inputSequence, inputBuilder) = AsyncStream<AnalyzerInput>.makeStream()

        // Figure out the format SpeechAnalyzer actually wants so
        // we can resample from our 16 kHz mono f32 tap if it
        // asks for anything different. Most commonly the best
        // format IS 16 kHz mono Float32 — in which case the
        // converter is a no-op passthrough.
        let analyzerFormat = await SpeechAnalyzer.bestAvailableAudioFormat(
            compatibleWith: [transcriber]
        )
        guard let analyzerFormat else {
            status = .error("SpeechAnalyzer reports no compatible audio format")
            enabled = false
            return
        }

        guard let tapFormat = AVAudioFormat(
            commonFormat: .pcmFormatFloat32,
            sampleRate: 16_000,
            channels: 1,
            interleaved: false
        ) else {
            status = .error("Failed to build 16 kHz mono Float32 AVAudioFormat")
            enabled = false
            return
        }

        let converter = AVAudioConverter(from: tapFormat, to: analyzerFormat)

        // 4. Start the audio tap and the analyzer. The tap
        // lives on the underlying `SdrCore` handle, not the
        // `CoreModel` wrapper — the model exposes it via
        // `private(set) var core: SdrCore?` so we reach through.
        guard let handle = core.core else {
            status = .error("engine not running")
            enabled = false
            return
        }
        do {
            let tap = try handle.startAudioTap()
            self.audioTap = tap
            self.analyzer = analyzer
            self.inputContinuation = inputBuilder

            try await analyzer.start(inputSequence: inputSequence)
            status = .listening
        } catch {
            status = .error("Failed to start analyzer: \(error.localizedDescription)")
            enabled = false
            await teardown()
            return
        }

        // 5. Spawn the feeder (tap → AnalyzerInput yield) and the
        // results consumer.
        let tap = self.audioTap!
        feederTask = Task.detached(priority: .userInitiated) { [weak self] in
            await Self.runFeeder(
                samples: tap.samples,
                tapFormat: tapFormat,
                analyzerFormat: analyzerFormat,
                converter: converter,
                builder: inputBuilder
            )
            // Feeder exits when the tap stream finishes. Finalize
            // so the analyzer drains any in-flight audio.
            inputBuilder.finish()
            await self?.onFeederEnded()
        }

        resultsTask = Task { [weak self] in
            do {
                for try await result in transcriber.results {
                    let rendered = String(result.text.characters)
                    await MainActor.run {
                        guard let self else { return }
                        if result.isFinal {
                            let line = Line(timestamp: Date(), text: rendered)
                            self.finalizedLines.append(line)
                            self.partialText = ""
                        } else {
                            self.partialText = rendered
                        }
                    }
                }
            } catch {
                await MainActor.run {
                    self?.status = .error("Transcription stream error: \(error.localizedDescription)")
                    self?.enabled = false
                }
                await self?.teardown()
            }
        }
    }

    private func stop() async {
        // Finish the input stream so the analyzer drains.
        inputContinuation?.finish()

        // Best-effort finalize; swallow errors — we're tearing
        // down either way.
        if let analyzer = self.analyzer {
            try? await analyzer.finalizeAndFinishThroughEndOfInput()
        }

        await teardown()
        if case .error = status {
            // Preserve the error message
        } else {
            status = .idle
        }
    }

    /// Teardown shared by the normal stop path and the error
    /// path. Safe to call multiple times.
    private func teardown() async {
        feederTask?.cancel()
        feederTask = nil
        resultsTask?.cancel()
        resultsTask = nil
        downloadProgressTask?.cancel()
        downloadProgressTask = nil
        downloadProgress = nil
        inputContinuation = nil
        analyzer = nil
        audioTap?.stop()
        audioTap = nil
        partialText = ""
    }

    private func onFeederEnded() async {
        // The tap stream closed — the engine is torn down or the
        // user hit Stop. Let the normal teardown path run.
    }

    // MARK: - Helpers

    /// `SFSpeechRecognizer.requestAuthorization` is callback-based;
    /// wrap it in a continuation so we can `await` it.
    private static func requestSpeechAuthorization() async
        -> SFSpeechRecognizerAuthorizationStatus
    {
        await withCheckedContinuation { continuation in
            SFSpeechRecognizer.requestAuthorization { status in
                continuation.resume(returning: status)
            }
        }
    }

    private static func mapAuth(
        _ status: SFSpeechRecognizerAuthorizationStatus
    ) -> PermissionStatus {
        switch status {
        case .authorized: .authorized
        case .denied: .denied
        case .restricted: .restricted
        case .notDetermined: .notDetermined
        @unknown default: .denied
        }
    }

    /// Feeder loop — runs off-main, pulls Float chunks from the
    /// tap, wraps them in AVAudioPCMBuffer, converts to the
    /// analyzer's preferred format, and yields `AnalyzerInput`
    /// into the shared stream.
    ///
    /// When the tap stream finishes (engine torn down or user
    /// stopped), this returns. The caller is responsible for
    /// `builder.finish()`.
    private static func runFeeder(
        samples: AsyncStream<[Float]>,
        tapFormat: AVAudioFormat,
        analyzerFormat: AVAudioFormat,
        converter: AVAudioConverter?,
        builder: AsyncStream<AnalyzerInput>.Continuation
    ) async {
        for await chunk in samples {
            guard !Task.isCancelled else { return }

            guard let tapBuffer = AVAudioPCMBuffer(
                pcmFormat: tapFormat,
                frameCapacity: AVAudioFrameCount(chunk.count)
            ) else { continue }
            tapBuffer.frameLength = AVAudioFrameCount(chunk.count)
            chunk.withUnsafeBufferPointer { src in
                if let dest = tapBuffer.floatChannelData?[0],
                   let base = src.baseAddress
                {
                    dest.update(from: base, count: chunk.count)
                }
            }

            let outBuffer: AVAudioPCMBuffer
            if let converter, analyzerFormat != tapFormat {
                // Resample / reformat. Output buffer sized
                // proportionally to the input/output rate ratio
                // with 32 samples of slack for converter state.
                let ratio = analyzerFormat.sampleRate / tapFormat.sampleRate
                let outCapacity = AVAudioFrameCount(
                    Double(tapBuffer.frameLength) * ratio + 32
                )
                guard let converted = AVAudioPCMBuffer(
                    pcmFormat: analyzerFormat,
                    frameCapacity: outCapacity
                ) else { continue }
                var error: NSError?
                let _ = converter.convert(to: converted, error: &error) { _, outStatus in
                    outStatus.pointee = .haveData
                    return tapBuffer
                }
                if error != nil { continue }
                outBuffer = converted
            } else {
                outBuffer = tapBuffer
            }

            builder.yield(AnalyzerInput(buffer: outBuffer))
        }
    }
}
