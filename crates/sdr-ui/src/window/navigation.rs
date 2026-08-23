//! Navigation panel wiring and bookmark profile restore.

use gtk4::prelude::*;
use libadwaita::prelude::*;

use super::{
    AppState, DeemphasisMode, Rc, ScannerForceDisable, SidebarPanels, StatusBar, UiToDsp, adw,
    header, sidebar, spectrum, tune_to_target,
};

/// Restore optional tuning-profile settings from a bookmark to DSP and UI.
pub(super) fn restore_bookmark_profile(
    bookmark: &sidebar::navigation_panel::Bookmark,
    state: &AppState,
    radio: &sidebar::RadioPanel,
    gain_row: &adw::SpinRow,
    agc_row: &adw::ComboRow,
    volume_button: &gtk4::ScaleButton,
) {
    if let Some(sq_en) = bookmark.squelch_enabled {
        state.send_dsp(UiToDsp::SetSquelchEnabled(sq_en));
        radio.squelch_enabled_row.set_active(sq_en);
    }
    if let Some(auto_sq) = bookmark.auto_squelch_enabled {
        state.send_dsp(UiToDsp::SetAutoSquelch(auto_sq));
        radio.auto_squelch_row.set_active(auto_sq);
    }
    if let Some(sq_lvl) = bookmark.squelch_level {
        state.send_dsp(UiToDsp::SetSquelch(sq_lvl));
        #[allow(clippy::cast_lossless)]
        radio.squelch_level_row.set_value(sq_lvl as f64);
    }
    restore_bookmark_gain_volume(bookmark, state, gain_row, agc_row, volume_button);

    restore_bookmark_af_profile(bookmark, state, radio);
}

/// Connect navigation panel (band presets + bookmarks) to DSP commands.
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
pub(super) fn connect_navigation_panel(
    panels: &SidebarPanels,
    state: &Rc<AppState>,
    freq_selector: &header::frequency_selector::FrequencySelector,
    demod_dropdown: &gtk4::DropDown,
    status_bar: &Rc<StatusBar>,
    spectrum_handle: &Rc<spectrum::SpectrumHandle>,
    scanner_force_disable: &Rc<ScannerForceDisable>,
    volume_button: &gtk4::ScaleButton,
) {
    wire_bookmark_navigation(
        panels,
        state,
        freq_selector,
        demod_dropdown,
        status_bar,
        spectrum_handle,
        scanner_force_disable,
        volume_button,
    );

    wire_add_bookmark_button(panels, state);

    wire_save_bookmark_button(panels, state);
}

/// Bookmark-selected navigation: restore the full tuning profile.
/// Split out per the 50-NLOC gate (#817).
fn wire_bookmark_navigation(
    panels: &SidebarPanels,
    state: &Rc<AppState>,
    freq_selector: &header::frequency_selector::FrequencySelector,
    demod_dropdown: &gtk4::DropDown,
    status_bar: &Rc<StatusBar>,
    spectrum_handle: &Rc<spectrum::SpectrumHandle>,
    scanner_force_disable: &Rc<ScannerForceDisable>,
    volume_button: &gtk4::ScaleButton,
) {
    // Navigation callback: restore full tuning profile from bookmark.
    let state_nav = Rc::clone(state);
    let fs = freq_selector.clone();
    // Strong clone — single-threaded GTK main loop, the closure
    // outlives the dropdown only at teardown which drops both at
    // once. Pre-#509 this was a `WeakRef` upgraded inside the
    // closure; `tune_to_target` takes `&gtk4::DropDown`, and the
    // strong clone keeps the call shape uniform with the satellite
    // closure (which has always held a strong clone).
    let dd = demod_dropdown.clone();
    let sb = Rc::clone(status_bar);
    let spectrum_nav = Rc::clone(spectrum_handle);
    let radio_nav = panels.radio.clone();
    let source_nav_gain = panels.source.gain_row.clone();
    let source_nav_agc = panels.source.agc_row.clone();
    let force_disable_nav = Rc::clone(scanner_force_disable);
    let volume_button_nav = volume_button.clone();
    let bandwidth_row_nav = panels.radio.bandwidth_row.clone();

    panels.bookmarks.connect_navigate(move |bookmark| {
        navigate_to_bookmark(
            bookmark,
            &state_nav,
            &fs,
            &dd,
            &sb,
            &spectrum_nav,
            &radio_nav,
            &source_nav_gain,
            &source_nav_agc,
            &force_disable_nav,
            &volume_button_nav,
            &bandwidth_row_nav,
        );
    });
}

/// Bookmark/preset navigation body: the canonical 13-step mirror
/// sequence (#509) plus the bookmark-specific profile restore.
/// Split out per the 50-NLOC gate (#817).
#[allow(clippy::too_many_arguments)]
fn navigate_to_bookmark(
    bookmark: &sidebar::navigation_panel::Bookmark,
    state_nav: &Rc<AppState>,
    fs: &header::frequency_selector::FrequencySelector,
    dd: &gtk4::DropDown,
    sb: &Rc<StatusBar>,
    spectrum_nav: &Rc<spectrum::SpectrumHandle>,
    radio_nav: &sidebar::radio_panel::RadioPanel,
    source_nav_gain: &adw::SpinRow,
    source_nav_agc: &adw::ComboRow,
    force_disable_nav: &Rc<ScannerForceDisable>,
    volume_button_nav: &gtk4::ScaleButton,
    bandwidth_row_nav: &adw::SpinRow,
) {
    // Both bookmark recall AND band-preset selection come in
    // through this callback (the preset handler in
    // `connect_preset_to_bookmarks` invokes `on_navigate` with
    // a synthesized Bookmark). Keep the toast reason neutral
    // so a preset click doesn't claim "bookmark recall".
    let freq = bookmark.frequency;
    let mode = sidebar::navigation_panel::parse_demod_mode(&bookmark.demod_mode);
    let bw = bookmark.bandwidth;

    // Canonical 13-step mirror sequence — single source of
    // truth shared with the satellite tune path. Per #509.
    tune_to_target(
        &state_nav,
        &fs,
        &dd,
        &spectrum_nav,
        &force_disable_nav,
        &bandwidth_row_nav,
        &radio_nav,
        &sb,
        freq,
        mode,
        bw,
        "preset/bookmark selection",
    );

    // Restore optional tuning-profile settings (squelch, gain,
    // etc.). Bookmark-specific layer on top of the canonical
    // mirror sequence — auto-record / satellite play don't
    // need this.
    restore_bookmark_profile(
        bookmark,
        &state_nav,
        &radio_nav,
        &source_nav_gain,
        &source_nav_agc,
        &volume_button_nav,
    );

    tracing::info!(
        frequency = freq,
        ?mode,
        bandwidth = bw,
        "navigated to frequency"
    );
}

/// Add Bookmark: capture the full tuning profile from current UI state.
/// Split out per the 50-NLOC gate (#817).
fn wire_add_bookmark_button(panels: &SidebarPanels, state: &Rc<AppState>) {
    let nav = &panels.navigation;
    let bm = &panels.bookmarks;
    // "Add Bookmark" button — capture full tuning profile from current UI state.
    let state_bm = Rc::clone(state);
    let radio_bm = panels.radio.clone();
    let source_gain_bm = panels.source.gain_row.clone();
    let source_agc_bm = panels.source.agc_row.clone();
    let nav = &panels.navigation;
    let bm = &panels.bookmarks;
    let bm_for_add = Rc::clone(bm);
    let name_entry = nav.name_entry.clone();

    nav.add_button.connect_clicked(move |_| {
        let freq = state_bm.center_frequency.get();
        let mode = state_bm.demod_mode.get();
        let bw = radio_bm.bandwidth_row.value();
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let freq_u64 = freq as u64;
        let entered = name_entry.text();
        let name = if entered.is_empty() {
            sidebar::navigation_panel::format_frequency(freq_u64)
        } else {
            entered.to_string()
        };

        // Capture full tuning profile from current UI widget state —
        // same reads as the Save path.
        let profile = capture_tuning_profile(&radio_bm, &source_gain_bm, &source_agc_bm);
        let bookmark =
            sidebar::navigation_panel::Bookmark::with_profile(&name, freq_u64, mode, bw, &profile);
        bm_for_add.bookmarks.borrow_mut().push(bookmark);
        sidebar::navigation_panel::save_bookmarks(&bm_for_add.bookmarks.borrow());
        bm_for_add.rebuild_after_mutation(&name_entry);
        name_entry.set_text("");
    });
}

/// Save: update the active bookmark with current settings.
/// Split out per the 50-NLOC gate (#817).
fn wire_save_bookmark_button(panels: &SidebarPanels, state: &Rc<AppState>) {
    let nav = &panels.navigation;
    let bm = &panels.bookmarks;
    // Save button — update the active bookmark with current settings.
    // Capture the bookmarks panel via `Weak` so the stored closure
    // doesn't keep the panel alive: the closure lives inside
    // `panel.on_save`, and cloning `Rc<BookmarksPanel>` into it
    // would form a cycle (panel → on_save → closure → panel)
    // that prevents the panel from dropping on window teardown.
    let save_bm_weak = std::rc::Rc::downgrade(bm);
    let save_name_entry = nav.name_entry.clone();
    let save_state = Rc::clone(state);
    let save_radio = panels.radio.clone();
    let save_source_gain = panels.source.gain_row.clone();
    let save_source_agc = panels.source.agc_row.clone();
    bm.connect_save(move || {
        // `save_bm_weak` is the ONLY reference this closure holds
        // to the panel. Upgrading on entry gives us a live handle
        // for the duration of the save; dropping it at the end of
        // the call lets the panel drop cleanly on teardown even
        // though the closure itself is stored inside
        // `panel.on_save`.
        let Some(save_bm) = save_bm_weak.upgrade() else {
            return;
        };
        let active = save_bm.active_bookmark.borrow().clone();
        if active.name.is_empty() && active.frequency == 0 {
            return; // No active bookmark to save.
        }
        let freq = save_state.center_frequency.get();
        let mode = save_state.demod_mode.get();
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let freq_u64 = freq as u64;
        let bw = save_radio.bandwidth_row.value();
        let profile = capture_tuning_profile(&save_radio, &save_source_gain, &save_source_agc);
        // Find and update the active bookmark in the list.
        let mut bms = save_bm.bookmarks.borrow_mut();
        if let Some(bm) = bms
            .iter_mut()
            .find(|b| b.name == active.name && b.frequency == active.frequency)
        {
            apply_profile_to_bookmark(bm, freq_u64, mode, bw, &profile);
            // Keep ActiveBookmark in sync with the updated frequency.
            *save_bm.active_bookmark.borrow_mut() = sidebar::navigation_panel::ActiveBookmark {
                name: active.name.clone(),
                frequency: freq_u64,
            };
        }
        sidebar::navigation_panel::save_bookmarks(&bms);
        drop(bms);
        // Rebuild to update subtitle. Fires `on_mutated` so the
        // scanner re-projects — Save can change `scan_enabled` /
        // `priority` / override fields on the bookmark.
        save_bm.rebuild_after_mutation(&save_name_entry);
        tracing::info!("bookmark saved: {}", active.name);
    });
}

/// Copy the captured profile + tune into an existing bookmark's
/// fields on Save. Split out per the 50-NLOC gate (#817).
fn apply_profile_to_bookmark(
    bm: &mut sidebar::navigation_panel::Bookmark,
    freq_u64: u64,
    mode: sdr_types::DemodMode,
    bw: f64,
    profile: &sidebar::navigation_panel::TuningProfile,
) {
    bm.frequency = freq_u64;
    bm.demod_mode = sidebar::navigation_panel::demod_mode_to_string(mode);
    bm.bandwidth = bw;
    bm.squelch_enabled = Some(profile.squelch_enabled);
    bm.auto_squelch_enabled = Some(profile.auto_squelch_enabled);
    bm.squelch_level = Some(profile.squelch_level);
    bm.gain = Some(profile.gain);
    // Legacy-compatible AGC save: write both the new
    // `agc_type` AND the legacy `agc: Option<bool>` so
    // a post-#354 bookmark still round-trips through
    // older builds. Software AGC maps to `false` on the
    // legacy path (safer than `true` since hardware AGC
    // is the documented-problem path in #332).
    bm.agc = Some(matches!(
        profile.agc_type,
        sidebar::source_panel::AgcType::Hardware
    ));
    bm.agc_type = Some(profile.agc_type);
    bm.volume = profile.volume;
    bm.deemphasis = Some(profile.deemphasis);
    bm.nb_enabled = Some(profile.nb_enabled);
    bm.nb_level = Some(profile.nb_level);
    bm.fm_if_nr = Some(profile.fm_if_nr);
    bm.wfm_stereo = Some(profile.wfm_stereo);
    bm.high_pass = profile.high_pass;
    bm.ctcss_mode = profile.ctcss_mode;
    bm.ctcss_threshold = profile.ctcss_threshold;
    bm.voice_squelch_mode = profile.voice_squelch_mode;
}

/// Capture the current Radio-panel + source gain/AGC widget state as
/// a `TuningProfile` for the bookmark Save path. Split out per the
/// 50-NLOC gate (#817).
fn capture_tuning_profile(
    radio: &sidebar::radio_panel::RadioPanel,
    gain_row: &adw::SpinRow,
    agc_row: &adw::ComboRow,
) -> sidebar::navigation_panel::TuningProfile {
    sidebar::navigation_panel::TuningProfile {
        squelch_enabled: radio.squelch_enabled_row.is_active(),
        auto_squelch_enabled: radio.auto_squelch_row.is_active(),
        #[allow(clippy::cast_possible_truncation)]
        squelch_level: radio.squelch_level_row.value() as f32,
        gain: gain_row.value(),
        // Same transient-index fallback as the new-bookmark
        // path above — user-initiated save always produces
        // a bookmark.
        agc_type: sidebar::source_panel::agc_type_from_selected(agc_row.selected())
            .unwrap_or(sidebar::source_panel::AgcType::DEFAULT),
        volume: None,
        deemphasis: radio.deemphasis_row.selected(),
        nb_enabled: radio.noise_blanker_row.is_active(),
        #[allow(clippy::cast_possible_truncation)]
        nb_level: radio.nb_level_row.value() as f32,
        fm_if_nr: radio.fm_if_nr_row.is_active(),
        wfm_stereo: radio.stereo_row.is_active(),
        high_pass: None,
        ctcss_mode: Some(sidebar::radio_panel::RadioPanel::ctcss_mode_from_index(
            radio.ctcss_row.selected(),
        )),
        #[allow(clippy::cast_possible_truncation)]
        ctcss_threshold: Some(radio.ctcss_threshold_row.value() as f32),
        voice_squelch_mode: Some({
            #[allow(clippy::cast_possible_truncation)]
            let t = radio.voice_squelch_threshold_row.value() as f32;
            sidebar::radio_panel::RadioPanel::voice_squelch_mode_from_index(
                radio.voice_squelch_row.selected(),
                t,
            )
        }),
    }
}

/// AGC/gain/volume portion of the bookmark profile restore (AGC before gain so manual mode lands first).
/// Split out per the 50-NLOC gate (#817).
fn restore_bookmark_gain_volume(
    bookmark: &sidebar::navigation_panel::Bookmark,
    state: &AppState,
    gain_row: &adw::SpinRow,
    agc_row: &adw::ComboRow,
    volume_button: &gtk4::ScaleButton,
) {
    // AGC must be set before gain — switching to manual mode first
    // ensures the saved gain value actually takes effect.
    //
    // New bookmarks carry `agc_type` directly; older ones only
    // have the legacy `agc: Option<bool>` field, which we map to
    // `Hardware` (true) or `Off` (false). The new field wins
    // when both are present. The notify handler on `agc_row`
    // dispatches the right `SetAgc` / `SetSoftwareAgc` pair and
    // applies the mutexes, so we only need to flip the combo
    // selector — no explicit dispatch here.
    let restored_agc_type: Option<sidebar::source_panel::AgcType> =
        bookmark.agc_type.or_else(|| {
            bookmark.agc.map(|on| {
                if on {
                    sidebar::source_panel::AgcType::Hardware
                } else {
                    sidebar::source_panel::AgcType::Off
                }
            })
        });
    if let Some(agc_type) = restored_agc_type {
        agc_row.set_selected(sidebar::source_panel::selected_from_agc_type(agc_type));
    }
    if let Some(gain) = bookmark.gain {
        // `set_value` fires the gain row's `connect_value_notify`
        // handler, which dispatches `SetGain` to the DSP — but
        // only when AGC is currently Off (the handler checks the
        // combo state and short-circuits otherwise). So a single
        // `set_value` call here handles both the "AGC is Off,
        // update the DSP too" path and the "AGC is active, just
        // display the bookmarked value in the locked row" path.
        // No explicit `state.send_dsp(SetGain(...))` needed — it
        // would either duplicate the handler's dispatch (AGC Off
        // case) or be a wasted write the DSP silently ignores
        // (AGC active case).
        gain_row.set_value(gain);
    }
    if let Some(vol) = bookmark.volume {
        // Route through the header `ScaleButton` so the restored
        // level flows through the single source of truth
        // `connect_volume_persistence` established: the button's
        // `value_changed` handler dispatches `SetVolume`, writes
        // `KEY_AUDIO_VOLUME`, and mirrors into the audio panel's
        // `volume_row`. Calling `send_dsp(SetVolume(vol))` directly
        // here would leave the button + audio row + persisted key
        // showing stale state until the next user edit flicked
        // them back. `set_value` fires the handler only if the new
        // value differs from the current one — same idempotency
        // story as the gain row above.
        #[allow(clippy::cast_lossless)]
        volume_button.set_value(vol as f64);
    }
}

/// Deemphasis / NB / notch / CTCSS / voice-squelch portion of the bookmark restore.
/// Split out per the 50-NLOC gate (#817).
fn restore_bookmark_af_profile(
    bookmark: &sidebar::navigation_panel::Bookmark,
    state: &AppState,
    radio: &sidebar::RadioPanel,
) {
    if let Some(de_idx) = bookmark.deemphasis {
        let deemp = match de_idx {
            1 => DeemphasisMode::Eu50,
            2 => DeemphasisMode::Us75,
            _ => DeemphasisMode::None,
        };
        state.send_dsp(UiToDsp::SetDeemphasis(deemp));
        radio.deemphasis_row.set_selected(de_idx);
    }
    if let Some(nb_en) = bookmark.nb_enabled {
        state.send_dsp(UiToDsp::SetNbEnabled(nb_en));
        radio.noise_blanker_row.set_active(nb_en);
    }
    if let Some(nb_lvl) = bookmark.nb_level {
        state.send_dsp(UiToDsp::SetNbLevel(nb_lvl));
        #[allow(clippy::cast_lossless)]
        radio.nb_level_row.set_value(nb_lvl as f64);
    }
    if let Some(fm_nr) = bookmark.fm_if_nr {
        state.send_dsp(UiToDsp::SetFmIfNrEnabled(fm_nr));
        radio.fm_if_nr_row.set_active(fm_nr);
    }
    if let Some(stereo) = bookmark.wfm_stereo {
        state.send_dsp(UiToDsp::SetWfmStereo(stereo));
        radio.stereo_row.set_active(stereo);
    }
    if let Some(hp) = bookmark.high_pass {
        state.send_dsp(UiToDsp::SetHighPass(hp));
    }
    // Restore CTCSS threshold BEFORE mode so the detector the
    // mode setter builds picks up the saved value instead of
    // defaulting. Mirrors the RadioModule::set_mode order.
    if let Some(threshold) = bookmark.ctcss_threshold {
        state.send_dsp(UiToDsp::SetCtcssThreshold(threshold));
        #[allow(clippy::cast_lossless)]
        radio.ctcss_threshold_row.set_value(threshold as f64);
    }
    if let Some(mode) = bookmark.ctcss_mode {
        state.send_dsp(UiToDsp::SetCtcssMode(mode));
        radio
            .ctcss_row
            .set_selected(sidebar::radio_panel::RadioPanel::ctcss_index_from_mode(
                mode,
            ));
    }
    restore_bookmark_voice_squelch(bookmark, state, radio);
}

/// Voice-squelch portion of the bookmark restore: dispatch to DSP
/// first, then sync the combo + threshold row. Split out per the
/// 50-NLOC gate (#817).
fn restore_bookmark_voice_squelch(
    bookmark: &sidebar::navigation_panel::Bookmark,
    state: &AppState,
    radio: &sidebar::radio_panel::RadioPanel,
) {
    // Voice squelch mode — the enum carries its threshold
    // inline, so a single field captures both. Dispatch to the
    // DSP first, then update the UI combo + threshold row to
    // reflect the restored state.
    if let Some(mode) = bookmark.voice_squelch_mode {
        state.send_dsp(UiToDsp::SetVoiceSquelchMode(mode));
        let idx = sidebar::radio_panel::RadioPanel::voice_squelch_index_from_mode(mode);
        radio.voice_squelch_row.set_selected(idx);
        let threshold = sidebar::radio_panel::RadioPanel::voice_squelch_threshold_from_mode(mode);
        #[allow(clippy::cast_lossless)]
        radio
            .voice_squelch_threshold_row
            .set_value(threshold as f64);
        // Push the threshold over the wire explicitly too —
        // `SetVoiceSquelchMode` already carries it inline on an
        // active variant, but sending the dedicated threshold
        // message keeps the radio module's cached mode variant
        // in sync in case a future refactor routes the two
        // updates through different code paths.
        state.send_dsp(UiToDsp::SetVoiceSquelchThreshold(threshold));
        radio.apply_voice_squelch_mode_ui(mode);
    }
}
