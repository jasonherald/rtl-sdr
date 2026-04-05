//! VFO manager — multi-VFO creation and parameter management.
//!
//! Ports SDR++ `VFOManager`. Each VFO extracts a channel from the
//! wideband IQ stream at a given frequency offset and bandwidth.

use sdr_types::DspError;
use std::collections::HashMap;

/// Default minimum VFO bandwidth in Hz.
const DEFAULT_MIN_BANDWIDTH: f64 = 1_000.0;

/// Parameters for a Virtual Frequency Oscillator.
#[derive(Clone, Debug)]
pub struct VfoParams {
    /// Name identifier.
    pub name: String,
    /// Frequency offset from center in Hz.
    pub offset: f64,
    /// Channel bandwidth in Hz.
    pub bandwidth: f64,
    /// Minimum allowed bandwidth in Hz.
    pub min_bandwidth: f64,
    /// Maximum allowed bandwidth in Hz.
    pub max_bandwidth: f64,
    /// Output sample rate in Hz.
    pub sample_rate: f64,
    /// Frequency snap interval in Hz (0 = no snap).
    pub snap_interval: f64,
}

impl VfoParams {
    /// Create VFO params with defaults.
    pub fn new(name: &str, offset: f64, bandwidth: f64, sample_rate: f64) -> Self {
        Self {
            name: name.to_string(),
            offset,
            bandwidth,
            min_bandwidth: DEFAULT_MIN_BANDWIDTH,
            max_bandwidth: sample_rate,
            sample_rate,
            snap_interval: 0.0,
        }
    }
}

/// Manages multiple VFOs, each extracting a channel from the IQ stream.
///
/// Ports SDR++ `VFOManager`.
pub struct VfoManager {
    vfos: HashMap<String, VfoParams>,
}

impl VfoManager {
    /// Create a new empty VFO manager.
    pub fn new() -> Self {
        Self {
            vfos: HashMap::new(),
        }
    }

    /// Create a new VFO with the given parameters.
    ///
    /// # Errors
    ///
    /// Returns `DspError::InvalidParameter` if a VFO with this name already exists.
    pub fn create_vfo(&mut self, params: VfoParams) -> Result<(), DspError> {
        if self.vfos.contains_key(&params.name) {
            return Err(DspError::InvalidParameter(format!(
                "VFO already exists: {}",
                params.name
            )));
        }
        self.vfos.insert(params.name.clone(), params);
        Ok(())
    }

    /// Delete a VFO by name.
    ///
    /// # Errors
    ///
    /// Returns `DspError::InvalidParameter` if the VFO is not found.
    pub fn delete_vfo(&mut self, name: &str) -> Result<(), DspError> {
        self.vfos
            .remove(name)
            .map(|_| ())
            .ok_or_else(|| DspError::InvalidParameter(format!("VFO not found: {name}")))
    }

    /// Check if a VFO exists.
    pub fn vfo_exists(&self, name: &str) -> bool {
        self.vfos.contains_key(name)
    }

    /// Get VFO parameters.
    pub fn get_vfo(&self, name: &str) -> Option<&VfoParams> {
        self.vfos.get(name)
    }

    /// Set VFO frequency offset.
    ///
    /// # Errors
    ///
    /// Returns `DspError::InvalidParameter` if the VFO is not found.
    pub fn set_offset(&mut self, name: &str, offset: f64) -> Result<(), DspError> {
        self.get_vfo_mut(name)?.offset = offset;
        Ok(())
    }

    /// Set VFO bandwidth, clamped to min/max limits.
    ///
    /// # Errors
    ///
    /// Returns `DspError::InvalidParameter` if the VFO is not found.
    pub fn set_bandwidth(&mut self, name: &str, bandwidth: f64) -> Result<(), DspError> {
        let vfo = self.get_vfo_mut(name)?;
        vfo.bandwidth = bandwidth.clamp(vfo.min_bandwidth, vfo.max_bandwidth);
        Ok(())
    }

    /// Set VFO bandwidth limits.
    ///
    /// # Errors
    ///
    /// Returns `DspError::InvalidParameter` if the VFO is not found or `min > max`.
    pub fn set_bandwidth_limits(&mut self, name: &str, min: f64, max: f64) -> Result<(), DspError> {
        if min > max {
            return Err(DspError::InvalidParameter(format!(
                "min bandwidth ({min}) must be <= max bandwidth ({max})"
            )));
        }
        let vfo = self.get_vfo_mut(name)?;
        vfo.min_bandwidth = min;
        vfo.max_bandwidth = max;
        vfo.bandwidth = vfo.bandwidth.clamp(min, max);
        Ok(())
    }

    /// Get all VFO names.
    pub fn vfo_names(&self) -> Vec<&str> {
        self.vfos.keys().map(String::as_str).collect()
    }

    /// Number of VFOs.
    pub fn count(&self) -> usize {
        self.vfos.len()
    }

    fn get_vfo_mut(&mut self, name: &str) -> Result<&mut VfoParams, DspError> {
        self.vfos
            .get_mut(name)
            .ok_or_else(|| DspError::InvalidParameter(format!("VFO not found: {name}")))
    }
}

impl Default for VfoManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::float_cmp)]
mod tests {
    use super::*;

    #[test]
    fn test_create_and_delete() {
        let mut mgr = VfoManager::new();
        let params = VfoParams::new("vfo1", 10_000.0, 200_000.0, 48_000.0);
        mgr.create_vfo(params).unwrap();
        assert!(mgr.vfo_exists("vfo1"));
        assert_eq!(mgr.count(), 1);
        mgr.delete_vfo("vfo1").unwrap();
        assert!(!mgr.vfo_exists("vfo1"));
    }

    #[test]
    fn test_duplicate_create() {
        let mut mgr = VfoManager::new();
        mgr.create_vfo(VfoParams::new("vfo1", 0.0, 200_000.0, 48_000.0))
            .unwrap();
        assert!(
            mgr.create_vfo(VfoParams::new("vfo1", 0.0, 200_000.0, 48_000.0))
                .is_err()
        );
    }

    #[test]
    fn test_delete_not_found() {
        let mut mgr = VfoManager::new();
        assert!(mgr.delete_vfo("nonexistent").is_err());
    }

    #[test]
    fn test_set_offset() {
        let mut mgr = VfoManager::new();
        mgr.create_vfo(VfoParams::new("vfo1", 0.0, 200_000.0, 48_000.0))
            .unwrap();
        mgr.set_offset("vfo1", 5_000.0).unwrap();
        assert_eq!(mgr.get_vfo("vfo1").unwrap().offset, 5_000.0);
    }

    #[test]
    fn test_set_bandwidth_clamped() {
        let mut mgr = VfoManager::new();
        mgr.create_vfo(VfoParams::new("vfo1", 0.0, 200_000.0, 48_000.0))
            .unwrap();
        mgr.set_bandwidth_limits("vfo1", 5_000.0, 100_000.0)
            .unwrap();
        // Set below min
        mgr.set_bandwidth("vfo1", 1_000.0).unwrap();
        assert_eq!(mgr.get_vfo("vfo1").unwrap().bandwidth, 5_000.0);
        // Set above max
        mgr.set_bandwidth("vfo1", 500_000.0).unwrap();
        assert_eq!(mgr.get_vfo("vfo1").unwrap().bandwidth, 100_000.0);
    }

    #[test]
    fn test_vfo_names() {
        let mut mgr = VfoManager::new();
        mgr.create_vfo(VfoParams::new("a", 0.0, 200_000.0, 48_000.0))
            .unwrap();
        mgr.create_vfo(VfoParams::new("b", 0.0, 200_000.0, 48_000.0))
            .unwrap();
        let mut names = mgr.vfo_names();
        names.sort_unstable();
        assert_eq!(names, vec!["a", "b"]);
    }
}
