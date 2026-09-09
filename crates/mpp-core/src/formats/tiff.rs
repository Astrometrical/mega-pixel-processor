//! The output sample-format enum, shared by the writer and its callers.
//!
//! This module holds only [`BitDepth`]. The TIFF/BigTIFF writer itself lives
//! in [`crate::formats::tiff_write`], which writes the IFD directly over the
//! `tiff` crate's `DirectoryEncoder`; `BitDepth` stays here because callers
//! that never touch the writer (the CLI's `--bit-depth` flag, the IPC
//! worker's session negotiation) still need to name a sample format.

/// Output sample format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BitDepth {
    /// 32-bit IEEE float samples, passthrough from XISF Float32 (default).
    #[default]
    F32,
    /// 64-bit IEEE float samples. Exact for `pcl::DImage` input; from a
    /// unit-range f32 source it simply widens.
    F64,
    /// 32-bit unsigned samples; unit-range input is clamped and scaled.
    U32,
    /// 16-bit unsigned samples; unit-range input is clamped and scaled.
    U16,
    /// 8-bit unsigned samples; unit-range input is clamped and scaled.
    U8,
}

impl BitDepth {
    /// Bytes per sample in the output file.
    pub fn bytes(self) -> usize {
        match self {
            BitDepth::U8 => 1,
            BitDepth::U16 => 2,
            BitDepth::U32 | BitDepth::F32 => 4,
            BitDepth::F64 => 8,
        }
    }
}
