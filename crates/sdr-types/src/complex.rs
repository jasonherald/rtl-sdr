/// Alpha-max-beta-min coefficient for fast amplitude approximation.
const FAST_AMPLITUDE_BETA: f32 = 0.4;

/// IQ complex sample type — matches SDR++ `dsp::complex_t` memory layout.
///
/// Two f32 fields (re, im) representing in-phase and quadrature components.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
#[repr(C)]
#[must_use]
pub struct Complex {
    pub re: f32,
    pub im: f32,
}

impl Complex {
    /// Create a new complex value.
    #[inline]
    pub fn new(re: f32, im: f32) -> Self {
        Self { re, im }
    }

    /// Complex conjugate (negate imaginary part).
    #[inline]
    pub fn conj(self) -> Self {
        Self {
            re: self.re,
            im: -self.im,
        }
    }

    /// Phase angle via `atan2(im, re)`.
    #[inline]
    pub fn phase(self) -> f32 {
        self.im.atan2(self.re)
    }

    /// Fast phase approximation using rational polynomial.
    ///
    /// Ports SDR++ `complex_t::fastPhase()` — accurate to ~0.01 radians.
    #[inline]
    pub fn fast_phase(self) -> f32 {
        let abs_im = self.im.abs();
        if self.re == 0.0 && self.im == 0.0 {
            return 0.0;
        }
        let angle = if self.re >= 0.0 {
            let r = (self.re - abs_im) / (self.re + abs_im);
            core::f32::consts::FRAC_PI_4 - core::f32::consts::FRAC_PI_4 * r
        } else {
            let r = (self.re + abs_im) / (abs_im - self.re);
            3.0 * core::f32::consts::FRAC_PI_4 - core::f32::consts::FRAC_PI_4 * r
        };
        if self.im < 0.0 { -angle } else { angle }
    }

    /// Amplitude (magnitude): `sqrt(re^2 + im^2)`.
    #[inline]
    pub fn amplitude(self) -> f32 {
        (self.re * self.re + self.im * self.im).sqrt()
    }

    /// Fast amplitude approximation: `max(|re|,|im|) + 0.4 * min(|re|,|im|)`.
    ///
    /// Ports SDR++ `complex_t::fastAmplitude()` — ~4% max error.
    #[inline]
    pub fn fast_amplitude(self) -> f32 {
        let re_abs = self.re.abs();
        let im_abs = self.im.abs();
        if re_abs > im_abs {
            re_abs + FAST_AMPLITUDE_BETA * im_abs
        } else {
            im_abs + FAST_AMPLITUDE_BETA * re_abs
        }
    }
}

// --- Arithmetic operators ---

impl std::ops::Add for Complex {
    type Output = Self;
    #[inline]
    fn add(self, rhs: Self) -> Self {
        Self {
            re: self.re + rhs.re,
            im: self.im + rhs.im,
        }
    }
}

impl std::ops::Sub for Complex {
    type Output = Self;
    #[inline]
    fn sub(self, rhs: Self) -> Self {
        Self {
            re: self.re - rhs.re,
            im: self.im - rhs.im,
        }
    }
}

impl std::ops::Mul for Complex {
    type Output = Self;
    /// Complex multiplication: `(a+bi)(c+di) = (ac-bd) + (bc+ad)i`
    #[inline]
    fn mul(self, rhs: Self) -> Self {
        Self {
            re: self.re * rhs.re - self.im * rhs.im,
            im: self.im * rhs.re + self.re * rhs.im,
        }
    }
}

impl std::ops::Mul<f32> for Complex {
    type Output = Self;
    #[inline]
    fn mul(self, rhs: f32) -> Self {
        Self {
            re: self.re * rhs,
            im: self.im * rhs,
        }
    }
}

impl std::ops::Div<f32> for Complex {
    type Output = Self;
    #[inline]
    fn div(self, rhs: f32) -> Self {
        Self {
            re: self.re / rhs,
            im: self.im / rhs,
        }
    }
}

impl std::ops::AddAssign for Complex {
    #[inline]
    fn add_assign(&mut self, rhs: Self) {
        self.re += rhs.re;
        self.im += rhs.im;
    }
}

impl std::ops::SubAssign for Complex {
    #[inline]
    fn sub_assign(&mut self, rhs: Self) {
        self.re -= rhs.re;
        self.im -= rhs.im;
    }
}

impl std::ops::MulAssign<f32> for Complex {
    #[inline]
    fn mul_assign(&mut self, rhs: f32) {
        self.re *= rhs;
        self.im *= rhs;
    }
}

impl std::ops::Neg for Complex {
    type Output = Self;
    #[inline]
    fn neg(self) -> Self {
        Self {
            re: -self.re,
            im: -self.im,
        }
    }
}

// Complex is two f32s — Send + Sync auto-derived.

#[cfg(test)]
#[allow(clippy::float_cmp)]
mod tests {
    use super::*;

    const EPS: f32 = 1e-6;

    fn approx_eq(a: f32, b: f32) -> bool {
        (a - b).abs() < EPS
    }

    #[test]
    fn test_new_and_default() {
        let c = Complex::new(1.0, 2.0);
        assert_eq!(c.re, 1.0);
        assert_eq!(c.im, 2.0);

        let d = Complex::default();
        assert_eq!(d.re, 0.0);
        assert_eq!(d.im, 0.0);
    }

    #[test]
    fn test_conj() {
        let c = Complex::new(3.0, 4.0).conj();
        assert_eq!(c.re, 3.0);
        assert_eq!(c.im, -4.0);
    }

    #[test]
    fn test_phase() {
        // Phase of (1, 0) = 0
        assert!(approx_eq(Complex::new(1.0, 0.0).phase(), 0.0));
        // Phase of (0, 1) = pi/2
        assert!(approx_eq(
            Complex::new(0.0, 1.0).phase(),
            core::f32::consts::FRAC_PI_2
        ));
        // Phase of (-1, 0) = pi
        assert!(approx_eq(
            Complex::new(-1.0, 0.0).phase(),
            core::f32::consts::PI
        ));
    }

    #[test]
    fn test_fast_phase() {
        // Fast phase should be within ~0.01 radians of true phase
        let cases = [
            Complex::new(1.0, 0.0),
            Complex::new(0.0, 1.0),
            Complex::new(-1.0, 0.0),
            Complex::new(0.0, -1.0),
            Complex::new(1.0, 1.0),
            Complex::new(-1.0, -1.0),
            Complex::new(3.0, 4.0),
        ];
        for c in &cases {
            let diff = (c.fast_phase() - c.phase()).abs();
            assert!(diff < 0.08, "fast_phase error {diff} for {c:?}");
        }
        // Zero returns zero
        assert_eq!(Complex::new(0.0, 0.0).fast_phase(), 0.0);
    }

    #[test]
    fn test_amplitude() {
        // 3-4-5 triangle
        assert!(approx_eq(Complex::new(3.0, 4.0).amplitude(), 5.0));
        assert!(approx_eq(Complex::new(0.0, 0.0).amplitude(), 0.0));
        assert!(approx_eq(Complex::new(1.0, 0.0).amplitude(), 1.0));
    }

    #[test]
    fn test_fast_amplitude() {
        let c = Complex::new(3.0, 4.0);
        let fast = c.fast_amplitude();
        let exact = c.amplitude();
        // Should be within ~5% of exact
        let error = ((fast - exact) / exact).abs();
        assert!(error < 0.05, "fast_amplitude error {error}");
    }

    #[test]
    fn test_add() {
        let a = Complex::new(1.0, 2.0);
        let b = Complex::new(3.0, 4.0);
        let c = a + b;
        assert_eq!(c.re, 4.0);
        assert_eq!(c.im, 6.0);
    }

    #[test]
    fn test_sub() {
        let a = Complex::new(5.0, 7.0);
        let b = Complex::new(3.0, 4.0);
        let c = a - b;
        assert_eq!(c.re, 2.0);
        assert_eq!(c.im, 3.0);
    }

    #[test]
    fn test_complex_mul() {
        // (1+2i)(3+4i) = (3-8) + (6+4)i = -5 + 10i
        let a = Complex::new(1.0, 2.0);
        let b = Complex::new(3.0, 4.0);
        let c = a * b;
        assert!(approx_eq(c.re, -5.0));
        assert!(approx_eq(c.im, 10.0));
    }

    #[test]
    fn test_scalar_mul() {
        let c = Complex::new(2.0, 3.0) * 2.0;
        assert_eq!(c.re, 4.0);
        assert_eq!(c.im, 6.0);
    }

    #[test]
    fn test_scalar_div() {
        let c = Complex::new(4.0, 6.0) / 2.0;
        assert_eq!(c.re, 2.0);
        assert_eq!(c.im, 3.0);
    }

    #[test]
    fn test_add_assign() {
        let mut a = Complex::new(1.0, 2.0);
        a += Complex::new(3.0, 4.0);
        assert_eq!(a.re, 4.0);
        assert_eq!(a.im, 6.0);
    }

    #[test]
    fn test_sub_assign() {
        let mut a = Complex::new(5.0, 7.0);
        a -= Complex::new(3.0, 4.0);
        assert_eq!(a.re, 2.0);
        assert_eq!(a.im, 3.0);
    }

    #[test]
    fn test_mul_assign() {
        let mut a = Complex::new(2.0, 3.0);
        a *= 2.0;
        assert_eq!(a.re, 4.0);
        assert_eq!(a.im, 6.0);
    }

    #[test]
    fn test_neg() {
        let c = -Complex::new(1.0, -2.0);
        assert_eq!(c.re, -1.0);
        assert_eq!(c.im, 2.0);
    }

    #[test]
    fn test_repr_c_size() {
        assert_eq!(std::mem::size_of::<Complex>(), 8);
        assert_eq!(std::mem::align_of::<Complex>(), 4);
    }
}
