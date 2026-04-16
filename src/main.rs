// The `sdr` binary is the GTK4 + libadwaita frontend, which is currently
// Linux-only and gated behind the `gtk-frontend` cargo feature. On
// non-Linux platforms (macOS, Windows) — or on Linux without the
// `gtk-frontend` feature — we provide a stub `main()` that prints a
// message and exits non-zero so the workspace still builds end-to-end
// on every platform without surprising linker failures. The macOS
// native frontend lives in `apps/macos/` (SwiftUI) and runs against
// the `sdr-core` engine via the `sdr-ffi` C ABI.

#[cfg(all(target_os = "linux", feature = "gtk-frontend"))]
use gtk4::glib;
#[cfg(all(target_os = "linux", feature = "gtk-frontend"))]
use gtk4::prelude::*;

#[cfg(all(target_os = "linux", feature = "gtk-frontend"))]
fn main() -> glib::ExitCode {
    // Splash subprocess mode. The sdr-splash controller re-execs us
    // with `--splash` as argv[1] to render a tiny GTK splash window
    // during the otherwise-blocking sherpa init phase. Dispatch BEFORE
    // any mallopt or sherpa init — this is a separate process that
    // does its own GTK setup, completely independent of the parent.
    if std::env::args().nth(1).as_deref() == Some("--splash") {
        let exit_code: i32 = sdr_splash_gtk::run();
        return glib::ExitCode::from(u8::try_from(exit_code).unwrap_or(1));
    }

    // Limit glibc malloc arenas before any threads spawn.
    // Without this, glibc creates up to 8*cores arenas that each keep
    // their high-water mark, causing RSS to grow indefinitely with 40+ threads.
    // Uses mallopt() instead of env var — glibc reads MALLOC_ARENA_MAX
    // at allocator init (before main), so set_var is too late.
    #[cfg(target_env = "gnu")]
    #[allow(unsafe_code)]
    let arena_ok = unsafe {
        unsafe extern "C" {
            fn mallopt(param: i32, value: i32) -> i32;
        }
        const M_ARENA_MAX: i32 = -8;
        mallopt(M_ARENA_MAX, 4) != 0
    };

    tracing_subscriber::fmt::init();
    #[cfg(target_env = "gnu")]
    if !arena_ok {
        tracing::warn!("mallopt(M_ARENA_MAX, 4) failed — arena cap not applied");
    }
    tracing::info!("sdr-rs starting");

    // Single-instance enforcement. Build the AdwApplication and
    // register it on the session bus BEFORE doing any expensive
    // startup work (sherpa bundle download, splash subprocess, DSP
    // engine spawn). If another sdr-rs is already the primary, the
    // register call marks us remote, we forward an `activate` signal
    // so the existing window raises, and exit 0 cleanly. This avoids
    // two processes racing on the RTL-SDR USB device, config file
    // writes, and sherpa model downloads.
    let app = sdr_ui::build_app();
    if !sdr_ui::register_and_check_primary(&app) {
        return glib::ExitCode::SUCCESS;
    }

    // Initialize the sherpa-onnx host BEFORE GTK is loaded.
    // Drain the event channel until we see Ready or Failed (or the
    // channel disconnects, which means the worker died unexpectedly).
    // A SplashController shows the user live progress for the duration.
    #[cfg(feature = "sherpa")]
    {
        use sdr_splash::SplashController;
        use sdr_transcription::{InitEvent, SherpaModel};

        // Read the persisted sherpa model selection from the user's
        // config so we initialize the recognizer they actually want.
        // Without this, init_sherpa_host always built Zipformer and
        // any other selection in the dropdown only took effect after
        // the user manually round-tripped the dropdown to trigger
        // the runtime reload from PR 5. Confusing UX — the dropdown
        // would show e.g. Parakeet but the recognizer was Zipformer
        // until the user clicked away and back.
        let saved_model = {
            let config_path = gtk4::glib::user_config_dir()
                .join("sdr-rs")
                .join("config.json");
            let defaults = serde_json::json!({});
            let mgr =
                sdr_config::ConfigManager::load(&config_path, &defaults).unwrap_or_else(|e| {
                    tracing::warn!(
                        "config load failed for sherpa model selection, using default: {e}"
                    );
                    sdr_config::ConfigManager::in_memory(&defaults)
                });
            let saved_idx = mgr.read(|v| {
                v.get("transcription_sherpa_model")
                    .and_then(serde_json::Value::as_u64)
                    .and_then(|idx| usize::try_from(idx).ok())
                    .unwrap_or(0)
            });
            SherpaModel::ALL
                .get(saved_idx)
                .copied()
                .unwrap_or(SherpaModel::StreamingZipformerEn)
        };
        tracing::info!(
            ?saved_model,
            "initializing sherpa host with persisted model"
        );

        // Spawn the splash subprocess BEFORE init_sherpa_host. If the
        // model is already cached, the recognizer creation takes ~1-2
        // seconds and the splash flickers briefly; if it has to
        // download (~30 seconds for the 256 MB bundle), the splash
        // shows live progress for the duration. Falls back to a no-op
        // controller if the subprocess can't spawn — see
        // SplashController::try_spawn for the failure modes.
        let mut splash = SplashController::try_spawn("Initializing sherpa-onnx...");

        let event_rx = sdr_transcription::init_sherpa_host(saved_model);

        let mut current_component: &'static str = "sherpa-onnx";
        loop {
            match event_rx.recv() {
                Ok(InitEvent::DownloadStart { component }) => {
                    tracing::info!(%component, "sherpa download starting");
                    current_component = component;
                    splash.update_text(&format!("Downloading {component}..."));
                }
                Ok(InitEvent::DownloadProgress { pct }) => {
                    tracing::info!(progress_pct = pct, "sherpa download progress");
                    splash.update_text(&format!("Downloading {current_component}... {pct}%"));
                }
                Ok(InitEvent::Extracting { component }) => {
                    tracing::info!(%component, "sherpa extracting archive");
                    current_component = component;
                    splash.update_text(&format!("Extracting {component}..."));
                }
                Ok(InitEvent::CreatingRecognizer) => {
                    tracing::info!("sherpa creating recognizer");
                    splash.update_text("Loading sherpa-onnx recognizer...");
                }
                Ok(InitEvent::Ready) => {
                    tracing::info!("sherpa ready");
                    break;
                }
                Ok(InitEvent::Failed { message }) => {
                    tracing::warn!(%message, "sherpa init failed");
                    // Don't update splash text — we're about to drop it.
                    // The error will surface in status_label when the
                    // user toggles Sherpa transcription.
                    break;
                }
                Err(_) => {
                    tracing::warn!("sherpa init event channel disconnected");
                    break;
                }
            }
        }

        // Drop the splash controller — closes the subprocess.
        drop(splash);
    }

    app.run()
}

#[cfg(any(not(target_os = "linux"), not(feature = "gtk-frontend")))]
fn main() -> std::process::ExitCode {
    eprintln!("sdr-rs: the GTK4 frontend is currently Linux-only.");
    eprintln!();
    eprintln!("macOS support via a native SwiftUI app is in progress");
    eprintln!("(see https://github.com/jasonherald/rtl-sdr/issues/228).");
    eprintln!();
    eprintln!("On Linux, install GTK4 + libadwaita and run `cargo run --release`");
    eprintln!("(the `gtk-frontend` feature is enabled by default).");
    std::process::ExitCode::from(1)
}
