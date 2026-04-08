use gtk4::glib;

fn main() -> glib::ExitCode {
    // Limit glibc malloc arenas BEFORE any threads spawn.
    // Without this, glibc creates up to 8*cores arenas that each keep
    // their high-water mark, causing RSS to grow indefinitely with 40+ threads.
    // Limit glibc malloc arenas BEFORE any threads spawn.
    // Without this, glibc creates up to 8*cores arenas that each keep
    // their high-water mark, causing RSS to grow indefinitely with 40+ threads.
    #[cfg(target_os = "linux")]
    #[allow(unsafe_code)]
    // SAFETY: single-threaded at this point — no threads spawned yet.
    unsafe {
        std::env::set_var("MALLOC_ARENA_MAX", "4");
    }

    tracing_subscriber::fmt::init();
    tracing::info!("sdr-rs starting");
    sdr_ui::run()
}
