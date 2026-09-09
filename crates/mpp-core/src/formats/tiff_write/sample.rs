//! The sample types the writer can emit, and their TIFF tag values.
//!
//! Sample bytes are always emitted in **native** byte order: the `tiff`
//! crate writes the file's byte-order mark from `target_endian` (`II` on a
//! little-endian host, `MM` on a big-endian one), so native-order samples
//! are by construction the file's order.

use crate::formats::tiff::BitDepth;

/// A sample type the writer can emit into a TIFF file.
pub trait Sample: Copy + 'static {
    /// Value of the `BitsPerSample` tag for this type.
    const BITS_PER_SAMPLE: u16;
    /// Value of the `SampleFormat` tag: `1` unsigned integer, `3` IEEE float.
    const TIFF_SAMPLE_FORMAT: u16;
    /// The [`BitDepth`] this type corresponds to.
    const DEPTH: BitDepth;

    /// Convert a unit-range `f32` (the XISF path's sample domain) to this
    /// type, clamping out-of-range input.
    fn from_unit_f32(v: f32) -> Self;

    /// Convert this sample to the unit-range `f32` domain the converter's
    /// XISF path uses. The inverse of [`Sample::from_unit_f32`] for the
    /// integer types (up to rounding); a widening or narrowing cast for the
    /// float ones.
    fn to_unit_f32(self) -> f32;

    /// Append this sample's native-order bytes to `out`.
    fn push_ne(self, out: &mut Vec<u8>);
}

macro_rules! uint_sample {
    ($t:ty, $bits:expr, $depth:expr) => {
        impl Sample for $t {
            const BITS_PER_SAMPLE: u16 = $bits;
            const TIFF_SAMPLE_FORMAT: u16 = 1;
            const DEPTH: BitDepth = $depth;

            fn from_unit_f32(v: f32) -> Self {
                // Scale in f64: for u32 the target range exceeds f32's
                // 24-bit mantissa, so scaling in f32 would both lose
                // resolution and risk rounding above the maximum on the
                // way to the integer cast. `clamp` before the cast makes
                // the saturation explicit rather than relying on Rust's
                // saturating float-to-int semantics.
                let scaled = f64::from(v.clamp(0.0, 1.0)) * <$t>::MAX as f64;
                scaled.round().clamp(0.0, <$t>::MAX as f64) as $t
            }

            fn to_unit_f32(self) -> f32 {
                // Divide in f64, mirroring `from_unit_f32`'s reasoning: for
                // u32 the maximum exceeds f32's 24-bit mantissa, so the
                // division has to happen at f64 precision before the
                // narrowing cast back to f32.
                (self as f64 / <$t>::MAX as f64) as f32
            }

            fn push_ne(self, out: &mut Vec<u8>) {
                out.extend_from_slice(&self.to_ne_bytes());
            }
        }
    };
}

uint_sample!(u8, 8, BitDepth::U8);
uint_sample!(u16, 16, BitDepth::U16);
uint_sample!(u32, 32, BitDepth::U32);

impl Sample for f32 {
    const BITS_PER_SAMPLE: u16 = 32;
    const TIFF_SAMPLE_FORMAT: u16 = 3;
    const DEPTH: BitDepth = BitDepth::F32;

    fn from_unit_f32(v: f32) -> Self {
        v
    }

    fn to_unit_f32(self) -> f32 {
        self
    }

    fn push_ne(self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.to_ne_bytes());
    }
}

impl Sample for f64 {
    const BITS_PER_SAMPLE: u16 = 64;
    const TIFF_SAMPLE_FORMAT: u16 = 3;
    const DEPTH: BitDepth = BitDepth::F64;

    fn from_unit_f32(v: f32) -> Self {
        f64::from(v)
    }

    fn to_unit_f32(self) -> f32 {
        self as f32
    }

    fn push_ne(self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.to_ne_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tag_values_match_the_tiff_spec() {
        // SampleFormat: 1 = unsigned integer, 3 = IEEE floating point.
        assert_eq!((u8::BITS_PER_SAMPLE, u8::TIFF_SAMPLE_FORMAT), (8, 1));
        assert_eq!((u16::BITS_PER_SAMPLE, u16::TIFF_SAMPLE_FORMAT), (16, 1));
        assert_eq!((u32::BITS_PER_SAMPLE, u32::TIFF_SAMPLE_FORMAT), (32, 1));
        assert_eq!((f32::BITS_PER_SAMPLE, f32::TIFF_SAMPLE_FORMAT), (32, 3));
        assert_eq!((f64::BITS_PER_SAMPLE, f64::TIFF_SAMPLE_FORMAT), (64, 3));
    }

    #[test]
    fn unit_range_conversion_saturates_and_rounds() {
        assert_eq!(u8::from_unit_f32(0.0), 0);
        assert_eq!(u8::from_unit_f32(1.0), 255);
        assert_eq!(u8::from_unit_f32(0.5), 128);
        assert_eq!(u8::from_unit_f32(-3.0), 0);
        assert_eq!(u8::from_unit_f32(7.0), 255);
        assert_eq!(u16::from_unit_f32(1.0), 65535);
        assert_eq!(u32::from_unit_f32(1.0), u32::MAX);
        assert_eq!(u32::from_unit_f32(0.0), 0);
        assert_eq!(f32::from_unit_f32(0.25), 0.25);
        assert_eq!(f64::from_unit_f32(0.25), 0.25);
    }

    #[test]
    fn u32_conversion_is_monotonic_near_the_top() {
        // f32 has a 24-bit mantissa, so the naive `v * u32::MAX as f32`
        // rounds several distinct inputs to the same output and can
        // overflow on the way to `as u32`. What we require is that the
        // conversion never wraps and never exceeds the maximum.
        assert_eq!(u32::from_unit_f32(1.0), u32::MAX);
        assert!(u32::from_unit_f32(0.999_999) < u32::MAX);
        assert!(u32::from_unit_f32(0.999_999) > u32::MAX / 2);
    }

    #[test]
    fn push_ne_emits_native_order_bytes() {
        let mut v = Vec::new();
        0x1234u16.push_ne(&mut v);
        assert_eq!(v, 0x1234u16.to_ne_bytes().to_vec());

        let mut v = Vec::new();
        1.5f32.push_ne(&mut v);
        assert_eq!(v, 1.5f32.to_ne_bytes().to_vec());
    }

    #[test]
    fn to_unit_f32_round_trips_with_from_unit_f32() {
        assert_eq!(u16::from_unit_f32(u16::to_unit_f32(40000)), 40000);

        for v in 0..=255u8 {
            assert_eq!(u8::from_unit_f32(u8::to_unit_f32(v)), v);
        }
    }
}
