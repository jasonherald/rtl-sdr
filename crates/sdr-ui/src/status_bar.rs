//! Bottom status bar displaying live metrics: signal level, sample rate, demod mode, frequency.

use gtk4::prelude::*;

use crate::spectrum::frequency_axis::format_frequency;

/// Threshold above which to display in Msps.
const MSPS_THRESHOLD: f64 = 1_000_000.0;
/// Threshold above which to display in ksps.
const KSPS_THRESHOLD: f64 = 1_000.0;

/// Samples per second per Msps.
const SPS_PER_MSPS: f64 = 1_000_000.0;
/// Samples per second per ksps.
const SPS_PER_KSPS: f64 = 1_000.0;

/// Default signal level display text when no data has arrived.
const DEFAULT_LEVEL_TEXT: &str = "Level: -- dBFS";
/// Default sample rate display text when no data has arrived.
const DEFAULT_SAMPLE_RATE_TEXT: &str = "SR: --";
/// Default demod display text when no data has arrived.
const DEFAULT_DEMOD_TEXT: &str = "-- --";
/// Default frequency display text when no data has arrived.
const DEFAULT_FREQUENCY_TEXT: &str = "-- Hz";

/// Bottom status bar showing live metrics.
pub struct StatusBar {
    /// The container widget to pack into the window.
    pub widget: gtk4::Box,
    /// Label showing signal level in dBFS.
    pub signal_level_label: gtk4::Label,
    /// Label showing effective sample rate.
    pub sample_rate_label: gtk4::Label,
    /// Label showing demod mode and bandwidth.
    pub demod_label: gtk4::Label,
    /// Label showing center frequency.
    pub frequency_label: gtk4::Label,
}

impl StatusBar {
    /// Update the signal level display with a new measurement (dBFS).
    pub fn update_signal_level(&self, db: f32) {
        self.signal_level_label
            .set_label(&format!("Level: {db:.1} dBFS"));
    }

    /// Update the sample rate display.
    pub fn update_sample_rate(&self, rate: f64) {
        self.sample_rate_label
            .set_label(&format!("SR: {}", format_sample_rate(rate)));
    }

    /// Update the demod mode and bandwidth display.
    pub fn update_demod(&self, mode: &str, bw: f64) {
        self.demod_label
            .set_label(&format!("{mode} {}", format_bandwidth(bw)));
    }

    /// Update the center frequency display.
    pub fn update_frequency(&self, hz: f64) {
        self.frequency_label.set_label(&format_frequency(hz));
    }
}

/// Build the status bar widget with initial placeholder labels.
pub fn build_status_bar() -> StatusBar {
    let signal_level_label = gtk4::Label::new(Some(DEFAULT_LEVEL_TEXT));
    let sample_rate_label = gtk4::Label::new(Some(DEFAULT_SAMPLE_RATE_TEXT));
    let demod_label = gtk4::Label::new(Some(DEFAULT_DEMOD_TEXT));
    let frequency_label = gtk4::Label::new(Some(DEFAULT_FREQUENCY_TEXT));

    let widget = gtk4::Box::builder()
        .orientation(gtk4::Orientation::Horizontal)
        .spacing(0)
        .css_classes(["status-bar"])
        .build();

    widget.append(&signal_level_label);
    widget.append(&gtk4::Separator::new(gtk4::Orientation::Vertical));
    widget.append(&sample_rate_label);
    widget.append(&gtk4::Separator::new(gtk4::Orientation::Vertical));
    widget.append(&demod_label);
    widget.append(&gtk4::Separator::new(gtk4::Orientation::Vertical));
    widget.append(&frequency_label);

    StatusBar {
        widget,
        signal_level_label,
        sample_rate_label,
        demod_label,
        frequency_label,
    }
}

/// Format a sample rate to a human-readable string with appropriate unit.
///
/// # Examples
///
/// ```
/// # use sdr_ui::status_bar::format_sample_rate;
/// assert_eq!(format_sample_rate(2_400_000.0), "2.4 Msps");
/// assert_eq!(format_sample_rate(250_000.0), "250.0 ksps");
/// assert_eq!(format_sample_rate(500.0), "500 sps");
/// ```
pub fn format_sample_rate(rate: f64) -> String {
    let abs = rate.abs();
    if abs >= MSPS_THRESHOLD {
        format!("{:.1} Msps", rate / SPS_PER_MSPS)
    } else if abs >= KSPS_THRESHOLD {
        format!("{:.1} ksps", rate / SPS_PER_KSPS)
    } else {
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let sps = rate as u64;
        format!("{sps} sps")
    }
}

/// Format a bandwidth in Hz to a human-readable string with appropriate unit.
fn format_bandwidth(hz: f64) -> String {
    let abs = hz.abs();
    if abs >= 1_000_000.0 {
        format!("{:.1} MHz", hz / 1_000_000.0)
    } else if abs >= 1_000.0 {
        format!("{:.1} kHz", hz / 1_000.0)
    } else {
        format!("{hz:.0} Hz")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_sample_rate_msps() {
        assert_eq!(format_sample_rate(2_400_000.0), "2.4 Msps");
        assert_eq!(format_sample_rate(1_000_000.0), "1.0 Msps");
    }

    #[test]
    fn format_sample_rate_ksps() {
        assert_eq!(format_sample_rate(250_000.0), "250.0 ksps");
        assert_eq!(format_sample_rate(48_000.0), "48.0 ksps");
        assert_eq!(format_sample_rate(1_000.0), "1.0 ksps");
    }

    #[test]
    fn format_sample_rate_sps() {
        assert_eq!(format_sample_rate(500.0), "500 sps");
        assert_eq!(format_sample_rate(0.0), "0 sps");
    }

    #[test]
    fn format_bandwidth_mhz() {
        assert_eq!(format_bandwidth(1_000_000.0), "1.0 MHz");
        assert_eq!(format_bandwidth(2_500_000.0), "2.5 MHz");
    }

    #[test]
    fn format_bandwidth_khz() {
        assert_eq!(format_bandwidth(12_500.0), "12.5 kHz");
        assert_eq!(format_bandwidth(200_000.0), "200.0 kHz");
        assert_eq!(format_bandwidth(1_000.0), "1.0 kHz");
    }

    #[test]
    fn format_bandwidth_hz() {
        assert_eq!(format_bandwidth(500.0), "500 Hz");
        assert_eq!(format_bandwidth(0.0), "0 Hz");
    }
}
