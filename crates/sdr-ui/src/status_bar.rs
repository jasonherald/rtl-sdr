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
/// Default role badge text when no `rtl_tcp` session is active.
/// The label itself stays invisible in this state so it doesn't
/// clutter the bar for users on local RTL-SDR / File / Network
/// sources. Per issue #396.
const DEFAULT_ROLE_TEXT: &str = "";
/// Default sample rate display text when no data has arrived.
const DEFAULT_SAMPLE_RATE_TEXT: &str = "SR: --";
/// Default demod display text when no data has arrived.
const DEFAULT_DEMOD_TEXT: &str = "-- --";
/// Default frequency display text when no data has arrived.
const DEFAULT_FREQUENCY_TEXT: &str = "-- Hz";
/// Default cursor readout text when the cursor is not over the spectrum.
const DEFAULT_CURSOR_TEXT: &str = "Cursor: --";

/// Role badge state rendered by the status bar when connected
/// to an `rtl_tcp` server. Variants carry role provenance
/// explicitly — the API that previously took an `Option<bool>`
/// (`true` = Controller, `false` = Listener) couldn't tell the
/// caller whether the bool came from the user's requested role
/// or the server's admission decision, so a UI that passed a
/// requested role here would mis-label sessions where the server
/// admitted a different role than asked. Per `CodeRabbit` round 1
/// on PR #408, callers must now name the slot explicitly. Per
/// issue #396.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RtlTcpRoleBadge {
    /// Server admitted us as the Controller client — accent
    /// styling in the status bar.
    Controller,
    /// Server admitted us as a Listener — dim styling.
    Listener,
}

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
    /// Label showing cursor frequency and power readout.
    pub cursor_label: gtk4::Label,
    /// `rtl_tcp` role badge — "Controller" (accent color) or
    /// "Listener" (dim) when connected to an `rtl_tcp` server.
    /// Hidden when the source isn't `rtl_tcp` or when the
    /// connection isn't in `Connected` state. Per issue #396.
    pub role_label: gtk4::Label,
    /// Separator widget packed immediately before `role_label`.
    /// Kept as a field so visibility can be toggled in lockstep
    /// with the label (hiding the label alone leaves a stray
    /// separator on the right edge).
    pub role_separator: gtk4::Separator,
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

    /// Update the `rtl_tcp` role badge. `Some(RtlTcpRoleBadge::
    /// Controller)` shows "Controller" with accent CSS; `Some(
    /// RtlTcpRoleBadge::Listener)` shows "Listener" with dim
    /// CSS; `None` hides the badge + its separator entirely.
    /// Callers must pass the server's admitted role (not the
    /// user's requested role) — see [`RtlTcpRoleBadge`]. Per
    /// issue #396 / `CodeRabbit` round 1 on PR #408.
    pub fn update_role(&self, role: Option<RtlTcpRoleBadge>) {
        match role {
            Some(RtlTcpRoleBadge::Controller) => {
                self.role_label.set_label("Controller");
                self.role_label.remove_css_class("dim-label");
                self.role_label.add_css_class("accent");
                self.role_label.set_visible(true);
                self.role_separator.set_visible(true);
            }
            Some(RtlTcpRoleBadge::Listener) => {
                self.role_label.set_label("Listener");
                self.role_label.remove_css_class("accent");
                self.role_label.add_css_class("dim-label");
                self.role_label.set_visible(true);
                self.role_separator.set_visible(true);
            }
            None => {
                self.role_label.set_visible(false);
                self.role_separator.set_visible(false);
            }
        }
    }

    /// Update the cursor readout with frequency and power at the mouse position.
    ///
    /// When `power_db` is `f32::NEG_INFINITY`, the cursor has left the area
    /// and the readout is cleared.
    pub fn update_cursor(&self, freq_hz: f64, power_db: f32) {
        if power_db == f32::NEG_INFINITY {
            self.cursor_label.set_label(DEFAULT_CURSOR_TEXT);
        } else {
            self.cursor_label.set_label(&format!(
                "Cursor: {} / {power_db:.1} dB",
                format_frequency(freq_hz)
            ));
        }
    }
}

/// Build the status bar widget with initial placeholder labels.
pub fn build_status_bar() -> StatusBar {
    let signal_level_label = gtk4::Label::new(Some(DEFAULT_LEVEL_TEXT));
    let sample_rate_label = gtk4::Label::new(Some(DEFAULT_SAMPLE_RATE_TEXT));
    let demod_label = gtk4::Label::new(Some(DEFAULT_DEMOD_TEXT));
    let frequency_label = gtk4::Label::new(Some(DEFAULT_FREQUENCY_TEXT));
    let cursor_label = gtk4::Label::new(Some(DEFAULT_CURSOR_TEXT));
    let role_label = gtk4::Label::new(Some(DEFAULT_ROLE_TEXT));
    role_label.set_visible(false);
    let role_separator = gtk4::Separator::new(gtk4::Orientation::Vertical);
    role_separator.set_visible(false);

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
    widget.append(&gtk4::Separator::new(gtk4::Orientation::Vertical));
    widget.append(&cursor_label);
    widget.append(&role_separator);
    widget.append(&role_label);

    StatusBar {
        widget,
        signal_level_label,
        sample_rate_label,
        demod_label,
        frequency_label,
        cursor_label,
        role_label,
        role_separator,
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
