//! Sink manager — registration and lifecycle for audio/network sinks.
//!
//! Ports SDR++ `SinkManager`. Manages output sinks (audio, network)
//! with volume control and stream routing.

use sdr_types::SinkError;
use std::collections::HashMap;

/// Trait for an audio/data output sink.
///
/// Implemented by each sink module (audio, network).
pub trait Sink: Send {
    /// Human-readable name (e.g., "Audio", "Network").
    fn name(&self) -> &str;

    /// Start the sink.
    fn start(&mut self) -> Result<(), SinkError>;

    /// Stop the sink.
    fn stop(&mut self) -> Result<(), SinkError>;

    /// Set the sample rate.
    fn set_sample_rate(&mut self, rate: f64);

    /// Current sample rate.
    fn sample_rate(&self) -> f64;
}

/// An audio stream managed by the sink manager.
pub struct ManagedStream {
    /// Name of this stream.
    pub name: String,
    /// Volume (0.0 to 1.0).
    pub volume: f32,
    /// Sample rate.
    pub sample_rate: f64,
    /// Active sink name.
    pub active_sink: Option<String>,
}

/// Manages output sinks and audio streams.
///
/// Ports SDR++ `SinkManager`.
pub struct SinkManager {
    sinks: HashMap<String, Box<dyn Sink>>,
    streams: HashMap<String, ManagedStream>,
}

impl SinkManager {
    /// Create a new empty sink manager.
    pub fn new() -> Self {
        Self {
            sinks: HashMap::new(),
            streams: HashMap::new(),
        }
    }

    /// Register a sink.
    ///
    /// # Errors
    ///
    /// Returns `SinkError` if a sink with this name already exists.
    pub fn register_sink(&mut self, sink: Box<dyn Sink>) -> Result<(), SinkError> {
        let name = sink.name().to_string();
        if self.sinks.contains_key(&name) {
            return Err(SinkError::OpenFailed(format!(
                "sink already registered: {name}"
            )));
        }
        self.sinks.insert(name, sink);
        Ok(())
    }

    /// Unregister a sink by name.
    pub fn unregister_sink(&mut self, name: &str) -> Option<Box<dyn Sink>> {
        self.sinks.remove(name)
    }

    /// Get the names of all registered sinks.
    pub fn sink_names(&self) -> Vec<&str> {
        self.sinks.keys().map(String::as_str).collect()
    }

    /// Register a named audio stream.
    pub fn register_stream(&mut self, name: &str, sample_rate: f64) {
        self.streams.insert(
            name.to_string(),
            ManagedStream {
                name: name.to_string(),
                volume: 1.0,
                sample_rate,
                active_sink: None,
            },
        );
    }

    /// Unregister a stream.
    pub fn unregister_stream(&mut self, name: &str) {
        self.streams.remove(name);
    }

    /// Get stream names.
    pub fn stream_names(&self) -> Vec<&str> {
        self.streams.keys().map(String::as_str).collect()
    }

    /// Set the volume for a stream (0.0 to 1.0).
    ///
    /// # Errors
    ///
    /// Returns `SinkError` if the stream is not found.
    pub fn set_volume(&mut self, stream_name: &str, volume: f32) -> Result<(), SinkError> {
        let stream = self
            .streams
            .get_mut(stream_name)
            .ok_or_else(|| SinkError::DeviceNotFound(stream_name.to_string()))?;
        stream.volume = volume.clamp(0.0, 1.0);
        Ok(())
    }

    /// Get the volume for a stream.
    pub fn volume(&self, stream_name: &str) -> Option<f32> {
        self.streams.get(stream_name).map(|s| s.volume)
    }

    /// Set the active sink for a stream.
    ///
    /// # Errors
    ///
    /// Returns `SinkError` if stream or sink is not found.
    pub fn set_stream_sink(&mut self, stream_name: &str, sink_name: &str) -> Result<(), SinkError> {
        if !self.sinks.contains_key(sink_name) {
            return Err(SinkError::DeviceNotFound(sink_name.to_string()));
        }
        let stream = self
            .streams
            .get_mut(stream_name)
            .ok_or_else(|| SinkError::DeviceNotFound(stream_name.to_string()))?;
        stream.active_sink = Some(sink_name.to_string());
        Ok(())
    }
}

impl Default for SinkManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    struct MockSink {
        name: String,
    }

    impl MockSink {
        fn new(name: &str) -> Self {
            Self {
                name: name.to_string(),
            }
        }
    }

    impl Sink for MockSink {
        fn name(&self) -> &str {
            &self.name
        }
        fn start(&mut self) -> Result<(), SinkError> {
            Ok(())
        }
        fn stop(&mut self) -> Result<(), SinkError> {
            Ok(())
        }
        fn set_sample_rate(&mut self, _rate: f64) {}
        fn sample_rate(&self) -> f64 {
            48_000.0
        }
    }

    #[test]
    fn test_register_sink() {
        let mut mgr = SinkManager::new();
        mgr.register_sink(Box::new(MockSink::new("Audio"))).unwrap();
        assert_eq!(mgr.sink_names(), vec!["Audio"]);
    }

    #[test]
    fn test_duplicate_sink() {
        let mut mgr = SinkManager::new();
        mgr.register_sink(Box::new(MockSink::new("Audio"))).unwrap();
        assert!(mgr.register_sink(Box::new(MockSink::new("Audio"))).is_err());
    }

    #[test]
    fn test_stream_lifecycle() {
        let mut mgr = SinkManager::new();
        mgr.register_stream("main", 48_000.0);
        assert_eq!(mgr.stream_names(), vec!["main"]);
        mgr.unregister_stream("main");
        assert!(mgr.stream_names().is_empty());
    }

    #[test]
    fn test_volume() {
        let mut mgr = SinkManager::new();
        mgr.register_stream("main", 48_000.0);
        mgr.set_volume("main", 0.5).unwrap();
        assert!((mgr.volume("main").unwrap() - 0.5).abs() < f32::EPSILON);
    }

    #[test]
    fn test_volume_clamp() {
        let mut mgr = SinkManager::new();
        mgr.register_stream("main", 48_000.0);
        mgr.set_volume("main", 2.0).unwrap();
        assert!((mgr.volume("main").unwrap() - 1.0).abs() < f32::EPSILON);
    }

    #[test]
    fn test_set_stream_sink() {
        let mut mgr = SinkManager::new();
        mgr.register_sink(Box::new(MockSink::new("Audio"))).unwrap();
        mgr.register_stream("main", 48_000.0);
        mgr.set_stream_sink("main", "Audio").unwrap();
    }

    #[test]
    fn test_set_stream_sink_not_found() {
        let mut mgr = SinkManager::new();
        mgr.register_stream("main", 48_000.0);
        assert!(mgr.set_stream_sink("main", "NonExistent").is_err());
    }
}
