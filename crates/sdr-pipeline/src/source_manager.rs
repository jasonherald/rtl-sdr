//! Source manager — registration and lifecycle for IQ sources.
//!
//! Ports SDR++ `SourceManager`. Manages the set of available IQ sources
//! (RTL-SDR, Network, File) and controls which one is active.

use sdr_types::{Complex, SourceError};
use std::collections::HashMap;

/// Trait for an IQ signal source.
///
/// Implemented by each source module (RTL-SDR, network, file).
pub trait Source: Send {
    /// Human-readable name (e.g., "RTL-SDR", "Network").
    fn name(&self) -> &str;

    /// Start producing IQ samples.
    fn start(&mut self) -> Result<(), SourceError>;

    /// Stop producing samples.
    fn stop(&mut self) -> Result<(), SourceError>;

    /// Tune to a frequency in Hz.
    fn tune(&mut self, frequency_hz: f64) -> Result<(), SourceError>;

    /// List of supported sample rates in Hz.
    fn sample_rates(&self) -> &[f64];

    /// Current sample rate in Hz.
    fn sample_rate(&self) -> f64;

    /// Set sample rate.
    fn set_sample_rate(&mut self, rate: f64) -> Result<(), SourceError>;

    /// Read IQ samples into the output buffer. Returns number of Complex samples written.
    fn read_samples(&mut self, output: &mut [Complex]) -> Result<usize, SourceError>;

    /// Set tuner gain in tenths of dB (no-op default for non-tuner sources).
    fn set_gain(&mut self, _gain_tenths: i32) -> Result<(), SourceError> {
        Ok(())
    }

    /// Set AGC mode (no-op default for non-tuner sources).
    fn set_gain_mode(&mut self, _manual: bool) -> Result<(), SourceError> {
        Ok(())
    }

    /// Get available gain values in tenths of dB (empty for non-tuner sources).
    fn gains(&self) -> &[i32] {
        &[]
    }

    /// Set PPM frequency correction (no-op default for non-tuner sources).
    fn set_ppm_correction(&mut self, _ppm: i32) -> Result<(), SourceError> {
        Ok(())
    }

    /// UI-facing connection state for `rtl_tcp` clients. Only the
    /// network `RtlTcpSource` implements this meaningfully — every
    /// other source returns `None`. Lets the UI poll the active
    /// source without downcasting to the concrete type.
    ///
    /// Returns a projected form (`Instant`-free) so the type can
    /// live in `sdr-types` without pulling in scheduling primitives
    /// that don't cross crate boundaries cleanly.
    fn rtl_tcp_connection_state(&self) -> Option<sdr_types::RtlTcpConnectionState> {
        None
    }
}

/// Manages available IQ sources and the active source lifecycle.
///
/// Ports SDR++ `SourceManager`.
pub struct SourceManager {
    sources: HashMap<String, Box<dyn Source>>,
    selected: Option<String>,
    running: bool,
}

impl SourceManager {
    /// Create a new empty source manager.
    pub fn new() -> Self {
        Self {
            sources: HashMap::new(),
            selected: None,
            running: false,
        }
    }

    /// Register a source.
    ///
    /// # Errors
    ///
    /// Returns `SourceError` if a source with this name already exists.
    pub fn register(&mut self, source: Box<dyn Source>) -> Result<(), SourceError> {
        let name = source.name().to_string();
        if self.sources.contains_key(&name) {
            return Err(SourceError::OpenFailed(format!(
                "source already registered: {name}"
            )));
        }
        self.sources.insert(name, source);
        Ok(())
    }

    /// Unregister a source by name. Stops it first if running.
    pub fn unregister(&mut self, name: &str) -> Option<Box<dyn Source>> {
        // Stop the source if it's the selected running source
        if self.selected.as_deref() == Some(name) && self.running {
            if let Some(source) = self.sources.get_mut(name)
                && let Err(e) = source.stop()
            {
                tracing::warn!("failed to stop source during unregister: {e}");
            }
            self.running = false;
            self.selected = None;
        } else if self.selected.as_deref() == Some(name) {
            self.selected = None;
        }
        self.sources.remove(name)
    }

    /// Get the names of all registered sources.
    pub fn source_names(&self) -> Vec<&str> {
        self.sources.keys().map(String::as_str).collect()
    }

    /// Select a source by name.
    ///
    /// # Errors
    ///
    /// Returns `SourceError::DeviceNotFound` if the name is not registered.
    pub fn select(&mut self, name: &str) -> Result<(), SourceError> {
        if !self.sources.contains_key(name) {
            return Err(SourceError::DeviceNotFound(name.to_string()));
        }
        // Stop before switching — force running=false even if stop fails
        if self.running && self.selected.as_deref() != Some(name) {
            if let Err(e) = self.stop() {
                tracing::warn!("failed to stop source during reselect: {e}");
            }
            // Ensure running is false regardless of stop() result
            // to prevent state drift
            self.running = false;
        }
        self.selected = Some(name.to_string());
        Ok(())
    }

    /// Get the currently selected source name.
    pub fn selected(&self) -> Option<&str> {
        self.selected.as_deref()
    }

    /// Start the selected source.
    ///
    /// # Errors
    ///
    /// Returns `SourceError` if no source is selected or start fails.
    pub fn start(&mut self) -> Result<(), SourceError> {
        self.with_selected_source(|source| source.start())?;
        self.running = true;
        Ok(())
    }

    /// Stop the selected source.
    ///
    /// # Errors
    ///
    /// Returns `SourceError` if no source is selected or stop fails.
    pub fn stop(&mut self) -> Result<(), SourceError> {
        self.with_selected_source(|source| source.stop())?;
        self.running = false;
        Ok(())
    }

    /// Tune the selected source.
    ///
    /// # Errors
    ///
    /// Returns `SourceError` if no source is selected or tune fails.
    pub fn tune(&mut self, frequency_hz: f64) -> Result<(), SourceError> {
        self.with_selected_source(|source| source.tune(frequency_hz))
    }

    /// Whether a source is currently running.
    pub fn is_running(&self) -> bool {
        self.running
    }

    /// Helper: look up the selected source and apply a closure.
    fn with_selected_source<F, R>(&mut self, f: F) -> Result<R, SourceError>
    where
        F: FnOnce(&mut dyn Source) -> Result<R, SourceError>,
    {
        let name = self
            .selected
            .as_ref()
            .ok_or(SourceError::NotRunning)?
            .clone();
        let source = self
            .sources
            .get_mut(&name)
            .ok_or(SourceError::DeviceNotFound(name))?;
        f(source.as_mut())
    }
}

impl Default for SourceManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    struct MockSource {
        name: String,
        started: bool,
        freq: f64,
    }

    impl MockSource {
        fn new(name: &str) -> Self {
            Self {
                name: name.to_string(),
                started: false,
                freq: 0.0,
            }
        }
    }

    impl Source for MockSource {
        fn name(&self) -> &str {
            &self.name
        }
        fn start(&mut self) -> Result<(), SourceError> {
            self.started = true;
            Ok(())
        }
        fn stop(&mut self) -> Result<(), SourceError> {
            self.started = false;
            Ok(())
        }
        fn tune(&mut self, frequency_hz: f64) -> Result<(), SourceError> {
            self.freq = frequency_hz;
            Ok(())
        }
        fn sample_rates(&self) -> &[f64] {
            &[48_000.0, 2_400_000.0]
        }
        fn sample_rate(&self) -> f64 {
            2_400_000.0
        }
        fn set_sample_rate(&mut self, _rate: f64) -> Result<(), SourceError> {
            Ok(())
        }
        fn read_samples(&mut self, output: &mut [Complex]) -> Result<usize, SourceError> {
            for s in output.iter_mut() {
                *s = Complex::default();
            }
            Ok(output.len())
        }
    }

    #[test]
    fn test_register_and_select() {
        let mut mgr = SourceManager::new();
        mgr.register(Box::new(MockSource::new("Test"))).unwrap();
        assert_eq!(mgr.source_names(), vec!["Test"]);
        mgr.select("Test").unwrap();
        assert_eq!(mgr.selected(), Some("Test"));
    }

    #[test]
    fn test_select_not_found() {
        let mut mgr = SourceManager::new();
        assert!(mgr.select("NonExistent").is_err());
    }

    #[test]
    fn test_start_stop() {
        let mut mgr = SourceManager::new();
        mgr.register(Box::new(MockSource::new("Test"))).unwrap();
        mgr.select("Test").unwrap();
        mgr.start().unwrap();
        assert!(mgr.is_running());
        mgr.stop().unwrap();
        assert!(!mgr.is_running());
    }

    #[test]
    fn test_tune() {
        let mut mgr = SourceManager::new();
        mgr.register(Box::new(MockSource::new("Test"))).unwrap();
        mgr.select("Test").unwrap();
        mgr.tune(100_000_000.0).unwrap();
    }

    #[test]
    fn test_start_no_selection() {
        let mut mgr = SourceManager::new();
        assert!(mgr.start().is_err());
    }

    #[test]
    fn test_duplicate_register() {
        let mut mgr = SourceManager::new();
        mgr.register(Box::new(MockSource::new("Test"))).unwrap();
        assert!(mgr.register(Box::new(MockSource::new("Test"))).is_err());
    }

    #[test]
    fn test_unregister() {
        let mut mgr = SourceManager::new();
        mgr.register(Box::new(MockSource::new("Test"))).unwrap();
        mgr.select("Test").unwrap();
        mgr.unregister("Test");
        assert!(mgr.selected().is_none());
        assert!(mgr.source_names().is_empty());
    }

    #[test]
    fn test_unregister_stops_running_source() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};

        struct TrackingSource {
            stopped: Arc<AtomicBool>,
        }
        impl Source for TrackingSource {
            #[allow(clippy::unnecessary_literal_bound)]
            fn name(&self) -> &str {
                "Tracker"
            }
            fn start(&mut self) -> Result<(), SourceError> {
                Ok(())
            }
            fn stop(&mut self) -> Result<(), SourceError> {
                self.stopped.store(true, Ordering::Relaxed);
                Ok(())
            }
            fn tune(&mut self, _: f64) -> Result<(), SourceError> {
                Ok(())
            }
            fn sample_rates(&self) -> &[f64] {
                &[]
            }
            fn sample_rate(&self) -> f64 {
                48_000.0
            }
            fn set_sample_rate(&mut self, _: f64) -> Result<(), SourceError> {
                Ok(())
            }
            fn read_samples(&mut self, output: &mut [Complex]) -> Result<usize, SourceError> {
                for s in output.iter_mut() {
                    *s = Complex::default();
                }
                Ok(output.len())
            }
        }

        let stopped = Arc::new(AtomicBool::new(false));
        let source = TrackingSource {
            stopped: Arc::clone(&stopped),
        };

        let mut mgr = SourceManager::new();
        mgr.register(Box::new(source)).unwrap();
        mgr.select("Tracker").unwrap();
        mgr.start().unwrap();
        assert!(mgr.is_running());

        mgr.unregister("Tracker");
        assert!(
            stopped.load(Ordering::Relaxed),
            "source should have been stopped"
        );
        assert!(!mgr.is_running());
    }
}
