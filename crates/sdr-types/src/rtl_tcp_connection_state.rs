//! UI-facing connection state for `rtl_tcp` network sources.
//!
//! Lives in `sdr-types` (not `sdr-source-network`) so the UI layer
//! can name this enum without depending on the source crate's full
//! implementation tree. The source-crate implementation keeps its
//! richer internal state (including `Instant`-based scheduling) as
//! a private type; a `From` impl projects that internal state into
//! this public form at the layer boundary.

use std::time::Duration;

/// Rendered state of an `rtl_tcp` client source's connection
/// lifecycle, as the UI consumes it. Every variant is serializable
/// to a short human-readable line without needing extra context
/// from the rest of the crate.
///
/// **Time representation:** the internal state machine holds
/// scheduled events as `Instant`s, which don't cross crate (or
/// serialization) boundaries cleanly. This type uses `Duration`
/// "time until" values instead — computed at projection time
/// (`Instant::checked_duration_since(now)`). UI tick cadence is
/// fast enough (ideally ≤1 s) that the relative-time staleness is
/// invisible.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RtlTcpConnectionState {
    /// No connection attempt has begun yet. Initial state on
    /// source construction, before `start()`.
    Disconnected,

    /// First TCP connect is in flight. Transient — advances to
    /// `Connected` on success or `Retrying` / `Failed` on error.
    Connecting,

    /// Handshake succeeded and the data pump is streaming. Carries
    /// tuner metadata the UI uses to label the status row.
    Connected {
        /// Human-readable tuner name (e.g. `"R820T"`, `"E4000"`).
        /// Pre-projected to `String` instead of the `TunerTypeCode`
        /// enum so this type doesn't force downstream consumers to
        /// depend on `sdr-server-rtltcp`'s protocol module.
        tuner_name: String,
        /// Number of discrete gain steps the tuner advertised in
        /// the dongle-info header. Lets the UI show
        /// `"R820T, 29 gain steps"` without a second lookup.
        gain_count: u32,
        /// Human-readable label for the negotiated stream codec
        /// (e.g. `"None"`, `"LZ4"`). Pre-projected to a plain
        /// `String` so this type doesn't pull in the codec enum
        /// from `sdr-server-rtltcp`. Legacy servers and
        /// uncompressed-by-choice paths both land on `"None"`.
        /// Issue #307.
        codec: String,
        /// Server's `ServerExtension.granted_role` decision from
        /// the #392 extended handshake, projected to `Option<bool>`
        /// so this type doesn't pull in `sdr-server-rtltcp`'s wire
        /// `Role` enum: `Some(true)` = Controller, `Some(false)` =
        /// Listener, `None` = unknown (we never sent a hello, or
        /// the server is a pre-#392 RTLX build that doesn't yet
        /// write the field). UIs render the role badge only when
        /// this is `Some` — per CodeRabbit round 1 on PR #408,
        /// a legacy / pre-#392 server's actual slot is unknowable
        /// from the client side, and guessing "Controller" there
        /// could mis-label the session. Issue #396.
        granted_role: Option<bool>,
    },

    /// Transport-level error (connect refused, EOF, stall). The
    /// manager is in its reconnect-with-backoff loop; UI can show
    /// the next retry countdown.
    Retrying {
        /// Attempt counter, monotonically increasing across the
        /// lifetime of this source. Useful for "retry #12" style
        /// display.
        attempt: u32,
        /// Time until the next connect attempt. Computed at the
        /// projection call site; a saturating subtraction is fine
        /// because the manager thread will just fire immediately if
        /// we happen to race past the deadline.
        retry_in: Duration,
    },

    /// Terminal failure — only entered on protocol-level errors
    /// (e.g. a non-RTL0 handshake). Transport failures stay in
    /// `Retrying` forever. UI treats this as "needs user action"
    /// (e.g. pick a different server or disconnect).
    Failed {
        /// Short reason string suitable for direct display. The
        /// underlying error type's `Display` should produce this.
        reason: String,
    },

    /// Server has an existing Control client and the user's
    /// connect attempt was denied with `Status::ControllerBusy`.
    /// The UI surfaces this distinctly (not as generic
    /// `Retrying` or `Failed`) so it can offer a "Take control"
    /// / "Connect as Listener" choice instead of silently
    /// retrying in the background. Pre-#396 this folded into
    /// `TemporarilyUnavailable` with an auto-retry loop — that
    /// hid the busy state from the user and prevented the
    /// explicit role-choice UX the issue calls for. Per #396.
    ControllerBusy,

    /// Server requires a pre-shared key (#394) and the client
    /// didn't send one. UI surfaces this by revealing / focusing
    /// the Server key field so the user can enter the key and
    /// reconnect. Distinct from `AuthFailed` (which means the
    /// client DID send a key but the server rejected it) so the
    /// UI can tell "never tried" vs "wrong key" apart in the
    /// toast copy. Terminal — the connection manager does not
    /// auto-retry while an auth prompt is pending. Per #396.
    AuthRequired,

    /// Server required a key and the client's attempt was
    /// rejected (`Status::AuthFailed`). UI surfaces "Key
    /// rejected. Check with the server owner." and re-focuses
    /// the Server key field. Terminal — same no-retry treatment
    /// as `AuthRequired`. Per #396.
    AuthFailed,
}

impl RtlTcpConnectionState {
    /// True when the source is actively connected and streaming
    /// data. Helper so UI code doesn't have to pattern-match the
    /// full enum when all it wants is a boolean.
    pub fn is_connected(&self) -> bool {
        matches!(self, Self::Connected { .. })
    }

    /// True in the two "activity in progress" states — either
    /// making the first attempt or cycling through reconnects.
    /// Used by the status indicator to pick a spinner-vs-icon
    /// treatment.
    pub fn is_in_progress(&self) -> bool {
        matches!(self, Self::Connecting | Self::Retrying { .. })
    }

    /// True for terminal states that require explicit user
    /// interaction before another connect attempt. `Failed` +
    /// the three role-denial variants (`ControllerBusy`,
    /// `AuthRequired`, `AuthFailed`) all halt the auto-retry
    /// loop. UI uses this to distinguish "keep showing the
    /// spinner, server will come back" (`Retrying`) from
    /// "the user has to pick / type / click something"
    /// (this group). Per #396.
    pub fn needs_user_action(&self) -> bool {
        matches!(
            self,
            Self::Failed { .. } | Self::ControllerBusy | Self::AuthRequired | Self::AuthFailed
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_connected_matches_only_connected_variant() {
        assert!(!RtlTcpConnectionState::Disconnected.is_connected());
        assert!(!RtlTcpConnectionState::Connecting.is_connected());
        assert!(
            RtlTcpConnectionState::Connected {
                tuner_name: "R820T".into(),
                gain_count: 29,
                codec: "None".into(),
                granted_role: Some(true),
            }
            .is_connected()
        );
        assert!(
            !RtlTcpConnectionState::Retrying {
                attempt: 1,
                retry_in: Duration::from_secs(5),
            }
            .is_connected()
        );
        assert!(
            !RtlTcpConnectionState::Failed {
                reason: "bad header".into(),
            }
            .is_connected()
        );
    }

    #[test]
    fn is_in_progress_matches_connecting_and_retrying() {
        assert!(!RtlTcpConnectionState::Disconnected.is_in_progress());
        assert!(RtlTcpConnectionState::Connecting.is_in_progress());
        assert!(
            !RtlTcpConnectionState::Connected {
                tuner_name: "R820T".into(),
                gain_count: 29,
                codec: "None".into(),
                granted_role: Some(true),
            }
            .is_in_progress()
        );
        assert!(
            RtlTcpConnectionState::Retrying {
                attempt: 2,
                retry_in: Duration::from_secs(3),
            }
            .is_in_progress()
        );
        assert!(!RtlTcpConnectionState::Failed { reason: "x".into() }.is_in_progress());
    }

    #[test]
    fn needs_user_action_matches_terminal_user_action_states() {
        // Terminal states that gate the auto-retry loop and
        // demand an explicit click / type / pick from the user.
        // Added alongside the #396 client UI so the helper
        // cleanly replaces an ad-hoc pattern match at every
        // call site. Pre-#396 only `Failed` needed this
        // treatment; `ControllerBusy` / `AuthRequired` /
        // `AuthFailed` are new terminal variants that must
        // also return `true`.
        assert!(RtlTcpConnectionState::Failed { reason: "x".into() }.needs_user_action());
        assert!(RtlTcpConnectionState::ControllerBusy.needs_user_action());
        assert!(RtlTcpConnectionState::AuthRequired.needs_user_action());
        assert!(RtlTcpConnectionState::AuthFailed.needs_user_action());

        // Non-terminal states — the connection manager is
        // either waiting to start, already streaming, or
        // auto-retrying on its own backoff schedule. The UI
        // should show a spinner / status icon, not a recovery
        // affordance.
        assert!(!RtlTcpConnectionState::Disconnected.needs_user_action());
        assert!(!RtlTcpConnectionState::Connecting.needs_user_action());
        assert!(
            !RtlTcpConnectionState::Connected {
                tuner_name: "R820T".into(),
                gain_count: 29,
                codec: "None".into(),
                granted_role: Some(true),
            }
            .needs_user_action()
        );
        assert!(
            !RtlTcpConnectionState::Retrying {
                attempt: 1,
                retry_in: Duration::from_secs(1),
            }
            .needs_user_action()
        );
    }
}
