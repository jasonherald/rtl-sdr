/// Stereo audio sample — matches SDR++ `dsp::stereo_t` memory layout.
///
/// Two f32 fields (l, r) representing left and right audio channels.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
#[repr(C)]
#[must_use]
pub struct Stereo {
    pub l: f32,
    pub r: f32,
}

impl Stereo {
    /// Create a new stereo sample.
    #[inline]
    pub fn new(l: f32, r: f32) -> Self {
        Self { l, r }
    }

    /// Create a mono sample (same value in both channels).
    #[inline]
    pub fn mono(v: f32) -> Self {
        Self { l: v, r: v }
    }
}

// --- Arithmetic operators ---

impl std::ops::Add for Stereo {
    type Output = Self;
    #[inline]
    fn add(self, rhs: Self) -> Self {
        Self {
            l: self.l + rhs.l,
            r: self.r + rhs.r,
        }
    }
}

impl std::ops::Sub for Stereo {
    type Output = Self;
    #[inline]
    fn sub(self, rhs: Self) -> Self {
        Self {
            l: self.l - rhs.l,
            r: self.r - rhs.r,
        }
    }
}

impl std::ops::Mul<f32> for Stereo {
    type Output = Self;
    #[inline]
    fn mul(self, rhs: f32) -> Self {
        Self {
            l: self.l * rhs,
            r: self.r * rhs,
        }
    }
}

impl std::ops::Div<f32> for Stereo {
    type Output = Self;
    #[inline]
    fn div(self, rhs: f32) -> Self {
        Self {
            l: self.l / rhs,
            r: self.r / rhs,
        }
    }
}

impl std::ops::AddAssign for Stereo {
    #[inline]
    fn add_assign(&mut self, rhs: Self) {
        self.l += rhs.l;
        self.r += rhs.r;
    }
}

impl std::ops::SubAssign for Stereo {
    #[inline]
    fn sub_assign(&mut self, rhs: Self) {
        self.l -= rhs.l;
        self.r -= rhs.r;
    }
}

impl std::ops::MulAssign<f32> for Stereo {
    #[inline]
    fn mul_assign(&mut self, rhs: f32) {
        self.l *= rhs;
        self.r *= rhs;
    }
}

// Stereo is two f32s — Send + Sync auto-derived.

#[cfg(test)]
#[allow(clippy::float_cmp)]
mod tests {
    use super::*;

    #[test]
    fn test_new_and_default() {
        let s = Stereo::new(1.0, 2.0);
        assert_eq!(s.l, 1.0);
        assert_eq!(s.r, 2.0);

        let d = Stereo::default();
        assert_eq!(d.l, 0.0);
        assert_eq!(d.r, 0.0);
    }

    #[test]
    fn test_mono() {
        let s = Stereo::mono(0.5);
        assert_eq!(s.l, 0.5);
        assert_eq!(s.r, 0.5);
    }

    #[test]
    fn test_add() {
        let a = Stereo::new(1.0, 2.0);
        let b = Stereo::new(3.0, 4.0);
        let c = a + b;
        assert_eq!(c.l, 4.0);
        assert_eq!(c.r, 6.0);
    }

    #[test]
    fn test_sub() {
        let a = Stereo::new(5.0, 7.0);
        let b = Stereo::new(3.0, 4.0);
        let c = a - b;
        assert_eq!(c.l, 2.0);
        assert_eq!(c.r, 3.0);
    }

    #[test]
    fn test_scalar_mul() {
        let s = Stereo::new(2.0, 3.0) * 2.0;
        assert_eq!(s.l, 4.0);
        assert_eq!(s.r, 6.0);
    }

    #[test]
    fn test_scalar_div() {
        let s = Stereo::new(4.0, 6.0) / 2.0;
        assert_eq!(s.l, 2.0);
        assert_eq!(s.r, 3.0);
    }

    #[test]
    fn test_add_assign() {
        let mut a = Stereo::new(1.0, 2.0);
        a += Stereo::new(3.0, 4.0);
        assert_eq!(a.l, 4.0);
        assert_eq!(a.r, 6.0);
    }

    #[test]
    fn test_mul_assign() {
        let mut a = Stereo::new(2.0, 3.0);
        a *= 2.0;
        assert_eq!(a.l, 4.0);
        assert_eq!(a.r, 6.0);
    }

    #[test]
    fn test_repr_c_size() {
        assert_eq!(std::mem::size_of::<Stereo>(), 8);
        assert_eq!(std::mem::align_of::<Stereo>(), 4);
    }
}
