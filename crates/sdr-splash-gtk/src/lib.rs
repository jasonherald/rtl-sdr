//! GTK4 + libadwaita splash window for sdr-rs.
//!
//! Linux-only. The cross-platform controller is in the `sdr-splash`
//! crate; this crate is the implementation that opens the actual
//! window. The controller spawns this binary as a subprocess by
//! re-exec'ing the main `sdr-rs` binary with `--splash`; that argv
//! mode dispatches to [`run`] in this crate.
//!
//! ## Wire protocol
//!
//! Reads single-line commands from stdin:
//!
//! - `text:<message>\n` — update the centered label
//! - `done\n` — close the window and exit cleanly
//!
//! All unrecognized lines are silently ignored. EOF on stdin closes
//! the window the same way `done` does.

#[cfg(target_os = "linux")]
mod linux_impl {
    use std::cell::RefCell;
    use std::io::BufRead;
    use std::rc::Rc;
    use std::sync::mpsc;
    use std::time::Duration;

    use gtk4::glib;
    use gtk4::prelude::*;

    /// Interval in milliseconds for polling the stdin command channel.
    const STDIN_POLL_INTERVAL_MS: u64 = 50;

    /// Commands sent from the stdin reader thread to the GTK main thread.
    enum StdinCommand {
        SetText(String),
        Done,
    }

    /// Run the GTK splash event loop. Returns when the window is closed
    /// or stdin EOFs.
    pub fn run() -> glib::ExitCode {
        let app = libadwaita::Application::builder()
            .application_id("com.sdr.rs.splash")
            .build();

        // Channel from stdin reader thread → main thread.
        let (cmd_tx, cmd_rx) = mpsc::channel::<StdinCommand>();

        // Spawn the stdin reader thread BEFORE app.run() so it is ready
        // when the activate signal fires.
        std::thread::Builder::new()
            .name("sdr-splash-stdin".into())
            .spawn(move || {
                let stdin = std::io::stdin();
                let reader = stdin.lock();
                for line in reader.lines() {
                    let Ok(line) = line else { break };
                    if line == "done" {
                        let _ = cmd_tx.send(StdinCommand::Done);
                        break;
                    }
                    if let Some(text) = line.strip_prefix("text:")
                        && cmd_tx.send(StdinCommand::SetText(text.to_owned())).is_err()
                    {
                        break;
                    }
                    // Unrecognized lines silently ignored.
                }
                // EOF on stdin → tell main thread to quit.
                let _ = cmd_tx.send(StdinCommand::Done);
            })
            .expect("failed to spawn sdr-splash-stdin reader thread");

        // Hold the label widget in a RefCell so the timeout closure can
        // update it. The cell is populated in build_window() during
        // connect_activate and read in the timeout closure below.
        //
        // connect_activate takes an `Fn` (potentially called more than once
        // for re-activation), so cmd_rx — which is not Copy — must be wrapped
        // in an Option so the first activation can take ownership and install
        // the timeout, while any subsequent activations (rare in practice) just
        // present the existing window.
        let label_cell: Rc<RefCell<Option<gtk4::Label>>> = Rc::new(RefCell::new(None));
        let label_cell_for_activate = Rc::clone(&label_cell);
        let cmd_rx_cell: Rc<RefCell<Option<mpsc::Receiver<StdinCommand>>>> =
            Rc::new(RefCell::new(Some(cmd_rx)));

        app.connect_activate(move |app| {
            build_window(app, &label_cell_for_activate);

            // Install a periodic timeout on first activation only.
            // If cmd_rx has already been taken (re-activation), skip.
            let Some(cmd_rx) = cmd_rx_cell.borrow_mut().take() else {
                return;
            };

            // Install a periodic timeout that drains the stdin command
            // channel on the GTK main thread. GTK widgets are !Send so
            // they cannot be touched from the stdin thread directly.
            let label_cell = Rc::clone(&label_cell_for_activate);
            let app_clone = app.clone();
            let _ =
                glib::timeout_add_local(Duration::from_millis(STDIN_POLL_INTERVAL_MS), move || {
                    loop {
                        match cmd_rx.try_recv() {
                            Ok(StdinCommand::SetText(text)) => {
                                if let Some(label) = label_cell.borrow().as_ref() {
                                    label.set_text(&text);
                                }
                            }
                            Ok(StdinCommand::Done) | Err(mpsc::TryRecvError::Disconnected) => {
                                app_clone.quit();
                                return glib::ControlFlow::Break;
                            }
                            Err(mpsc::TryRecvError::Empty) => break,
                        }
                    }
                    glib::ControlFlow::Continue
                });
        });

        // Pass empty argv to GTK so it does not try to parse --splash.
        let no_args: [&str; 0] = [];
        app.run_with_args(&no_args)
    }

    fn build_window(app: &libadwaita::Application, label_cell: &Rc<RefCell<Option<gtk4::Label>>>) {
        let label = gtk4::Label::builder()
            .label("Initializing...")
            .wrap(true)
            .justify(gtk4::Justification::Center)
            .css_classes(["title-3"])
            .build();

        let spinner = gtk4::Spinner::builder()
            .spinning(true)
            .width_request(48)
            .height_request(48)
            .build();

        let vbox = gtk4::Box::builder()
            .orientation(gtk4::Orientation::Vertical)
            .spacing(16)
            .margin_top(32)
            .margin_bottom(32)
            .margin_start(32)
            .margin_end(32)
            .halign(gtk4::Align::Center)
            .valign(gtk4::Align::Center)
            .build();
        vbox.append(&spinner);
        vbox.append(&label);

        let window = libadwaita::ApplicationWindow::builder()
            .application(app)
            .title("SDR-RS")
            .default_width(420)
            .default_height(180)
            .resizable(false)
            .content(&vbox)
            .build();

        // Stash the label so the timeout closure can update it.
        *label_cell.borrow_mut() = Some(label);

        window.present();
    }
}

/// Run the splash window. On Linux, opens a tiny GTK4 + libadwaita
/// window with a spinner and a label that updates in response to
/// commands read from stdin. On other platforms, prints an error and
/// returns 1.
#[cfg(target_os = "linux")]
pub fn run() -> i32 {
    let exit_code = linux_impl::run();
    // glib::ExitCode implements From<ExitCode> for i32.
    i32::from(exit_code)
}

#[cfg(not(target_os = "linux"))]
pub fn run() -> i32 {
    tracing::error!("sdr-splash-gtk: GTK splash window is currently Linux-only");
    1
}
