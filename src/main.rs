fn main() -> glib::ExitCode {
    tracing_subscriber::fmt::init();
    tracing::info!("sdr-rs starting");
    sdr_ui::run()
}

use gtk4::glib;
