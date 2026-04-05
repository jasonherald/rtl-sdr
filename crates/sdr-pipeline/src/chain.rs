//! Processor chain with enable/disable support.
//!
//! Ports SDR++ `dsp::chain`. A chain is an ordered list of processors
//! that can be individually enabled or disabled. When a processor is
//! disabled, data flows around it to the next enabled processor.

use sdr_types::DspError;

/// Boxed processor function type for chain steps.
type ProcessorFn<T> = Box<dyn FnMut(&[T], &mut [T]) -> Result<usize, DspError> + Send>;

/// A processing step in a chain.
///
/// Each step wraps a boxed processor function and tracks its enabled state.
pub struct ChainStep<T> {
    /// The processor function: takes input slice, writes to output, returns count.
    processor: ProcessorFn<T>,
    /// Whether this step is enabled.
    enabled: bool,
    /// Name for debugging.
    name: String,
}

/// Ordered chain of processors with enable/disable support.
///
/// When a processor is disabled, input passes through to the next
/// enabled processor unchanged. All enabled processors are applied
/// in order.
pub struct Chain<T: Copy + Default> {
    steps: Vec<ChainStep<T>>,
    buf_a: Vec<T>,
    buf_b: Vec<T>,
}

impl<T: Copy + Default> Chain<T> {
    /// Create a new empty chain.
    pub fn new() -> Self {
        Self {
            steps: Vec::new(),
            buf_a: Vec::new(),
            buf_b: Vec::new(),
        }
    }

    /// Add a named processor to the chain (initially enabled).
    pub fn add<F>(&mut self, name: &str, processor: F)
    where
        F: FnMut(&[T], &mut [T]) -> Result<usize, DspError> + Send + 'static,
    {
        self.steps.push(ChainStep {
            processor: Box::new(processor),
            enabled: true,
            name: name.to_string(),
        });
    }

    /// Enable a processor by name.
    ///
    /// # Errors
    ///
    /// Returns `DspError::InvalidParameter` if the name is not found.
    pub fn enable(&mut self, name: &str) -> Result<(), DspError> {
        self.find_step_mut(name)?.enabled = true;
        Ok(())
    }

    /// Disable a processor by name.
    ///
    /// # Errors
    ///
    /// Returns `DspError::InvalidParameter` if the name is not found.
    pub fn disable(&mut self, name: &str) -> Result<(), DspError> {
        self.find_step_mut(name)?.enabled = false;
        Ok(())
    }

    /// Check if a processor is enabled.
    ///
    /// # Errors
    ///
    /// Returns `DspError::InvalidParameter` if the name is not found.
    pub fn is_enabled(&self, name: &str) -> Result<bool, DspError> {
        Ok(self.find_step(name)?.enabled)
    }

    /// Number of steps in the chain.
    pub fn len(&self) -> usize {
        self.steps.len()
    }

    /// Whether the chain has no steps.
    pub fn is_empty(&self) -> bool {
        self.steps.is_empty()
    }

    /// Process samples through all enabled processors in order.
    ///
    /// Disabled processors are skipped (input passes through unchanged).
    /// Returns the number of output samples.
    ///
    /// # Errors
    ///
    /// Returns `DspError::BufferTooSmall` if `output` is too small.
    pub fn process(&mut self, input: &[T], output: &mut [T]) -> Result<usize, DspError> {
        if output.len() < input.len() {
            return Err(DspError::BufferTooSmall {
                need: input.len(),
                got: output.len(),
            });
        }

        // Count enabled steps
        let enabled_count = self.steps.iter().filter(|s| s.enabled).count();

        if enabled_count == 0 {
            // No processors enabled — passthrough
            output[..input.len()].copy_from_slice(input);
            return Ok(input.len());
        }

        // Process through enabled steps using ping-pong buffers (a ↔ b)
        self.buf_a.resize(input.len(), T::default());
        self.buf_b.resize(input.len(), T::default());
        self.buf_a[..input.len()].copy_from_slice(input);

        let mut current_count = input.len();
        let mut src_is_a = true;

        for step in &mut self.steps {
            if !step.enabled {
                continue;
            }
            if src_is_a {
                current_count = (step.processor)(&self.buf_a[..current_count], &mut self.buf_b)?;
            } else {
                current_count = (step.processor)(&self.buf_b[..current_count], &mut self.buf_a)?;
            }
            src_is_a = !src_is_a;
        }

        // Copy result to output from whichever buffer has the final data
        let result = if src_is_a {
            &self.buf_a[..current_count]
        } else {
            &self.buf_b[..current_count]
        };
        output[..current_count].copy_from_slice(result);

        Ok(current_count)
    }

    fn find_step(&self, name: &str) -> Result<&ChainStep<T>, DspError> {
        self.steps
            .iter()
            .find(|s| s.name == name)
            .ok_or_else(|| DspError::InvalidParameter(format!("step not found: {name}")))
    }

    fn find_step_mut(&mut self, name: &str) -> Result<&mut ChainStep<T>, DspError> {
        self.steps
            .iter_mut()
            .find(|s| s.name == name)
            .ok_or_else(|| DspError::InvalidParameter(format!("step not found: {name}")))
    }
}

impl<T: Copy + Default> Default for Chain<T> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::float_cmp)]
mod tests {
    use super::*;

    #[test]
    fn test_empty_chain_passthrough() {
        let mut chain: Chain<f32> = Chain::new();
        let input = [1.0, 2.0, 3.0, 4.0];
        let mut output = [0.0_f32; 4];
        let count = chain.process(&input, &mut output).unwrap();
        assert_eq!(count, 4);
        assert_eq!(output, input);
    }

    #[test]
    fn test_single_processor() {
        let mut chain: Chain<f32> = Chain::new();
        chain.add("double", |input: &[f32], output: &mut [f32]| {
            for (i, &v) in input.iter().enumerate() {
                output[i] = v * 2.0;
            }
            Ok(input.len())
        });

        let input = [1.0, 2.0, 3.0];
        let mut output = [0.0_f32; 3];
        let count = chain.process(&input, &mut output).unwrap();
        assert_eq!(count, 3);
        assert_eq!(output, [2.0, 4.0, 6.0]);
    }

    #[test]
    fn test_chained_processors() {
        let mut chain: Chain<f32> = Chain::new();
        chain.add("add_one", |input: &[f32], output: &mut [f32]| {
            for (i, &v) in input.iter().enumerate() {
                output[i] = v + 1.0;
            }
            Ok(input.len())
        });
        chain.add("double", |input: &[f32], output: &mut [f32]| {
            for (i, &v) in input.iter().enumerate() {
                output[i] = v * 2.0;
            }
            Ok(input.len())
        });

        let input = [1.0, 2.0, 3.0];
        let mut output = [0.0_f32; 3];
        let count = chain.process(&input, &mut output).unwrap();
        assert_eq!(count, 3);
        // (1+1)*2=4, (2+1)*2=6, (3+1)*2=8
        assert_eq!(output, [4.0, 6.0, 8.0]);
    }

    #[test]
    fn test_disable_processor() {
        let mut chain: Chain<f32> = Chain::new();
        chain.add("add_one", |input: &[f32], output: &mut [f32]| {
            for (i, &v) in input.iter().enumerate() {
                output[i] = v + 1.0;
            }
            Ok(input.len())
        });
        chain.add("double", |input: &[f32], output: &mut [f32]| {
            for (i, &v) in input.iter().enumerate() {
                output[i] = v * 2.0;
            }
            Ok(input.len())
        });

        // Disable the first processor
        chain.disable("add_one").unwrap();

        let input = [1.0, 2.0, 3.0];
        let mut output = [0.0_f32; 3];
        let count = chain.process(&input, &mut output).unwrap();
        assert_eq!(count, 3);
        // Only double: 1*2=2, 2*2=4, 3*2=6
        assert_eq!(output, [2.0, 4.0, 6.0]);
    }

    #[test]
    fn test_enable_disable_toggle() {
        let mut chain: Chain<f32> = Chain::new();
        chain.add("negate", |input: &[f32], output: &mut [f32]| {
            for (i, &v) in input.iter().enumerate() {
                output[i] = -v;
            }
            Ok(input.len())
        });

        assert!(chain.is_enabled("negate").unwrap());
        chain.disable("negate").unwrap();
        assert!(!chain.is_enabled("negate").unwrap());
        chain.enable("negate").unwrap();
        assert!(chain.is_enabled("negate").unwrap());
    }

    #[test]
    fn test_all_disabled_passthrough() {
        let mut chain: Chain<f32> = Chain::new();
        chain.add("a", |_: &[f32], _: &mut [f32]| Ok(0));
        chain.add("b", |_: &[f32], _: &mut [f32]| Ok(0));
        chain.disable("a").unwrap();
        chain.disable("b").unwrap();

        let input = [1.0, 2.0, 3.0];
        let mut output = [0.0_f32; 3];
        let count = chain.process(&input, &mut output).unwrap();
        assert_eq!(count, 3);
        assert_eq!(output, input);
    }

    #[test]
    fn test_not_found_error() {
        let mut chain: Chain<f32> = Chain::new();
        assert!(chain.enable("nonexistent").is_err());
        assert!(chain.disable("nonexistent").is_err());
        assert!(chain.is_enabled("nonexistent").is_err());
    }

    #[test]
    fn test_buffer_too_small() {
        let mut chain: Chain<f32> = Chain::new();
        let input = [1.0, 2.0, 3.0];
        let mut output = [0.0_f32; 2];
        assert!(chain.process(&input, &mut output).is_err());
    }
}
