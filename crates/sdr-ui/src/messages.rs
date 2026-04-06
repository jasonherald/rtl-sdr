//! Message types for communication between the DSP thread and the UI thread.

use sdr_radio::DeemphasisMode;
use sdr_types::DemodMode;

/// Messages sent from the DSP pipeline thread to the UI main loop.
#[derive(Debug)]
pub enum DspToUi {
    /// New FFT magnitude data ready for display.
    FftData(Vec<f32>),
    /// Updated SNR measurement in dB.
    SnrUpdate(f32),
    /// A non-fatal error occurred in the pipeline.
    Error(String),
    /// The source has stopped (device disconnected, EOF, etc.).
    SourceStopped,
    /// The effective sample rate changed (after decimation, device reconfiguration).
    SampleRateChanged(f64),
    /// Device information string (e.g., tuner name, USB descriptor).
    DeviceInfo(String),
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_dsp_to_ui_variants() {
        let fft = DspToUi::FftData(vec![1.0, 2.0, 3.0]);
        assert!(matches!(fft, DspToUi::FftData(v) if v.len() == 3));

        let snr = DspToUi::SnrUpdate(12.5);
        assert!(matches!(snr, DspToUi::SnrUpdate(s) if (s - 12.5).abs() < f32::EPSILON));

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
    }

    #[test]
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
    }
}
