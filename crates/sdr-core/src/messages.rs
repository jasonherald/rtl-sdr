//! Message types for communication between the DSP thread and the UI thread.

use sdr_radio::DeemphasisMode;
use sdr_types::DemodMode;

/// Messages sent from the DSP pipeline thread to the UI main loop.
#[derive(Debug)]
pub enum DspToUi {
    /// New FFT magnitude data ready for display.
    FftData(Vec<f32>),
    /// Updated SNR measurement in dB.
    SignalLevel(f32),
    /// A non-fatal error occurred in the pipeline.
    Error(String),
    /// The source has stopped (device disconnected, EOF, etc.).
    SourceStopped,
    /// The effective sample rate changed (after decimation, device reconfiguration).
    SampleRateChanged(f64),
    /// Device information string (e.g., tuner name, USB descriptor).
    DeviceInfo(String),
    /// Available tuner gain values in dB (queried from device on open).
    GainList(Vec<f64>),
    /// Raw (pre-decimation) sample rate for spectrum display bandwidth.
    DisplayBandwidth(f64),
    /// Audio recording started (contains the file path for display).
    AudioRecordingStarted(std::path::PathBuf),
    /// Audio recording stopped.
    AudioRecordingStopped,
    /// IQ recording started (contains the file path for display).
    IqRecordingStarted(std::path::PathBuf),
    /// IQ recording stopped.
    IqRecordingStopped,
}

/// Available source types for IQ input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceType {
    /// RTL-SDR USB dongle.
    RtlSdr,
    /// TCP/UDP network IQ stream.
    Network,
    /// WAV file playback.
    File,
}

/// Messages sent from the UI thread to the DSP pipeline thread.
#[derive(Debug)]
pub enum UiToDsp {
    /// Start the DSP pipeline.
    Start,
    /// Stop the DSP pipeline.
    Stop,
    /// Tune to a new center frequency (Hz).
    Tune(f64),
    /// Change the demodulation mode.
    SetDemodMode(DemodMode),
    /// Set the radio channel bandwidth (Hz).
    SetBandwidth(f64),
    /// Set the squelch threshold (dB).
    SetSquelch(f32),
    /// Enable or disable the squelch gate.
    SetSquelchEnabled(bool),
    /// Enable or disable auto-squelch (noise floor tracking).
    SetAutoSquelch(bool),
    /// Set the audio output volume (0.0..=1.0).
    SetVolume(f32),
    /// Set the FM deemphasis mode.
    SetDeemphasis(DeemphasisMode),
    /// Change the source sample rate (Hz).
    SetSampleRate(f64),
    /// Set the decimation ratio (power-of-2, 1 = none).
    SetDecimation(u32),
    /// Enable or disable DC blocking.
    SetDcBlocking(bool),
    /// Enable or disable IQ inversion (conjugation).
    SetIqInversion(bool),
    /// Change the FFT size for spectrum display.
    SetFftSize(usize),
    /// Enable or disable the noise blanker.
    SetNbEnabled(bool),
    /// Enable or disable FM IF noise reduction.
    SetFmIfNrEnabled(bool),
    /// Set the RTL-SDR tuner gain (dB). Converted to tenths internally.
    SetGain(f64),
    /// Enable or disable RTL-SDR AGC.
    SetAgc(bool),
    /// Enable or disable IQ correction.
    SetIqCorrection(bool),
    /// Set the FFT window function.
    SetWindowFunction(sdr_pipeline::iq_frontend::FftWindow),
    /// Set the VFO frequency offset from center in Hz (for click-to-tune).
    SetVfoOffset(f64),
    /// Set the noise blanker level (threshold multiplier, >= 1.0).
    SetNbLevel(f32),
    /// Enable or disable WFM stereo decode.
    SetWfmStereo(bool),
    /// Set the FFT display frame rate (FPS).
    SetFftRate(f64),
    /// Enable or disable the audio high-pass filter (voice modes).
    SetHighPass(bool),
    /// Enable or disable the audio notch filter.
    SetNotchEnabled(bool),
    /// Set the audio notch filter frequency in Hz.
    SetNotchFrequency(f32),
    /// Set the audio output device by `PipeWire` node name.
    SetAudioDevice(String),
    /// Switch the source type (stops current source if running).
    SetSourceType(SourceType),
    /// Configure network source hostname, port, and protocol.
    SetNetworkConfig {
        hostname: String,
        port: u16,
        protocol: sdr_types::Protocol,
    },
    /// Set the file path for file source playback.
    SetFilePath(std::path::PathBuf),
    /// Set PPM frequency correction for RTL-SDR crystal offset.
    SetPpmCorrection(i32),
    /// Start recording demodulated audio to a WAV file.
    StartAudioRecording(std::path::PathBuf),
    /// Stop audio recording and finalize the WAV file.
    StopAudioRecording,
    /// Start recording raw IQ samples to a WAV file.
    StartIqRecording(std::path::PathBuf),
    /// Stop IQ recording and finalize the WAV file.
    StopIqRecording,
    /// Start sending audio to the transcription engine.
    EnableTranscription(std::sync::mpsc::SyncSender<sdr_transcription::TranscriptionInput>),
    /// Stop sending audio to the transcription engine.
    DisableTranscription,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_dsp_to_ui_variants() {
        let fft = DspToUi::FftData(vec![1.0, 2.0, 3.0]);
        assert!(matches!(fft, DspToUi::FftData(v) if v.len() == 3));

        let snr = DspToUi::SignalLevel(12.5);
        assert!(matches!(snr, DspToUi::SignalLevel(s) if (s - 12.5).abs() < f32::EPSILON));

        let err = DspToUi::Error("test error".to_string());
        assert!(matches!(err, DspToUi::Error(ref s) if s == "test error"));

        let stopped = DspToUi::SourceStopped;
        assert!(matches!(stopped, DspToUi::SourceStopped));

        let sr = DspToUi::SampleRateChanged(2_400_000.0);
        assert!(
            matches!(sr, DspToUi::SampleRateChanged(r) if (r - 2_400_000.0).abs() < f64::EPSILON)
        );

        let info = DspToUi::DeviceInfo("RTL2838UHIDIR".to_string());
        assert!(matches!(info, DspToUi::DeviceInfo(ref s) if s == "RTL2838UHIDIR"));

        let audio_rec = DspToUi::AudioRecordingStarted(std::path::PathBuf::from("/tmp/test.wav"));
        assert!(matches!(audio_rec, DspToUi::AudioRecordingStarted(_)));

        let audio_stop = DspToUi::AudioRecordingStopped;
        assert!(matches!(audio_stop, DspToUi::AudioRecordingStopped));

        let iq_rec = DspToUi::IqRecordingStarted(std::path::PathBuf::from("/tmp/iq.wav"));
        assert!(matches!(iq_rec, DspToUi::IqRecordingStarted(_)));

        let iq_stop = DspToUi::IqRecordingStopped;
        assert!(matches!(iq_stop, DspToUi::IqRecordingStopped));
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn test_ui_to_dsp_variants() {
        let start = UiToDsp::Start;
        assert!(matches!(start, UiToDsp::Start));

        let stop = UiToDsp::Stop;
        assert!(matches!(stop, UiToDsp::Stop));

        let tune = UiToDsp::Tune(144_000_000.0);
        assert!(matches!(tune, UiToDsp::Tune(f) if (f - 144_000_000.0).abs() < f64::EPSILON));

        let mode = UiToDsp::SetDemodMode(DemodMode::Am);
        assert!(matches!(mode, UiToDsp::SetDemodMode(DemodMode::Am)));

        let bw = UiToDsp::SetBandwidth(12_500.0);
        assert!(matches!(bw, UiToDsp::SetBandwidth(b) if (b - 12_500.0).abs() < f64::EPSILON));

        let sq = UiToDsp::SetSquelch(-50.0);
        assert!(matches!(sq, UiToDsp::SetSquelch(s) if (s - (-50.0)).abs() < f32::EPSILON));

        let sqe = UiToDsp::SetSquelchEnabled(true);
        assert!(matches!(sqe, UiToDsp::SetSquelchEnabled(true)));

        let auto_sq = UiToDsp::SetAutoSquelch(true);
        assert!(matches!(auto_sq, UiToDsp::SetAutoSquelch(true)));

        let vol = UiToDsp::SetVolume(0.75);
        assert!(matches!(vol, UiToDsp::SetVolume(v) if (v - 0.75).abs() < f32::EPSILON));

        let deemp = UiToDsp::SetDeemphasis(DeemphasisMode::Eu50);
        assert!(matches!(
            deemp,
            UiToDsp::SetDeemphasis(DeemphasisMode::Eu50)
        ));

        let sr = UiToDsp::SetSampleRate(2_400_000.0);
        assert!(matches!(sr, UiToDsp::SetSampleRate(r) if (r - 2_400_000.0).abs() < f64::EPSILON));

        let dec = UiToDsp::SetDecimation(4);
        assert!(matches!(dec, UiToDsp::SetDecimation(4)));

        let dc = UiToDsp::SetDcBlocking(true);
        assert!(matches!(dc, UiToDsp::SetDcBlocking(true)));

        let iq = UiToDsp::SetIqInversion(false);
        assert!(matches!(iq, UiToDsp::SetIqInversion(false)));

        let fft = UiToDsp::SetFftSize(2048);
        assert!(matches!(fft, UiToDsp::SetFftSize(2048)));

        let nb = UiToDsp::SetNbEnabled(true);
        assert!(matches!(nb, UiToDsp::SetNbEnabled(true)));

        let nr = UiToDsp::SetFmIfNrEnabled(false);
        assert!(matches!(nr, UiToDsp::SetFmIfNrEnabled(false)));

        let gain = UiToDsp::SetGain(33.8);
        assert!(matches!(gain, UiToDsp::SetGain(g) if (g - 33.8).abs() < f64::EPSILON));

        let agc = UiToDsp::SetAgc(true);
        assert!(matches!(agc, UiToDsp::SetAgc(true)));

        let iq_corr = UiToDsp::SetIqCorrection(false);
        assert!(matches!(iq_corr, UiToDsp::SetIqCorrection(false)));

        let wf = UiToDsp::SetWindowFunction(sdr_pipeline::iq_frontend::FftWindow::Blackman);
        assert!(matches!(
            wf,
            UiToDsp::SetWindowFunction(sdr_pipeline::iq_frontend::FftWindow::Blackman)
        ));

        let vfo = UiToDsp::SetVfoOffset(25_000.0);
        assert!(matches!(vfo, UiToDsp::SetVfoOffset(o) if (o - 25_000.0).abs() < f64::EPSILON));

        let nb = UiToDsp::SetNbLevel(5.0);
        assert!(matches!(nb, UiToDsp::SetNbLevel(l) if (l - 5.0).abs() < f32::EPSILON));

        let stereo = UiToDsp::SetWfmStereo(true);
        assert!(matches!(stereo, UiToDsp::SetWfmStereo(true)));

        let fft_rate = UiToDsp::SetFftRate(30.0);
        assert!(matches!(fft_rate, UiToDsp::SetFftRate(r) if (r - 30.0).abs() < f64::EPSILON));

        let hp = UiToDsp::SetHighPass(true);
        assert!(matches!(hp, UiToDsp::SetHighPass(true)));

        let notch_en = UiToDsp::SetNotchEnabled(true);
        assert!(matches!(notch_en, UiToDsp::SetNotchEnabled(true)));

        let notch_freq = UiToDsp::SetNotchFrequency(60.0);
        assert!(
            matches!(notch_freq, UiToDsp::SetNotchFrequency(f) if (f - 60.0).abs() < f32::EPSILON)
        );

        let device = UiToDsp::SetAudioDevice("default".to_string());
        assert!(matches!(device, UiToDsp::SetAudioDevice(ref s) if s == "default"));

        let src_type = UiToDsp::SetSourceType(SourceType::RtlSdr);
        assert!(matches!(
            src_type,
            UiToDsp::SetSourceType(SourceType::RtlSdr)
        ));

        let src_net = UiToDsp::SetSourceType(SourceType::Network);
        assert!(matches!(
            src_net,
            UiToDsp::SetSourceType(SourceType::Network)
        ));

        let src_file = UiToDsp::SetSourceType(SourceType::File);
        assert!(matches!(src_file, UiToDsp::SetSourceType(SourceType::File)));

        let net_cfg = UiToDsp::SetNetworkConfig {
            hostname: "192.168.1.1".to_string(),
            port: 4321,
            protocol: sdr_types::Protocol::TcpClient,
        };
        assert!(matches!(
            net_cfg,
            UiToDsp::SetNetworkConfig { ref hostname, port: 4321, .. } if hostname == "192.168.1.1"
        ));

        let file_path = UiToDsp::SetFilePath(std::path::PathBuf::from("/tmp/test.wav"));
        assert!(matches!(
            file_path,
            UiToDsp::SetFilePath(ref p) if p == std::path::Path::new("/tmp/test.wav")
        ));

        let ppm = UiToDsp::SetPpmCorrection(42);
        assert!(matches!(ppm, UiToDsp::SetPpmCorrection(42)));

        let audio_rec = UiToDsp::StartAudioRecording(std::path::PathBuf::from("/tmp/audio.wav"));
        assert!(matches!(audio_rec, UiToDsp::StartAudioRecording(_)));

        let audio_stop = UiToDsp::StopAudioRecording;
        assert!(matches!(audio_stop, UiToDsp::StopAudioRecording));

        let iq_rec = UiToDsp::StartIqRecording(std::path::PathBuf::from("/tmp/iq.wav"));
        assert!(matches!(iq_rec, UiToDsp::StartIqRecording(_)));

        let iq_stop = UiToDsp::StopIqRecording;
        assert!(matches!(iq_stop, UiToDsp::StopIqRecording));

        let (tx, _rx) = std::sync::mpsc::sync_channel::<sdr_transcription::TranscriptionInput>(1);
        let enable = UiToDsp::EnableTranscription(tx);
        assert!(matches!(enable, UiToDsp::EnableTranscription(_)));

        let disable = UiToDsp::DisableTranscription;
        assert!(matches!(disable, UiToDsp::DisableTranscription));
    }

    #[test]
    fn test_source_type_variants() {
        assert_eq!(SourceType::RtlSdr, SourceType::RtlSdr);
        assert_ne!(SourceType::RtlSdr, SourceType::Network);
        assert_ne!(SourceType::Network, SourceType::File);

        let types = [SourceType::RtlSdr, SourceType::Network, SourceType::File];
        assert_eq!(types.len(), 3);
    }
}
