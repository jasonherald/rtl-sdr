//! Stream splitter — fan-out one input to multiple outputs.
//!
//! Ports SDR++ `dsp::routing::Splitter`. Copies input data to all
//! bound output streams.

use sdr_types::DspError;

/// Fan-out splitter that copies input to multiple output buffers.
///
/// Ports SDR++ `dsp::routing::Splitter`. Each call to `process()`
/// copies the input to all registered output buffers.
pub struct Splitter<T: Copy + Default> {
    outputs: Vec<Vec<T>>,
}

impl<T: Copy + Default> Splitter<T> {
    /// Create a new splitter with no outputs.
    pub fn new() -> Self {
        Self {
            outputs: Vec::new(),
        }
    }

    /// Add an output buffer. Returns the index of the new output.
    pub fn add_output(&mut self) -> usize {
        let idx = self.outputs.len();
        self.outputs.push(Vec::new());
        idx
    }

    /// Remove an output by index.
    ///
    /// # Errors
    ///
    /// Returns `DspError::InvalidParameter` if the index is out of bounds.
    pub fn remove_output(&mut self, index: usize) -> Result<(), DspError> {
        if index >= self.outputs.len() {
            return Err(DspError::InvalidParameter(format!(
                "output index {index} out of bounds (have {})",
                self.outputs.len()
            )));
        }
        self.outputs.remove(index);
        Ok(())
    }

    /// Number of outputs.
    pub fn output_count(&self) -> usize {
        self.outputs.len()
    }

    /// Process input by copying to all output buffers.
    ///
    /// Each output buffer is resized to hold the input data.
    pub fn process(&mut self, input: &[T]) {
        for output in &mut self.outputs {
            output.resize(input.len(), T::default());
            output.copy_from_slice(input);
        }
    }

    /// Get a reference to an output buffer.
    ///
    /// Valid after `process()` has been called.
    pub fn output(&self, index: usize) -> Option<&[T]> {
        self.outputs.get(index).map(Vec::as_slice)
    }
}

impl<T: Copy + Default> Default for Splitter<T> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::float_cmp)]
mod tests {
    use super::*;

    #[test]
    fn test_no_outputs() {
        let mut splitter: Splitter<f32> = Splitter::new();
        splitter.process(&[1.0, 2.0, 3.0]);
        assert_eq!(splitter.output_count(), 0);
    }

    #[test]
    fn test_single_output() {
        let mut splitter: Splitter<f32> = Splitter::new();
        let idx = splitter.add_output();
        assert_eq!(idx, 0);

        splitter.process(&[1.0, 2.0, 3.0]);
        let out = splitter.output(0).unwrap();
        assert_eq!(out, &[1.0, 2.0, 3.0]);
    }

    #[test]
    fn test_multiple_outputs() {
        let mut splitter: Splitter<f32> = Splitter::new();
        splitter.add_output();
        splitter.add_output();
        splitter.add_output();
        assert_eq!(splitter.output_count(), 3);

        let input = [10.0, 20.0, 30.0];
        splitter.process(&input);

        for i in 0..3 {
            let out = splitter.output(i).unwrap();
            assert_eq!(out, &input);
        }
    }

    #[test]
    fn test_remove_output() {
        let mut splitter: Splitter<f32> = Splitter::new();
        splitter.add_output();
        splitter.add_output();
        assert_eq!(splitter.output_count(), 2);

        splitter.remove_output(0).unwrap();
        assert_eq!(splitter.output_count(), 1);
    }

    #[test]
    fn test_remove_output_invalid() {
        let mut splitter: Splitter<f32> = Splitter::new();
        assert!(splitter.remove_output(0).is_err());
    }

    #[test]
    fn test_output_out_of_bounds() {
        let splitter: Splitter<f32> = Splitter::new();
        assert!(splitter.output(0).is_none());
    }

    #[test]
    fn test_independent_outputs() {
        let mut splitter: Splitter<f32> = Splitter::new();
        splitter.add_output();
        splitter.add_output();

        // First process
        splitter.process(&[1.0, 2.0]);
        assert_eq!(splitter.output(0).unwrap(), &[1.0, 2.0]);
        assert_eq!(splitter.output(1).unwrap(), &[1.0, 2.0]);

        // Second process with different data
        splitter.process(&[3.0, 4.0, 5.0]);
        assert_eq!(splitter.output(0).unwrap(), &[3.0, 4.0, 5.0]);
        assert_eq!(splitter.output(1).unwrap(), &[3.0, 4.0, 5.0]);
    }
}
