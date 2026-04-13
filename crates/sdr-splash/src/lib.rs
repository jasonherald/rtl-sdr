//! Cross-platform controller for the sdr-rs splash subprocess.
//!
//! The splash itself is implemented in `sdr-splash-gtk` (Linux) and
//! invoked by re-exec'ing the main `sdr-rs` binary with a `--splash`
//! argv. This crate just spawns that subprocess and writes
//! line-oriented commands to its stdin.
//!
//! ## Wire protocol
//!
//! Single-line commands sent on stdin:
//!
//! - `text:<message>\n` — update the splash window's label text
//! - `done\n` — close the window cleanly
//!
//! All unrecognized lines are silently ignored. Closing stdin (EOF)
//! has the same effect as sending `done`.
//!
//! ## Lifetime
//!
//! [`SplashController::try_spawn`] returns immediately, with the
//! subprocess running in the background. The controller can be
//! updated via [`SplashController::update_text`]. On `Drop`, the
//! controller closes the subprocess's stdin (which the splash window
//! observes as EOF and exits cleanly) and reaps the child.
//!
//! If the subprocess can't be started for any reason — `current_exe()`
//! unavailable, fork failure, etc. — the controller silently falls
//! back to a no-op state and all methods become no-ops. Callers don't
//! need conditional logic; the splash either appears or it doesn't.

use std::io::Write;
use std::process::{Child, ChildStdin, Command, Stdio};

/// Replace any embedded `\n` / `\r` characters with spaces. The splash
/// subprocess protocol is one command per line, so embedded newlines
/// in a label update would split into extra frames and a later line
/// could be interpreted as `done`. Callers currently pass static
/// strings with interpolated percent values so this is defensive, but
/// cheap enough to do unconditionally.
fn sanitize_protocol_text(text: &str) -> String {
    text.replace(['\r', '\n'], " ")
}

/// Controller for the sdr-rs splash subprocess.
pub struct SplashController {
    inner: Option<SplashInner>,
}

/// Internal state when the splash subprocess actually started.
struct SplashInner {
    child: Child,
    stdin: ChildStdin,
}

impl SplashController {
    /// Try to spawn the splash subprocess by re-exec'ing the current
    /// binary with `--splash` as argv[1]. Returns an empty controller
    /// (all methods no-op) on any failure.
    ///
    /// `initial_text` is sent to the splash window immediately after
    /// spawn so the user sees it right away.
    #[must_use]
    pub fn try_spawn(initial_text: &str) -> Self {
        let Ok(exe) = std::env::current_exe() else {
            tracing::warn!("SplashController: current_exe() failed; skipping splash");
            return Self { inner: None };
        };

        let child_result = Command::new(&exe)
            .arg("--splash")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();

        let mut child = match child_result {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "SplashController: failed to spawn splash subprocess; skipping splash"
                );
                return Self { inner: None };
            }
        };

        let Some(stdin) = child.stdin.take() else {
            tracing::warn!("SplashController: child stdin unavailable; skipping splash");
            // Still need to reap the child even though we won't use it.
            let _ = child.kill();
            let _ = child.wait();
            return Self { inner: None };
        };

        let mut inner = SplashInner { child, stdin };
        // Send the initial text. If this fails the controller stays
        // active but won't render anything until the next update.
        let sanitized = sanitize_protocol_text(initial_text);
        if let Err(e) = writeln!(inner.stdin, "text:{sanitized}") {
            tracing::warn!(error = %e, "SplashController: initial text write failed");
        }
        let _ = inner.stdin.flush();

        Self { inner: Some(inner) }
    }

    /// True if the splash subprocess is running.
    #[must_use]
    pub fn is_active(&self) -> bool {
        self.inner.is_some()
    }

    /// Update the label text on the splash window. No-op if the
    /// controller is inactive (subprocess didn't spawn).
    pub fn update_text(&mut self, text: &str) {
        // Take ownership of `inner` so we can explicitly reap the child
        // on the failure path. If we used `as_mut` and set `self.inner =
        // None` on error, the `Child` handle would drop without the
        // `Drop` impl ever reaping it.
        let Some(mut inner) = self.inner.take() else {
            return;
        };
        let sanitized = sanitize_protocol_text(text);
        if writeln!(inner.stdin, "text:{sanitized}")
            .and_then(|()| inner.stdin.flush())
            .is_err()
        {
            tracing::warn!("SplashController: stdin write failed; subprocess may have died");
            // Reap the child explicitly since Drop won't run on the
            // half-moved inner. Dropping stdin first signals EOF so
            // a still-alive child exits cleanly.
            drop(inner.stdin);
            let _ = inner.child.kill();
            let _ = inner.child.wait();
            return;
        }
        // Write succeeded — put `inner` back.
        self.inner = Some(inner);
    }
}

impl Drop for SplashController {
    fn drop(&mut self) {
        let Some(mut inner) = self.inner.take() else {
            return;
        };
        // Closing stdin signals EOF to the splash subprocess, which
        // observes it and exits cleanly. We then wait for the child
        // to reap; if it doesn't exit within a short window we kill it.
        let _ = writeln!(inner.stdin, "done");
        let _ = inner.stdin.flush();
        drop(inner.stdin);

        // Best-effort wait. We don't want to block the main thread for
        // long during process exit, so kill if it doesn't exit promptly.
        if matches!(inner.child.try_wait(), Ok(Some(_))) {
            return;
        }
        // Give it a brief window to exit on its own.
        std::thread::sleep(std::time::Duration::from_millis(150));
        if matches!(inner.child.try_wait(), Ok(Some(_))) {
            return;
        }
        let _ = inner.child.kill();
        let _ = inner.child.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_controller_is_inactive() {
        // Construct an empty controller directly (simulating the
        // try_spawn failure path) and verify all methods are no-ops.
        let mut controller = SplashController { inner: None };
        assert!(!controller.is_active());
        // Should not panic.
        controller.update_text("hello");
        // Drop should not panic on an empty controller.
        drop(controller);
    }
}
