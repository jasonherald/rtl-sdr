//! DSP thread bridge — owns all backend DSP objects and communicates with the
//! GTK UI via message channels.
//!
//! The DSP thread runs a loop that:
//! 1. Checks for UI commands (non-blocking when running, blocking when stopped).
//! 2. Reads IQ samples from the RTL-SDR device via `read_sync`.
//! 3. Processes samples through `IqFrontend` (decimation, DC blocking, FFT).
//! 4. Processes through `RadioModule` (IF chain, demod, AF chain).
//! 5. Sends FFT data back to the UI for display.

use std::sync::mpsc;
use std::time::Duration;

use sdr_pipeline::iq_frontend::{FftWindow, IqFrontend};
use sdr_radio::RadioModule;
use sdr_rtlsdr::RtlSdrDevice;
use sdr_source_rtlsdr::RtlSdrSource;
use sdr_types::{Complex, Stereo};

use crate::messages::{DspToUi, UiToDsp};

/// Number of IQ sample pairs per USB bulk read.
const IQ_PAIRS_PER_READ: usize = 16_384;

/// Raw USB buffer size in bytes (2 bytes per IQ pair: I + Q).
const RAW_BUF_SIZE: usize = IQ_PAIRS_PER_READ * 2;

/// Default FFT size for spectrum display.
const DEFAULT_FFT_SIZE: usize = 2048;

/// Default sample rate in Hz (2.4 MHz).
const DEFAULT_SAMPLE_RATE: f64 = 2_400_000.0;

/// Default center frequency in Hz (100 MHz — FM broadcast).
const DEFAULT_CENTER_FREQ: f64 = 100_000_000.0;

/// Sleep duration when a USB read returns zero bytes or errors transiently (ms).
const IDLE_SLEEP_MS: u64 = 50;

/// Timeout for blocking `recv` when the pipeline is stopped (ms).
const RECV_TIMEOUT_MS: u64 = 50;

/// RTL-SDR device index to open.
const DEVICE_INDEX: u32 = 0;

/// Spawn the DSP controller thread.
///
/// The thread owns all backend DSP objects and communicates with the UI
/// via `ui_rx` (commands from UI) and `dsp_tx` (data/status to UI).
///
/// This function returns immediately; the DSP work happens on a background
/// thread that runs until the UI channel is dropped.
pub fn spawn_dsp_thread(dsp_tx: mpsc::Sender<DspToUi>, ui_rx: mpsc::Receiver<UiToDsp>) {
    std::thread::Builder::new()
        .name("dsp-controller".into())
        .spawn(move || {
            dsp_thread_main(dsp_tx, ui_rx);
        })
        .expect("failed to spawn DSP controller thread");
}

/// Main function for the DSP controller thread.
///
/// Runs until the `ui_rx` channel is disconnected (UI closed).
#[allow(clippy::needless_pass_by_value)]
fn dsp_thread_main(dsp_tx: mpsc::Sender<DspToUi>, ui_rx: mpsc::Receiver<UiToDsp>) {
    tracing::info!("DSP controller thread started");

    let mut state = match DspState::new() {
        Ok(s) => s,
        Err(e) => {
            tracing::error!("failed to initialize DSP state: {e}");
            let _ = dsp_tx.send(DspToUi::Error(format!("DSP init failed: {e}")));
            return;
        }
    };

    loop {
        if state.running {
            // Non-blocking: drain all pending commands.
            loop {
                match ui_rx.try_recv() {
                    Ok(cmd) => handle_command(&mut state, &dsp_tx, cmd),
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => {
                        tracing::info!("UI channel disconnected — DSP thread exiting");
                        cleanup(&mut state);
                        return;
                    }
                }
            }

            // Read and process one IQ block.
            process_iq_block(&mut state, &dsp_tx);
        } else {
            // Pipeline stopped — block with timeout to avoid busy-waiting.
            match ui_rx.recv_timeout(Duration::from_millis(RECV_TIMEOUT_MS)) {
                Ok(cmd) => handle_command(&mut state, &dsp_tx, cmd),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    tracing::info!("UI channel disconnected — DSP thread exiting");
                    return;
                }
            }
        }
    }
}

/// Mutable state owned by the DSP thread.
struct DspState {
    device: Option<RtlSdrDevice>,
    frontend: IqFrontend,
    radio: RadioModule,
    running: bool,
    center_freq: f64,
    sample_rate: f64,
    #[allow(dead_code)]
    volume: f32,

    // Pre-allocated buffers
    raw_buf: Vec<u8>,
    iq_buf: Vec<Complex>,
    processed_buf: Vec<Complex>,
    fft_buf: Vec<f32>,
    audio_buf: Vec<Stereo>,
}

impl DspState {
    fn new() -> Result<Self, String> {
        let frontend = IqFrontend::new(
            DEFAULT_SAMPLE_RATE,
            1, // no decimation
            DEFAULT_FFT_SIZE,
            FftWindow::Nuttall,
            true, // DC blocking on by default
        )
        .map_err(|e| format!("IqFrontend init: {e}"))?;

        let radio =
            RadioModule::with_default_rate().map_err(|e| format!("RadioModule init: {e}"))?;

        Ok(Self {
            device: None,
            frontend,
            radio,
            running: false,
            center_freq: DEFAULT_CENTER_FREQ,
            sample_rate: DEFAULT_SAMPLE_RATE,
            volume: 1.0,
            raw_buf: vec![0u8; RAW_BUF_SIZE],
            iq_buf: vec![Complex::default(); IQ_PAIRS_PER_READ],
            processed_buf: vec![Complex::default(); IQ_PAIRS_PER_READ],
            fft_buf: vec![0.0; DEFAULT_FFT_SIZE],
            audio_buf: Vec::new(),
        })
    }
}

/// Handle a single UI command.
#[allow(clippy::too_many_lines, clippy::needless_pass_by_value)]
fn handle_command(state: &mut DspState, dsp_tx: &mpsc::Sender<DspToUi>, cmd: UiToDsp) {
    match cmd {
        UiToDsp::Start => {
            if state.running {
                tracing::warn!("start requested but already running");
                return;
            }
            tracing::info!("starting DSP pipeline");
            match open_device(state) {
                Ok(()) => {
                    state.running = true;
                    tracing::info!("DSP pipeline started");
                }
                Err(e) => {
                    tracing::error!("failed to start source: {e}");
                    let _ = dsp_tx.send(DspToUi::Error(format!("Start failed: {e}")));
                    let _ = dsp_tx.send(DspToUi::SourceStopped);
                }
            }
        }

        UiToDsp::Stop => {
            if !state.running {
                tracing::warn!("stop requested but not running");
                return;
            }
            tracing::info!("stopping DSP pipeline");
            cleanup(state);
            state.running = false;
            let _ = dsp_tx.send(DspToUi::SourceStopped);
        }

        UiToDsp::Tune(freq) => {
            tracing::debug!(frequency_hz = freq, "tune command");
            state.center_freq = freq;
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            if let Some(dev) = &mut state.device
                && let Err(e) = dev.set_center_freq(freq as u32)
            {
                tracing::warn!("tune failed: {e}");
                let _ = dsp_tx.send(DspToUi::Error(format!("Tune failed: {e}")));
            }
        }

        UiToDsp::SetDemodMode(mode) => {
            tracing::debug!(?mode, "set demod mode");
            if let Err(e) = state.radio.set_mode(mode) {
                tracing::warn!("set demod mode failed: {e}");
                let _ = dsp_tx.send(DspToUi::Error(format!("Mode switch failed: {e}")));
            }
        }

        UiToDsp::SetBandwidth(bw) => {
            tracing::debug!(bandwidth_hz = bw, "set bandwidth");
            state.radio.set_bandwidth(bw);
        }

        UiToDsp::SetSquelch(level) => {
            tracing::debug!(squelch_db = level, "set squelch level");
            state.radio.set_squelch(level);
        }

        UiToDsp::SetSquelchEnabled(enabled) => {
            tracing::debug!(enabled, "set squelch enabled");
            state.radio.set_squelch_enabled(enabled);
        }

        UiToDsp::SetVolume(vol) => {
            tracing::debug!(volume = vol, "set volume");
            state.volume = vol;
        }

        UiToDsp::SetDeemphasis(mode) => {
            tracing::debug!(?mode, "set deemphasis");
            if let Err(e) = state.radio.set_deemp_mode(mode) {
                tracing::warn!("set deemphasis failed: {e}");
                let _ = dsp_tx.send(DspToUi::Error(format!("Deemphasis failed: {e}")));
            }
        }

        UiToDsp::SetSampleRate(rate) => {
            tracing::debug!(sample_rate = rate, "set sample rate");
            state.sample_rate = rate;
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            if let Some(dev) = &mut state.device
                && let Err(e) = dev.set_sample_rate(rate as u32)
            {
                tracing::warn!("set sample rate failed: {e}");
                let _ = dsp_tx.send(DspToUi::Error(format!("Sample rate failed: {e}")));
                return;
            }
            match rebuild_frontend(state) {
                Ok(()) => {
                    let _ = dsp_tx.send(DspToUi::SampleRateChanged(
                        state.frontend.effective_sample_rate(),
                    ));
                }
                Err(e) => {
                    tracing::warn!("frontend rebuild failed: {e}");
                    let _ = dsp_tx.send(DspToUi::Error(format!("Frontend rebuild: {e}")));
                }
            }
        }

        UiToDsp::SetDecimation(ratio) => {
            tracing::debug!(ratio, "set decimation");
            if let Err(e) = state.frontend.set_decimation(ratio) {
                tracing::warn!("set decimation failed: {e}");
                let _ = dsp_tx.send(DspToUi::Error(format!("Decimation failed: {e}")));
            } else {
                let _ = dsp_tx.send(DspToUi::SampleRateChanged(
                    state.frontend.effective_sample_rate(),
                ));
            }
        }

        UiToDsp::SetDcBlocking(enabled) => {
            tracing::debug!(enabled, "set DC blocking");
            if let Err(e) = state.frontend.set_dc_blocking(enabled) {
                tracing::warn!("set DC blocking failed: {e}");
            }
        }

        UiToDsp::SetIqInversion(enabled) => {
            tracing::debug!(enabled, "set IQ inversion");
            state.frontend.set_invert_iq(enabled);
        }

        UiToDsp::SetFftSize(size) => {
            tracing::debug!(fft_size = size, "set FFT size");
            match IqFrontend::new(
                state.frontend.sample_rate(),
                state.frontend.decim_ratio(),
                size,
                FftWindow::Nuttall,
                true,
            ) {
                Ok(new_frontend) => {
                    state.frontend = new_frontend;
                    state.fft_buf = vec![0.0; size];
                }
                Err(e) => {
                    tracing::warn!("set FFT size failed: {e}");
                    let _ = dsp_tx.send(DspToUi::Error(format!("FFT size failed: {e}")));
                }
            }
        }

        UiToDsp::SetNbEnabled(enabled) => {
            tracing::debug!(enabled, "set noise blanker");
            state.radio.if_chain_mut().set_nb_enabled(enabled);
        }

        UiToDsp::SetFmIfNrEnabled(enabled) => {
            tracing::debug!(enabled, "set FM IF NR");
            state.radio.if_chain_mut().set_fm_if_nr_enabled(enabled);
        }
    }
}

/// Open the RTL-SDR device and configure it for streaming.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn open_device(state: &mut DspState) -> Result<(), String> {
    let mut device = RtlSdrDevice::open(DEVICE_INDEX).map_err(|e| e.to_string())?;

    device
        .set_sample_rate(state.sample_rate as u32)
        .map_err(|e| format!("set sample rate: {e}"))?;

    device
        .set_center_freq(state.center_freq as u32)
        .map_err(|e| format!("set center freq: {e}"))?;

    device
        .reset_buffer()
        .map_err(|e| format!("reset buffer: {e}"))?;

    // Rebuild the frontend to match the configured sample rate.
    state.device = Some(device);

    rebuild_frontend(state)?;

    tracing::info!(
        sample_rate = state.sample_rate,
        center_freq = state.center_freq,
        "RTL-SDR device opened"
    );
    Ok(())
}

/// Stop the device and release resources.
fn cleanup(state: &mut DspState) {
    // Dropping the device closes the USB handle.
    state.device = None;
    tracing::info!("RTL-SDR device closed");
}

/// Rebuild the IQ frontend with the current sample rate, preserving other settings.
fn rebuild_frontend(state: &mut DspState) -> Result<(), String> {
    let new_frontend = IqFrontend::new(
        state.sample_rate,
        state.frontend.decim_ratio(),
        state.frontend.fft_size(),
        FftWindow::Nuttall,
        true,
    )
    .map_err(|e| format!("frontend rebuild: {e}"))?;

    state.frontend = new_frontend;
    Ok(())
}

/// Read one block of IQ data from the device, process it, and send FFT data
/// to the UI.
fn process_iq_block(state: &mut DspState, dsp_tx: &mpsc::Sender<DspToUi>) {
    // Destructure to allow simultaneous borrows of different fields.
    let Some(device) = state.device.as_ref() else {
        tracing::warn!("process_iq_block called without device");
        state.running = false;
        let _ = dsp_tx.send(DspToUi::SourceStopped);
        return;
    };

    // Read raw USB samples.
    let raw_buf = &mut state.raw_buf;
    let bytes_read = match device.read_sync(raw_buf) {
        Ok(n) => n,
        Err(e) => {
            tracing::warn!("USB read error: {e}");
            let _ = dsp_tx.send(DspToUi::Error(format!("Read error: {e}")));
            // On persistent read errors the user should stop manually.
            std::thread::sleep(Duration::from_millis(IDLE_SLEEP_MS));
            return;
        }
    };

    if bytes_read == 0 {
        std::thread::sleep(Duration::from_millis(IDLE_SLEEP_MS));
        return;
    }

    // Convert uint8 pairs to f32 Complex.
    let iq_count = RtlSdrSource::convert_samples(&state.raw_buf[..bytes_read], &mut state.iq_buf);

    // Process through IQ frontend (decimation, DC blocking, FFT).
    match state.frontend.process(
        &state.iq_buf[..iq_count],
        &mut state.processed_buf,
        &mut state.fft_buf,
    ) {
        Ok((processed_count, fft_ready)) => {
            // Send FFT data to UI if a new frame is ready.
            if fft_ready {
                let _ = dsp_tx.send(DspToUi::FftData(state.fft_buf.clone()));
            }

            // Process through radio module for audio output.
            if processed_count > 0 {
                let max_out = state.radio.max_output_samples(processed_count);
                state.audio_buf.resize(max_out, Stereo::default());
                match state.radio.process(
                    &state.processed_buf[..processed_count],
                    &mut state.audio_buf,
                ) {
                    Ok(_audio_count) => {
                        // Audio sink output will be connected in a future PR.
                    }
                    Err(e) => {
                        tracing::warn!("radio processing error: {e}");
                    }
                }
            }
        }
        Err(e) => {
            tracing::warn!("frontend processing error: {e}");
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// Compile-time validation that DSP buffer constants are consistent.
    const _: () = {
        assert!(RAW_BUF_SIZE == IQ_PAIRS_PER_READ * 2);
        assert!(DEFAULT_FFT_SIZE > 0);
        assert!(DEFAULT_SAMPLE_RATE > 0.0);
        assert!(DEFAULT_CENTER_FREQ > 0.0);
        assert!(IDLE_SLEEP_MS > 0);
        assert!(RECV_TIMEOUT_MS > 0);
    };

    #[test]
    fn dsp_state_creates_successfully() {
        let state = DspState::new().unwrap();
        assert!(!state.running);
        assert!(state.device.is_none());
        assert_eq!(state.raw_buf.len(), RAW_BUF_SIZE);
        assert_eq!(state.iq_buf.len(), IQ_PAIRS_PER_READ);
        assert_eq!(state.fft_buf.len(), DEFAULT_FFT_SIZE);
    }
}
