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
/// Default antenna-dimension text when no frequency is set yet or
/// the tuned frequency is below the renderable floor. Mirrors the
/// other `-- unit` placeholders (`Level: -- dBFS`, `SR: --`) so the
/// feature is discoverable in the status bar from app launch, not
/// hidden until the first tune. Per issue #157 — post-smoke-test
/// tweak to drop the start-hidden behaviour (the user couldn't
/// find "a way to activate" the calculator because there was no
/// indication it existed until the freq fell in range).
const DEFAULT_ANTENNA_TEXT: &str = "λ/2 -- · λ/4 --";
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
    /// Label showing half-wave + quarter-wave antenna dimensions for
    /// the current frequency. Always visible (falls back to the
    /// `DEFAULT_ANTENNA_TEXT` placeholder for frequencies below
    /// [`crate::antenna::MIN_RENDERABLE_FREQUENCY_HZ`]) so the
    /// feature is always discoverable in the status bar. Per
    /// issue #157.
    pub antenna_label: gtk4::Label,
    /// Separator packed immediately before [`antenna_label`]. Always
    /// visible alongside the label.
    pub antenna_separator: gtk4::Separator,
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

    /// Update the center frequency display. Also refreshes the
    /// companion antenna-dimension label (half-wave + quarter-wave
    /// arm length) for the same frequency so the builder value
    /// stays in sync with the tuned band. Falls back to the
    /// `DEFAULT_ANTENNA_TEXT` placeholder when the frequency is
    /// below the renderable floor (see
    /// [`crate::antenna::MIN_RENDERABLE_FREQUENCY_HZ`]); the label
    /// itself stays visible either way so the feature is always
    /// discoverable.
    pub fn update_frequency(&self, hz: f64) {
        self.frequency_label.set_label(&format_frequency(hz));
        // Matched rather than `.unwrap_or_else(|| .to_string())`
        // so the no-render branch passes the `&'static str`
        // `DEFAULT_ANTENNA_TEXT` directly — no `String`
        // allocation per update. Matters on VFO-drag retune
        // storms where this fires at GTK mouse-event cadence.
        // Per `CodeRabbit` round 1 on PR #418.
        if let Some(antenna_text) = crate::antenna::format_antenna_line(hz) {
            self.antenna_label.set_label(&antenna_text);
        } else {
            self.antenna_label.set_label(DEFAULT_ANTENNA_TEXT);
        }
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
    // Ellipsize every label so a crowded status bar (long frequency
    // + long antenna readout + live cursor readout + role badge)
    // can shrink gracefully below its natural width when the
    // sidebars pinch content-area room. Without this, the status
    // bar demands its full natural width from its parent, which
    // with both sidebars open leaves the content area in a
    // renegotiation loop with the split view — the visible symptom
    // is a few-pixel layout "bounce" every time a label updates.
    // `EllipsizeMode::End` puts "…" at the end of the trimmed text;
    // combined with not setting `max_width_chars`, GTK picks the
    // widest fit per frame and shrinks only when needed.
    let make_label = |text: &str| -> gtk4::Label {
        let label = gtk4::Label::new(Some(text));
        label.set_ellipsize(gtk4::pango::EllipsizeMode::End);
        label.set_xalign(0.0);
        label
    };

    let signal_level_label = make_label(DEFAULT_LEVEL_TEXT);
    let sample_rate_label = make_label(DEFAULT_SAMPLE_RATE_TEXT);
    let demod_label = make_label(DEFAULT_DEMOD_TEXT);
    let frequency_label = make_label(DEFAULT_FREQUENCY_TEXT);
    let antenna_label = make_label(DEFAULT_ANTENNA_TEXT);
    let antenna_separator = gtk4::Separator::new(gtk4::Orientation::Vertical);
    let cursor_label = make_label(DEFAULT_CURSOR_TEXT);
    let role_label = make_label(DEFAULT_ROLE_TEXT);
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
    // Antenna-dimension label packs immediately after the
    // frequency label so builders reading the screen can scan
    // right across the pair: "146.52 MHz · λ/2 1.02 m · λ/4 51.2 cm"
    // Per issue #157.
    widget.append(&antenna_separator);
    widget.append(&antenna_label);
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
        antenna_label,
        antenna_separator,
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
