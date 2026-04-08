use gtk4::glib;

fn main() -> glib::ExitCode {
    // Limit glibc malloc arenas before any threads spawn.
    // Without this, glibc creates up to 8*cores arenas that each keep
    // their high-water mark, causing RSS to grow indefinitely with 40+ threads.
    // Uses mallopt() instead of env var — glibc reads MALLOC_ARENA_MAX
    // at allocator init (before main), so set_var is too late.
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    #[allow(unsafe_code)]
    let arena_ok = unsafe {
        unsafe extern "C" {
            fn mallopt(param: i32, value: i32) -> i32;
        }
        const M_ARENA_MAX: i32 = -8;
        mallopt(M_ARENA_MAX, 4) != 0
    };

    tracing_subscriber::fmt::init();
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    if !arena_ok {
        tracing::warn!("mallopt(M_ARENA_MAX, 4) failed — arena cap not applied");
    }
    tracing::info!("sdr-rs starting");
    sdr_ui::run()
}
