//! Widget construction for the Radio panel —
//! [`build_radio_panel`] lays out the Bandwidth / Squelch /
//! Filters / De-emphasis / CTCSS / Distance Estimator groups and
//! wires the build-time distance-refresh signals. Split out of
//! `radio_panel.rs` per the file-size pass (issue #819).

use libadwaita as adw;
use libadwaita::prelude::*;
use sdr_dsp::tone_detect::{CTCSS_DEFAULT_THRESHOLD, CTCSS_TONES_HZ};
use sdr_dsp::voice_squelch::{
    VOICE_SQUELCH_SNR_DEFAULT_THRESHOLD_DB, VOICE_SQUELCH_SYLLABIC_DEFAULT_THRESHOLD,
};

use super::{
    BANDWIDTH_PAGE_HZ, BANDWIDTH_STEP_HZ, CTCSS_THRESHOLD_PAGE, CTCSS_THRESHOLD_STEP,
    DEEMPHASIS_MODEL_LEN, DEFAULT_BANDWIDTH_HZ, DEFAULT_CTCSS_THRESHOLD, DEFAULT_NB_LEVEL,
    DEFAULT_NOTCH_FREQ_HZ, DEFAULT_SQUELCH_DB, MAX_BANDWIDTH_HZ, MAX_CTCSS_THRESHOLD, MAX_NB_LEVEL,
    MAX_NOTCH_FREQ_HZ, MAX_SQUELCH_DB, MIN_BANDWIDTH_HZ, MIN_CTCSS_THRESHOLD, MIN_NB_LEVEL,
    MIN_NOTCH_FREQ_HZ, MIN_SQUELCH_DB, NB_LEVEL_PAGE, NB_LEVEL_STEP, NOTCH_FREQ_PAGE_HZ,
    NOTCH_FREQ_STEP_HZ, RadioPanel, SNR_THRESHOLD_DB_MAX, SNR_THRESHOLD_DB_MIN, SQUELCH_PAGE_DB,
    SQUELCH_STEP_DB, SYLLABIC_THRESHOLD_MAX, SYLLABIC_THRESHOLD_MIN, SYLLABIC_THRESHOLD_PAGE,
    SYLLABIC_THRESHOLD_STEP, VOICE_SQUELCH_MODE_LABELS,
};

mod section_distance;
use section_distance::{assemble_page, build_distance_section};

/// Build the radio / demodulator configuration panel.
///
/// Lays out as an `AdwPreferencesPage` with five titled sections —
/// Bandwidth / Squelch / Filters / De-emphasis / CTCSS — matching
/// the activity-bar redesign's Apple-style section rhythm (design
/// doc §3.2). Section groups are flat rather than `AdwExpanderRow`
/// — same call as the General panel: the expander-row inset +
/// group-title inset stacked to a double-indent that read cluttered
/// once sections were populated, so we pin "expanded by default"
/// into "always visible" and give the user scroll instead of
/// collapse as the focus affordance.
pub fn build_radio_panel() -> RadioPanel {
    // Row construction happens per section below; the page/group
    // ASSEMBLY (and therefore the user-visible row order) is
    // unchanged from the pre-split monolith — groups are packed in
    // `assemble_radio_page` / `assemble_page` in the original
    // sequence.
    let (bandwidth_row, bandwidth_reset_button) = build_bandwidth_rows();
    let squelch = build_squelch_rows();
    let deemphasis_row = build_deemphasis_row();
    let filters = build_filter_rows();
    let ctcss = build_ctcss_rows();
    let voice = build_voice_squelch_rows();
    let distance = build_distance_section();

    let (bandwidth_group, squelch_group, filters_group, deemphasis_group, ctcss_group) =
        assemble_radio_page(
            &bandwidth_row,
            &squelch,
            &voice,
            &filters,
            &deemphasis_row,
            &ctcss,
        );

    let page = assemble_page(
        &bandwidth_group,
        &squelch_group,
        &filters_group,
        &deemphasis_group,
        &ctcss_group,
        &distance,
    );

    // All rows connected to DSP pipeline via window.rs

    RadioPanel {
        widget: page,
        deemphasis_group,
        ctcss_group,
        bandwidth_row,
        bandwidth_reset_button,
        squelch_enabled_row: squelch.squelch_enabled_row,
        squelch_level_row: squelch.squelch_level_row,
        auto_squelch_row: squelch.auto_squelch_row,
        deemphasis_row,
        noise_blanker_row: filters.noise_blanker_row,
        nb_level_row: filters.nb_level_row,
        fm_if_nr_row: filters.fm_if_nr_row,
        stereo_row: filters.stereo_row,
        notch_enabled_row: filters.notch_enabled_row,
        notch_freq_row: filters.notch_freq_row,
        ctcss_row: ctcss.ctcss_row,
        ctcss_threshold_row: ctcss.ctcss_threshold_row,
        ctcss_status_row: ctcss.ctcss_status_row,
        voice_squelch_row: voice.voice_squelch_row,
        voice_squelch_threshold_row: voice.voice_squelch_threshold_row,
        voice_squelch_status_row: voice.voice_squelch_status_row,
        erp_row: distance.erp_row,
        calibration_row: distance.calibration_row,
        distance_row: distance.distance_row,
        distance_last_signal_db: distance.last_signal_db,
        distance_last_frequency_hz: distance.last_frequency_hz,
    }
}

/// Bandwidth spin row + its reset-to-mode-default suffix button.
/// Split out of [`build_radio_panel`] per the 50-NLOC gate (#819).
fn build_bandwidth_rows() -> (adw::SpinRow, gtk4::Button) {
    // --- Bandwidth ---
    let bandwidth_adj = gtk4::Adjustment::new(
        DEFAULT_BANDWIDTH_HZ,
        MIN_BANDWIDTH_HZ,
        MAX_BANDWIDTH_HZ,
        BANDWIDTH_STEP_HZ,
        BANDWIDTH_PAGE_HZ,
        0.0,
    );
    let bandwidth_row = adw::SpinRow::builder()
        .title("Bandwidth")
        .subtitle("Hz")
        .adjustment(&bandwidth_adj)
        .digits(0)
        .build();

    // "Reset bandwidth to default for current demod mode" —
    // packed as a suffix so it sits inline with the spin row.
    // Flat + valign(Center) matches the affordance pattern other
    // sidebar rows use for secondary actions.
    let bandwidth_reset_button = gtk4::Button::builder()
        .icon_name("edit-undo-symbolic")
        .tooltip_text("Reset bandwidth to default for current demod mode")
        .css_classes(["flat"])
        .valign(gtk4::Align::Center)
        // Start insensitive — the initial bandwidth is the
        // mode default. The value-notify + DemodModeChanged
        // handlers in window.rs update sensitivity from here.
        .sensitive(false)
        .build();
    bandwidth_reset_button.update_property(&[gtk4::accessible::Property::Label(
        "Reset bandwidth to default",
    )]);
    bandwidth_row.add_suffix(&bandwidth_reset_button);

    (bandwidth_row, bandwidth_reset_button)
}

/// The Squelch section's three rows, named per the bundle
/// convention (#819).
#[allow(
    clippy::struct_field_names,
    reason = "field names deliberately mirror the RadioPanel fields they \
              feed so the orchestrator's struct literal moves them 1:1 \
              (same call as PR #886's SharedRows)"
)]
struct SquelchRows {
    squelch_enabled_row: adw::SwitchRow,
    squelch_level_row: adw::SpinRow,
    auto_squelch_row: adw::SwitchRow,
}

/// Squelch enable / level / auto-squelch rows. Split out per the
/// 50-NLOC gate (#819).
fn build_squelch_rows() -> SquelchRows {
    // --- Squelch ---
    let squelch_enabled_row = adw::SwitchRow::builder().title("Squelch").build();

    let squelch_adj = gtk4::Adjustment::new(
        DEFAULT_SQUELCH_DB,
        MIN_SQUELCH_DB,
        MAX_SQUELCH_DB,
        SQUELCH_STEP_DB,
        SQUELCH_PAGE_DB,
        0.0,
    );
    let squelch_level_row = adw::SpinRow::builder()
        .title("Squelch Level")
        .subtitle("dB")
        .adjustment(&squelch_adj)
        .digits(0)
        .build();

    // --- Auto-squelch ---
    let auto_squelch_row = adw::SwitchRow::builder()
        .title("Auto Squelch")
        .subtitle("Track noise floor automatically")
        .build();

    SquelchRows {
        squelch_enabled_row,
        squelch_level_row,
        auto_squelch_row,
    }
}

/// De-emphasis selector row. Split out per the 50-NLOC gate (#819).
fn build_deemphasis_row() -> adw::ComboRow {
    // --- De-emphasis ---
    let deemphasis_model =
        gtk4::StringList::new(&["None", "50 \u{00b5}s (EU)", "75 \u{00b5}s (US)"]);
    debug_assert_eq!(deemphasis_model.n_items(), DEEMPHASIS_MODEL_LEN);
    adw::ComboRow::builder()
        .title("De-emphasis")
        .model(&deemphasis_model)
        .build()
}

/// The six Filters-group rows, named so the orchestrator moves
/// them by field instead of positionally. Split out per the
/// 50-NLOC gate (#819).
#[allow(
    clippy::struct_field_names,
    reason = "field names deliberately mirror the RadioPanel fields they \
              feed so the orchestrator's struct literal moves them 1:1 \
              (same call as PR #886's SharedRows)"
)]
struct FilterRows {
    noise_blanker_row: adw::SwitchRow,
    nb_level_row: adw::SpinRow,
    fm_if_nr_row: adw::SwitchRow,
    stereo_row: adw::SwitchRow,
    notch_enabled_row: adw::SwitchRow,
    notch_freq_row: adw::SpinRow,
}

/// Noise blanker / FM IF NR / stereo / notch rows. Split out per
/// the 50-NLOC gate (#819).
fn build_filter_rows() -> FilterRows {
    // --- Noise Blanker ---
    let noise_blanker_row = adw::SwitchRow::builder().title("Noise Blanker").build();

    // --- Noise Blanker Level ---
    let nb_level_adj = gtk4::Adjustment::new(
        DEFAULT_NB_LEVEL,
        MIN_NB_LEVEL,
        MAX_NB_LEVEL,
        NB_LEVEL_STEP,
        NB_LEVEL_PAGE,
        0.0,
    );
    let nb_level_row = adw::SpinRow::builder()
        .title("NB Level")
        .subtitle("Threshold multiplier")
        .adjustment(&nb_level_adj)
        .digits(1)
        .build();

    // --- FM IF Noise Reduction ---
    let fm_if_nr_row = adw::SwitchRow::builder()
        .title("FM IF NR")
        .subtitle("IF noise reduction for FM modes")
        .build();

    // --- WFM Stereo ---
    let stereo_row = adw::SwitchRow::builder()
        .title("Stereo")
        .subtitle("WFM stereo decode")
        .visible(false) // Only shown in WFM mode
        .build();

    let (notch_enabled_row, notch_freq_row) = build_notch_rows();

    FilterRows {
        noise_blanker_row,
        nb_level_row,
        fm_if_nr_row,
        stereo_row,
        notch_enabled_row,
        notch_freq_row,
    }
}

/// CTCSS combo / threshold / status rows (with the UI-vs-DSP
/// default-threshold debug assert). Split out per the 50-NLOC
/// gate (#819).
/// Notch enable + frequency rows. Split out of
/// [`build_filter_rows`] per the 50-NLOC gate (#819).
fn build_notch_rows() -> (adw::SwitchRow, adw::SpinRow) {
    // --- Notch Filter ---
    let notch_enabled_row = adw::SwitchRow::builder()
        .title("Notch Filter")
        .subtitle("Remove interference tones")
        .build();

    let notch_freq_adj = gtk4::Adjustment::new(
        DEFAULT_NOTCH_FREQ_HZ,
        MIN_NOTCH_FREQ_HZ,
        MAX_NOTCH_FREQ_HZ,
        NOTCH_FREQ_STEP_HZ,
        NOTCH_FREQ_PAGE_HZ,
        0.0,
    );
    let notch_freq_row = adw::SpinRow::builder()
        .title("Notch Frequency")
        .subtitle("Hz")
        .adjustment(&notch_freq_adj)
        .digits(0)
        .build();

    (notch_enabled_row, notch_freq_row)
}

/// The CTCSS section's three rows, named per the bundle
/// convention (#819).
#[allow(
    clippy::struct_field_names,
    reason = "field names deliberately mirror the RadioPanel fields they \
              feed so the orchestrator's struct literal moves them 1:1 \
              (same call as PR #886's SharedRows)"
)]
struct CtcssRows {
    ctcss_row: adw::ComboRow,
    ctcss_threshold_row: adw::SpinRow,
    ctcss_status_row: adw::ActionRow,
}

fn build_ctcss_rows() -> CtcssRows {
    // --- CTCSS tone squelch ---
    // Build the combo model with "Off" followed by the 51 CTCSS
    // tones. Each tone is labelled to one decimal place (matching
    // the hardware convention — e.g. "100.0 Hz", "151.4 Hz").
    let mut ctcss_labels: Vec<String> = Vec::with_capacity(CTCSS_TONES_HZ.len() + 1);
    ctcss_labels.push("Off".to_string());
    for &tone in CTCSS_TONES_HZ {
        ctcss_labels.push(format!("{tone:.1} Hz"));
    }
    let ctcss_label_refs: Vec<&str> = ctcss_labels.iter().map(String::as_str).collect();
    let ctcss_model = gtk4::StringList::new(&ctcss_label_refs);
    let ctcss_row = adw::ComboRow::builder()
        .title("CTCSS Tone Squelch")
        .subtitle("Sub-audible tone required to open squelch")
        .model(&ctcss_model)
        .visible(false) // NFM-only; startup mode sets it
        .build();

    let ctcss_threshold_adj = gtk4::Adjustment::new(
        DEFAULT_CTCSS_THRESHOLD,
        MIN_CTCSS_THRESHOLD,
        MAX_CTCSS_THRESHOLD,
        CTCSS_THRESHOLD_STEP,
        CTCSS_THRESHOLD_PAGE,
        0.0,
    );
    let ctcss_threshold_row = adw::SpinRow::builder()
        .title("CTCSS Threshold")
        .subtitle("Higher = more conservative (fewer false triggers)")
        .adjustment(&ctcss_threshold_adj)
        .digits(2)
        .visible(false)
        .build();
    // Debug assert the default matches the DSP layer at startup —
    // a future bump to CTCSS_DEFAULT_THRESHOLD should be
    // accompanied by a bump to DEFAULT_CTCSS_THRESHOLD so the
    // slider and the detector agree on the un-tuned default.
    debug_assert!(
        (DEFAULT_CTCSS_THRESHOLD - f64::from(CTCSS_DEFAULT_THRESHOLD)).abs() < 1e-6,
        "UI default CTCSS threshold diverged from DSP default"
    );

    let ctcss_status_row = adw::ActionRow::builder()
        .title("CTCSS Status")
        .subtitle("Off")
        .visible(false)
        .build();

    CtcssRows {
        ctcss_row,
        ctcss_threshold_row,
        ctcss_status_row,
    }
}

/// Voice-squelch combo / threshold / status rows (with the
/// DSP-default range debug asserts). Split out per the 50-NLOC
/// gate (#819).
/// The voice-squelch section's three rows, named per the bundle
/// convention (#819).
#[allow(
    clippy::struct_field_names,
    reason = "field names deliberately mirror the RadioPanel fields they \
              feed so the orchestrator's struct literal moves them 1:1 \
              (same call as PR #886's SharedRows)"
)]
struct VoiceSquelchRows {
    voice_squelch_row: adw::ComboRow,
    voice_squelch_threshold_row: adw::SpinRow,
    voice_squelch_status_row: adw::ActionRow,
}

fn build_voice_squelch_rows() -> VoiceSquelchRows {
    // --- Voice squelch ---
    // Three-entry combo: Off / Syllabic / SNR ratio. Threshold
    // spin row + status row start hidden (Off is the default);
    // they're revealed by `apply_voice_squelch_mode_ui` when
    // the combo changes to an active mode.
    let voice_squelch_model = gtk4::StringList::new(VOICE_SQUELCH_MODE_LABELS);
    let voice_squelch_row = adw::ComboRow::builder()
        .title("Voice squelch")
        .subtitle("Speech / signal detector, gates alongside CTCSS")
        .model(&voice_squelch_model)
        // Start hidden — `apply_demod_visibility` reveals it on
        // NFM. Without this the row would flash briefly on the
        // default non-NFM startup path before the visibility
        // handler kicks in, mirroring the CTCSS pattern.
        .visible(false)
        .build();

    // Threshold spin row — starts in syllabic-default range but
    // the adjustment is overwritten by `apply_voice_squelch_mode_ui`
    // whenever the mode changes, so the initial range is just a
    // placeholder.
    let voice_squelch_threshold_adj = gtk4::Adjustment::new(
        f64::from(VOICE_SQUELCH_SYLLABIC_DEFAULT_THRESHOLD),
        SYLLABIC_THRESHOLD_MIN,
        SYLLABIC_THRESHOLD_MAX,
        SYLLABIC_THRESHOLD_STEP,
        SYLLABIC_THRESHOLD_PAGE,
        0.0,
    );
    let voice_squelch_threshold_row = adw::SpinRow::builder()
        .title("Voice squelch threshold")
        .subtitle("Select a mode first")
        .adjustment(&voice_squelch_threshold_adj)
        .digits(2)
        .visible(false)
        .build();

    let voice_squelch_status_row = adw::ActionRow::builder()
        .title("Voice squelch status")
        .subtitle("Off")
        .visible(false)
        .build();

    // Sanity-check that the DSP-layer defaults haven't drifted
    // from the UI's tuning range. If someone bumps the DSP
    // default out of the UI range, this debug_assert forces them
    // to update the UI bounds too.
    debug_assert!(
        f64::from(VOICE_SQUELCH_SYLLABIC_DEFAULT_THRESHOLD) >= SYLLABIC_THRESHOLD_MIN
            && f64::from(VOICE_SQUELCH_SYLLABIC_DEFAULT_THRESHOLD) <= SYLLABIC_THRESHOLD_MAX,
        "syllabic default threshold outside UI range"
    );
    debug_assert!(
        f64::from(VOICE_SQUELCH_SNR_DEFAULT_THRESHOLD_DB) >= SNR_THRESHOLD_DB_MIN
            && f64::from(VOICE_SQUELCH_SNR_DEFAULT_THRESHOLD_DB) <= SNR_THRESHOLD_DB_MAX,
        "SNR default threshold outside UI range"
    );

    VoiceSquelchRows {
        voice_squelch_row,
        voice_squelch_threshold_row,
        voice_squelch_status_row,
    }
}

fn assemble_radio_page(
    bandwidth_row: &adw::SpinRow,
    squelch: &SquelchRows,
    voice: &VoiceSquelchRows,
    filters: &FilterRows,
    deemphasis_row: &adw::ComboRow,
    ctcss: &CtcssRows,
) -> (
    adw::PreferencesGroup,
    adw::PreferencesGroup,
    adw::PreferencesGroup,
    adw::PreferencesGroup,
    adw::PreferencesGroup,
) {
    // --- Sectioned preferences page ---
    //
    // Individual row-level `.visible(false)` flags set at
    // construction above (stereo, CTCSS rows, voice-squelch rows)
    // are preserved — they keep the startup state correct before
    // `apply_demod_visibility` runs, and the group-level hide in
    // `apply_demod_visibility` is a second line of defence for
    // screen-reader users.
    // Section groups — `title` + `description` pattern mirrors the
    // other panels (Audio / Display / Source / etc.) so header
    // spacing + typography stays consistent across activities.
    // Descriptions double as plain-English hints for users new to
    // SDR jargon.
    let bandwidth_group = adw::PreferencesGroup::builder()
        .title("Bandwidth")
        .description("Filter width around the tuned frequency")
        .build();
    bandwidth_group.add(bandwidth_row);

    let squelch_group = assemble_squelch_group(squelch, voice);
    let filters_group = assemble_filters_group(filters);
    let deemphasis_group = adw::PreferencesGroup::builder()
        .title("De-emphasis")
        .description("Restore high-frequency audio on FM")
        .build();
    deemphasis_group.add(deemphasis_row);

    let ctcss_group = adw::PreferencesGroup::builder()
        .title("CTCSS")
        .description("Open audio only when a matching tone is present")
        .build();
    ctcss_group.add(&ctcss.ctcss_row);
    ctcss_group.add(&ctcss.ctcss_threshold_row);
    ctcss_group.add(&ctcss.ctcss_status_row);

    (
        bandwidth_group,
        squelch_group,
        filters_group,
        deemphasis_group,
        ctcss_group,
    )
}

/// Squelch group packing — squelch rows first, then the voice-
/// squelch trio, in the pre-split order. Split out per the
/// 50-NLOC gate (#819).
fn assemble_squelch_group(
    squelch: &SquelchRows,
    voice: &VoiceSquelchRows,
) -> adw::PreferencesGroup {
    let squelch_group = adw::PreferencesGroup::builder()
        .title("Squelch")
        .description("Mute audio when the signal is too weak")
        .build();
    squelch_group.add(&squelch.squelch_enabled_row);
    squelch_group.add(&squelch.squelch_level_row);
    squelch_group.add(&squelch.auto_squelch_row);
    squelch_group.add(&voice.voice_squelch_row);
    squelch_group.add(&voice.voice_squelch_threshold_row);
    squelch_group.add(&voice.voice_squelch_status_row);

    squelch_group
}

/// Filters group packing, pre-split row order. Split out per the
/// 50-NLOC gate (#819).
fn assemble_filters_group(filters: &FilterRows) -> adw::PreferencesGroup {
    let filters_group = adw::PreferencesGroup::builder()
        .title("Filters")
        .description("Clean up interference and noise")
        .build();
    filters_group.add(&filters.noise_blanker_row);
    filters_group.add(&filters.nb_level_row);
    filters_group.add(&filters.fm_if_nr_row);
    filters_group.add(&filters.stereo_row);
    filters_group.add(&filters.notch_enabled_row);
    filters_group.add(&filters.notch_freq_row);

    filters_group
}
