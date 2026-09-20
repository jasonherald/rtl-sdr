use super::*;
use sdr_sat::DailyWindow;

fn ctx(enabled: bool, present: bool, wefax: WefaxState) -> TickCtx {
    TickCtx {
        enabled,
        choice: StationChoice::Auto,
        user_lat: 30.0,
        user_lon: -90.0,
        now_min: 0, // ticks are sample-free; monotone counter
        wefax_state: wefax,
        fax_present: present,
        saved_tune: SavedTune::default(),
    }
}

/// Test-only station with a narrow broadcast window, for exercising
/// schedule prioritization independent of the real (all-continuous)
/// built-in catalog.
const TEST_ACTIVE_WINDOW: WefaxStation = WefaxStation {
    name: "TEST-ACTIVE-WINDOW",
    channels_hz: &[5_000_000],
    lat_deg: 0.0,
    lon_deg: 0.0,
    schedule: &[DailyWindow {
        start_min_utc: 0,
        end_min_utc: 360,
    }],
};

const TEST_INACTIVE_WINDOW: WefaxStation = WefaxStation {
    name: "TEST-INACTIVE-WINDOW",
    channels_hz: &[6_000_000],
    lat_deg: 0.0,
    lon_deg: 0.0,
    schedule: &[DailyWindow {
        start_min_utc: 500,
        end_min_utc: 600,
    }],
};

#[test]
fn enable_snapshots_tune_and_starts_scanning() {
    let mut c = WefaxCatcher::new();
    let acts = c.tick(ctx(true, false, WefaxState::Idle));
    let freq = match c.state() {
        CatcherState::Scanning {
            candidates, idx, ..
        } => candidates[*idx].freq_hz,
        other => panic!("expected Scanning, got {other:?}"),
    };
    // Exact order/content: ResetDecoder, Tune, SetDemodMode — Task 6's
    // interpret layer relies on this order.
    assert_eq!(
        acts,
        vec![
            Action::ResetDecoder,
            Action::Tune(freq),
            Action::SetDemodMode
        ]
    );
}

#[test]
fn presence_on_a_channel_locks_it() {
    let mut c = WefaxCatcher::new();
    c.tick(ctx(true, false, WefaxState::Idle)); // -> Scanning
    // let the dwell elapse with no signal would advance; now signal appears
    let _ = c.tick(ctx(true, true, WefaxState::Idle));
    assert!(matches!(c.state(), CatcherState::Locked { .. }));
}

#[test]
fn locked_transitions_to_imaging_and_opens_viewer() {
    let mut c = WefaxCatcher::new();
    c.tick(ctx(true, false, WefaxState::Idle));
    c.tick(ctx(true, true, WefaxState::Idle)); // Locked
    let acts = c.tick(ctx(true, true, WefaxState::Imaging));
    assert!(matches!(c.state(), CatcherState::Imaging { .. }));
    assert_eq!(acts, vec![Action::OpenViewer]);
}

#[test]
fn imaging_then_complete_saves_and_stays() {
    let mut c = WefaxCatcher::new();
    c.tick(ctx(true, false, WefaxState::Idle));
    c.tick(ctx(true, true, WefaxState::Idle)); // Locked
    c.tick(ctx(true, true, WefaxState::Imaging)); // Imaging (viewer opens)
    let acts = c.tick(ctx(true, true, WefaxState::Stopped)); // complete
    assert_eq!(acts, vec![Action::SavePng(PathBuf::new())]);
    // stays on the working channel (not back to a fresh scan rotation)
    assert!(matches!(
        c.state(),
        CatcherState::Locked { .. } | CatcherState::Imaging { .. }
    ));
}

#[test]
fn disable_restores_the_tune_captured_at_enable_not_the_disable_ticks_ctx() {
    let mut c = WefaxCatcher::new();
    let enable_saved = SavedTune {
        center_hz: 14_346_000.0,
        demod_mode: sdr_types::DemodMode::Usb,
        bandwidth_hz: 3_000.0,
        was_wefax: false,
    };
    let mut enable_ctx = ctx(true, false, WefaxState::Idle);
    enable_ctx.saved_tune = enable_saved.clone();
    c.tick(enable_ctx);

    // The disabling tick's ctx carries a DIFFERENT saved_tune. The
    // machine must restore the snapshot captured at enable time, not
    // this one — it is self-contained, not dependent on the caller
    // re-supplying an unchanged saved_tune every tick.
    let mut disable_ctx = ctx(false, false, WefaxState::Idle);
    disable_ctx.saved_tune = SavedTune {
        center_hz: 999.0,
        demod_mode: sdr_types::DemodMode::Nfm,
        bandwidth_hz: 12_500.0,
        was_wefax: true,
    };
    let acts = c.tick(disable_ctx);

    assert_eq!(acts, vec![Action::RestoreTune(enable_saved)]);
    assert!(matches!(c.state(), CatcherState::Idle));
}

#[test]
fn disable_while_already_idle_is_a_noop() {
    let mut c = WefaxCatcher::new();
    let acts = c.tick(ctx(false, false, WefaxState::Idle));
    assert!(acts.is_empty());
    assert!(matches!(c.state(), CatcherState::Idle));
}

#[test]
fn disable_from_locked_restores_tune_and_idles() {
    let mut c = WefaxCatcher::new();
    c.tick(ctx(true, false, WefaxState::Idle));
    c.tick(ctx(true, true, WefaxState::Idle)); // Locked
    let acts = c.tick(ctx(false, false, WefaxState::Idle));
    assert!(acts.iter().any(|a| matches!(a, Action::RestoreTune(_))));
    assert!(matches!(c.state(), CatcherState::Idle));
}

#[test]
fn disable_from_imaging_restores_tune_and_idles() {
    let mut c = WefaxCatcher::new();
    c.tick(ctx(true, false, WefaxState::Idle));
    c.tick(ctx(true, true, WefaxState::Idle)); // Locked
    c.tick(ctx(true, true, WefaxState::Imaging)); // Imaging
    let acts = c.tick(ctx(false, false, WefaxState::Imaging));
    assert!(acts.iter().any(|a| matches!(a, Action::RestoreTune(_))));
    assert!(matches!(c.state(), CatcherState::Idle));
}

#[test]
fn false_lock_times_out_back_to_scanning() {
    let mut c = WefaxCatcher::new();
    c.tick(ctx(true, false, WefaxState::Idle));
    c.tick(ctx(true, true, WefaxState::Idle)); // Locked
    // presence drops and stays down past the lock timeout, no imaging
    for _ in 0..=LOCK_TIMEOUT_TICKS {
        c.tick(ctx(true, false, WefaxState::Idle));
    }
    assert!(matches!(c.state(), CatcherState::Scanning { .. }));
}

#[test]
fn locked_without_presence_increments_waited_before_timeout() {
    let mut c = WefaxCatcher::new();
    c.tick(ctx(true, false, WefaxState::Idle));
    c.tick(ctx(true, true, WefaxState::Idle)); // Locked, waited=0
    c.tick(ctx(true, false, WefaxState::Idle)); // waited=1
    match c.state() {
        CatcherState::Locked { waited, .. } => assert_eq!(*waited, 1),
        other => panic!("expected Locked, got {other:?}"),
    }
}

#[test]
fn imaging_brief_presence_flicker_does_not_abort() {
    let mut c = WefaxCatcher::new();
    c.tick(ctx(true, false, WefaxState::Idle));
    c.tick(ctx(true, true, WefaxState::Idle)); // Locked
    c.tick(ctx(true, true, WefaxState::Imaging)); // Imaging, lost=0

    // A one-tick flicker (detector false-negative, e.g. over a
    // blank/white image region) must NOT abort the in-progress chart.
    let acts = c.tick(ctx(true, false, WefaxState::Imaging)); // lost=1
    assert!(matches!(c.state(), CatcherState::Imaging { .. }));
    assert!(acts.is_empty());

    // Presence returns before the debounce threshold; `lost` resets.
    c.tick(ctx(true, true, WefaxState::Imaging));
    match c.state() {
        CatcherState::Imaging { lost, .. } => assert_eq!(*lost, 0),
        other => panic!("expected Imaging, got {other:?}"),
    }
}

#[test]
fn imaging_sustained_presence_loss_returns_to_scanning() {
    let mut c = WefaxCatcher::new();
    c.tick(ctx(true, false, WefaxState::Idle));
    c.tick(ctx(true, true, WefaxState::Idle)); // Locked
    c.tick(ctx(true, true, WefaxState::Imaging)); // Imaging, lost=0

    let mut acts = Vec::new();
    for _ in 0..IMAGING_LOSS_TICKS {
        acts = c.tick(ctx(true, false, WefaxState::Imaging));
    }
    assert!(matches!(c.state(), CatcherState::Scanning { .. }));
    assert!(acts.iter().any(|a| matches!(a, Action::Tune(_))));
}

#[test]
fn dwell_advances_to_next_candidate_after_dwell_ticks() {
    let mut c = WefaxCatcher::new();
    c.tick(ctx(true, false, WefaxState::Idle)); // enable -> Scanning idx0
    let first_freq = match c.state() {
        CatcherState::Scanning {
            candidates, idx, ..
        } => candidates[*idx].freq_hz,
        other => panic!("expected Scanning, got {other:?}"),
    };
    let mut acts = Vec::new();
    for _ in 0..SCAN_DWELL_TICKS {
        acts = c.tick(ctx(true, false, WefaxState::Idle));
    }
    match c.state() {
        CatcherState::Scanning {
            candidates,
            idx,
            dwell,
            ..
        } => {
            assert_eq!(*dwell, 0);
            assert_ne!(candidates[*idx].freq_hz, first_freq);
            assert_eq!(
                acts,
                vec![
                    Action::ResetDecoder,
                    Action::Tune(candidates[*idx].freq_hz),
                    Action::SetDemodMode,
                ]
            );
        }
        other => panic!("expected Scanning, got {other:?}"),
    }
}

#[test]
fn scanning_dwell_increments_without_retune_before_elapsed() {
    let mut c = WefaxCatcher::new();
    c.tick(ctx(true, false, WefaxState::Idle)); // -> Scanning idx0 dwell0
    let acts = c.tick(ctx(true, false, WefaxState::Idle)); // dwell1, no retune (SCAN_DWELL_TICKS=3)
    assert!(!acts.iter().any(|a| matches!(a, Action::Tune(_))));
    match c.state() {
        CatcherState::Scanning { idx, dwell, .. } => {
            assert_eq!(*idx, 0);
            assert_eq!(*dwell, 1);
        }
        other => panic!("expected Scanning, got {other:?}"),
    }
}

#[test]
fn empty_rotation_no_candidates_toasts_and_stays_idle() {
    let mut c = WefaxCatcher::new();
    let mut tctx = ctx(true, false, WefaxState::Idle);
    tctx.choice = StationChoice::Pinned("Nonexistent Station");
    let acts = c.tick(tctx);
    assert!(matches!(c.state(), CatcherState::Idle));
    assert!(acts.iter().any(|a| matches!(a, Action::Toast(_))));
}

#[test]
fn pinned_choice_filters_candidates_to_one_station() {
    let mut c = WefaxCatcher::new();
    let mut tctx = ctx(true, false, WefaxState::Idle);
    tctx.choice = StationChoice::Pinned("NMG New Orleans");
    c.tick(tctx);
    match c.state() {
        CatcherState::Scanning { candidates, .. } => {
            assert!(!candidates.is_empty());
            assert!(candidates.iter().all(|c| c.station == "NMG New Orleans"));
        }
        other => panic!("expected Scanning, got {other:?}"),
    }
}

#[test]
fn is_active_now_respects_a_limited_schedule_window() {
    assert!(WefaxCatcher::is_active_now(&TEST_ACTIVE_WINDOW, 180)); // 03:00Z, inside
    assert!(!WefaxCatcher::is_active_now(&TEST_ACTIVE_WINDOW, 420)); // 07:00Z, outside
}

#[test]
fn schedule_active_station_sorts_ahead_of_inactive_one() {
    let mut stations: Vec<&'static WefaxStation> = vec![&TEST_INACTIVE_WINDOW, &TEST_ACTIVE_WINDOW];
    WefaxCatcher::sort_schedule_active_first(&mut stations, 180); // 03:00Z
    assert_eq!(stations[0].name, "TEST-ACTIVE-WINDOW");
    assert_eq!(stations[1].name, "TEST-INACTIVE-WINDOW");
}

#[test]
fn default_impl_matches_new() {
    let c = WefaxCatcher::default();
    assert!(matches!(c.state(), CatcherState::Idle));
}

/// FIX 1(a) / #914: a channel that keeps presence true but never reaches
/// `Imaging` (a birdie, or an in-band carrier with no chart) must NOT
/// hang in `Locked` forever. Since the decoder never reaches `Phasing`
/// either, the much shorter `IDLE_LOCK_TIMEOUT_TICKS` timeout resumes
/// scanning at the NEXT candidate, rather than the full `LOCK_MAX_TICKS`
/// ceiling.
#[test]
fn locked_hard_ceiling_resumes_at_next_candidate() {
    let mut c = WefaxCatcher::new();
    c.tick(ctx(true, false, WefaxState::Idle)); // Scanning idx0
    c.tick(ctx(true, true, WefaxState::Idle)); // Locked idx0, waited0
    let next = match c.state() {
        CatcherState::Locked {
            candidates, idx, ..
        } => {
            assert_eq!(*idx, 0);
            candidates[(idx + 1) % candidates.len()].clone()
        }
        other => panic!("expected Locked, got {other:?}"),
    };

    // Presence STAYS true but the decoder never reaches Phasing/Imaging.
    let mut acts = Vec::new();
    for _ in 0..IDLE_LOCK_TIMEOUT_TICKS {
        acts = c.tick(ctx(true, true, WefaxState::Idle));
        if matches!(c.state(), CatcherState::Scanning { .. }) {
            break;
        }
    }
    assert!(
        matches!(c.state(), CatcherState::Scanning { .. }),
        "present-but-never-phasing must not hang in Locked"
    );
    assert_eq!(
        acts,
        vec![
            Action::ResetDecoder,
            Action::Tune(next.freq_hz),
            Action::SetDemodMode,
        ],
        "resumes on the NEXT candidate, not index 0"
    );
    match c.state() {
        CatcherState::Scanning {
            candidates, idx, ..
        } => {
            assert_ne!(*idx, 0, "must have advanced past candidate 0");
            assert_eq!(candidates[*idx].freq_hz, next.freq_hz);
        }
        other => panic!("expected Scanning, got {other:?}"),
    }
}

/// #914: a channel that never phases rescans at the short
/// `IDLE_LOCK_TIMEOUT_TICKS`, not the full `LOCK_MAX_TICKS` ceiling —
/// this is the actual "steady carrier" symptom from live IQ captures.
#[test]
fn locked_carrier_without_phasing_rescans_at_idle_timeout() {
    let mut c = WefaxCatcher::new();
    c.tick(ctx(true, false, WefaxState::Idle)); // Scanning idx0
    c.tick(ctx(true, true, WefaxState::Idle)); // Locked idx0, waited0
    let (start_idx, next) = match c.state() {
        CatcherState::Locked {
            candidates, idx, ..
        } => (*idx, candidates[(idx + 1) % candidates.len()].clone()),
        other => panic!("expected Locked, got {other:?}"),
    };

    // Presence stays true, decoder stays Idle (carrier, no preamble) for
    // IDLE_LOCK_TIMEOUT_TICKS - 1 ticks: must still be Locked.
    for _ in 0..IDLE_LOCK_TIMEOUT_TICKS - 1 {
        c.tick(ctx(true, true, WefaxState::Idle));
        assert!(
            matches!(c.state(), CatcherState::Locked { .. }),
            "must not rescan before IDLE_LOCK_TIMEOUT_TICKS"
        );
    }
    match c.state() {
        CatcherState::Locked { idx, .. } => assert_eq!(*idx, start_idx),
        other => panic!("expected Locked, got {other:?}"),
    }

    // The tick that reaches IDLE_LOCK_TIMEOUT_TICKS rescans to the NEXT
    // candidate, not index 0.
    let acts = c.tick(ctx(true, true, WefaxState::Idle));
    assert!(matches!(c.state(), CatcherState::Scanning { .. }));
    assert_eq!(
        acts,
        vec![
            Action::ResetDecoder,
            Action::Tune(next.freq_hz),
            Action::SetDemodMode,
        ]
    );
}

/// #914: once the decoder has reached `Phasing` at least once, the lock
/// is held toward the full `LOCK_MAX_TICKS` ceiling even if it later
/// drops back to `Idle` (e.g. between phasing and confirmed imaging) —
/// it must NOT rescan at the short idle timeout.
#[test]
fn locked_holds_past_idle_timeout_once_phasing_seen() {
    let mut c = WefaxCatcher::new();
    c.tick(ctx(true, false, WefaxState::Idle)); // Scanning idx0
    c.tick(ctx(true, true, WefaxState::Idle)); // Locked idx0

    // A couple of ticks actually phasing.
    c.tick(ctx(true, true, WefaxState::Phasing));
    c.tick(ctx(true, true, WefaxState::Phasing));

    // Then back to Idle (e.g. a brief re-detect gap) for well past
    // IDLE_LOCK_TIMEOUT_TICKS more ticks — must still be Locked, holding
    // toward LOCK_MAX_TICKS instead.
    for _ in 0..(IDLE_LOCK_TIMEOUT_TICKS + 4) {
        c.tick(ctx(true, true, WefaxState::Idle));
    }
    assert!(
        matches!(c.state(), CatcherState::Locked { .. }),
        "must hold toward LOCK_MAX_TICKS once phasing has been observed"
    );
}

/// #914: a real chart phasing then reaching `Imaging` still opens the
/// viewer as before — the phasing-aware timeout must not interfere with
/// the happy path.
#[test]
fn locked_reaches_imaging_from_phasing() {
    let mut c = WefaxCatcher::new();
    c.tick(ctx(true, false, WefaxState::Idle)); // Scanning idx0
    c.tick(ctx(true, true, WefaxState::Idle)); // Locked idx0
    c.tick(ctx(true, true, WefaxState::Phasing));
    let acts = c.tick(ctx(true, true, WefaxState::Imaging));
    assert!(matches!(c.state(), CatcherState::Imaging { .. }));
    assert_eq!(acts, vec![Action::OpenViewer]);
}

/// FIX 1(b): a false lock via a presence DROP must resume at the next
/// candidate too, not restart the rotation at index 0.
#[test]
fn false_lock_presence_drop_resumes_at_next_candidate() {
    let mut c = WefaxCatcher::new();
    c.tick(ctx(true, false, WefaxState::Idle)); // Scanning idx0
    c.tick(ctx(true, true, WefaxState::Idle)); // Locked idx0
    let next = match c.state() {
        CatcherState::Locked {
            candidates, idx, ..
        } => candidates[(idx + 1) % candidates.len()].clone(),
        other => panic!("expected Locked, got {other:?}"),
    };

    let mut acts = Vec::new();
    for _ in 0..LOCK_TIMEOUT_TICKS {
        acts = c.tick(ctx(true, false, WefaxState::Idle));
        if matches!(c.state(), CatcherState::Scanning { .. }) {
            break;
        }
    }
    assert!(matches!(c.state(), CatcherState::Scanning { .. }));
    assert_eq!(
        acts,
        vec![
            Action::ResetDecoder,
            Action::Tune(next.freq_hz),
            Action::SetDemodMode,
        ],
        "false lock resumes on the NEXT candidate, not index 0"
    );
}

/// FIX 1(b): a candidate 0 that persistently trips presence but never
/// images must not starve later candidates — each hard-ceiling timeout
/// advances the rotation (0 -> 1 -> 2 ...), never resetting to 0.
#[test]
fn persistent_false_lock_does_not_starve_later_candidates() {
    let mut c = WefaxCatcher::new();
    c.tick(ctx(true, false, WefaxState::Idle));
    c.tick(ctx(true, true, WefaxState::Idle)); // Locked idx0

    let mut resumed_indices = Vec::new();
    // Drive well past two hard ceilings; presence stays true, no imaging.
    for _ in 0..(LOCK_MAX_TICKS * 2 + 10) {
        c.tick(ctx(true, true, WefaxState::Idle));
        if let CatcherState::Scanning { idx, dwell, .. } = c.state()
            && *dwell == 0
        {
            resumed_indices.push(*idx);
        }
    }
    assert!(
        resumed_indices.contains(&1),
        "must resume at candidate 1, not restart at 0: {resumed_indices:?}"
    );
    assert!(
        resumed_indices.contains(&2),
        "must keep advancing past 1 to 2: {resumed_indices:?}"
    );
    assert!(
        !resumed_indices.iter().all(|&i| i == 0),
        "must never be stuck on candidate 0: {resumed_indices:?}"
    );
}
