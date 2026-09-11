//! Planar staging for out-of-order pixel-band arrival.
//!
//! The host (PixInsight module) delivers pixel bands over shared memory in
//! whatever order its own row-generation happens to produce them — not
//! necessarily row 0 first, not necessarily one channel at a time. [`Staging`]
//! gives each band a home: an anonymous memory mapping sized for the whole
//! image (`width * height * channels * size_of::<T>()` bytes, planar layout —
//! one contiguous plane per channel, matching
//! [`mpp_core::formats::xisf::XisfImage`]), plus a per-channel, per-row
//! coverage bitmap so callers can tell when enough of the image has arrived
//! to resume a paused write, and so a band landing on already-written rows
//! is rejected rather than silently corrupting the file.
//!
//! `T` is the session's negotiated band sample type
//! (`InitMessage::band_format`, protocol v3): staging the host's own sample
//! type rather than flattening everything to unit-range `f32` is what lets
//! `u32` and `f64` pixels reach the encoder intact.
//!
//! Once complete (or complete enough for the writer's next strip),
//! [`Staging`] itself implements [`PlanarRows`], so it drops straight into
//! `mpp-core`'s streaming writers in place of an [`XisfImage`](mpp_core::formats::xisf::XisfImage).

use std::io;

use bytemuck::Pod;
use memmap2::MmapMut;
use mpp_core::formats::PlanarRows;

/// Errors from [`Staging::write_band`].
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum StagingError {
    /// The channel index, or the band's row range, falls outside the image
    /// this [`Staging`] was created for.
    #[error(
        "band for channel {channel} rows {start_row}..{end_row} is out of range \
         for a {width}x{height}x{channels} image"
    )]
    OutOfRange {
        /// The channel index the band was written for.
        channel: u64,
        /// The band's first row (inclusive).
        start_row: u64,
        /// The band's last row (exclusive).
        end_row: u64,
        /// Image width, for the error message.
        width: u64,
        /// Image height, for the error message.
        height: u64,
        /// Image channel count, for the error message.
        channels: u64,
    },
    /// `samples.len()` is not an exact multiple of the image width, so no
    /// whole number of rows can be derived from it.
    #[error(
        "band for channel {channel} at row {start_row} has {len} samples, \
         not a multiple of width {width}"
    )]
    NotRowAligned {
        /// The channel index the band was written for.
        channel: u64,
        /// The band's first row.
        start_row: u64,
        /// The sample count that failed to divide evenly by `width`.
        len: usize,
        /// Image width, for the error message.
        width: u64,
    },
    /// One or more rows the band would cover were already written by an
    /// earlier `write_band` call.
    #[error("band for channel {channel} rows {start_row}..{end_row} overlaps already-written rows")]
    Overlap {
        /// The channel index the band was written for.
        channel: u64,
        /// The band's first row (inclusive).
        start_row: u64,
        /// The band's last row (exclusive).
        end_row: u64,
    },
}

/// Planar staging for out-of-order band arrival.
///
/// The type parameter is the sample type the session negotiated
/// (`InitMessage::band_format`). The staging area is
/// `width * height * channels * size_of::<T>()` bytes, mapped read/write, so
/// out-of-order bands land at their final position and the encoder drains
/// the result through [`PlanarRows<T>`] without a second copy or a
/// conversion.
///
/// Backed by an anonymous mapping (see [`create`](Staging::create)). It used
/// to be a scratch file next to the output, so the OS could page staged
/// data out under memory pressure — but a file-backed mapping's dirty pages
/// are written back to disk proactively, which cost every save a second,
/// image-sized write before encoding even started. Anonymous pages go to
/// swap only under actual pressure, so the common case (the image fits) no
/// longer touches the disk at all, and an image larger than RAM still
/// completes if swap has room. Pages are committed lazily as bands land.
pub struct Staging<T> {
    mmap: MmapMut,
    width: u64,
    height: u64,
    channels: u64,
    /// Per-channel, per-row coverage: `coverage[c][y]` is `true` once row
    /// `y` of channel `c` has been written.
    coverage: Vec<Vec<bool>>,
    /// `Staging` owns no `T` value — the samples live in the mapping, read
    /// back through [`PlanarRows`] — so the parameter is carried here.
    _sample: std::marker::PhantomData<T>,
}

impl<T: Pod> Staging<T> {
    /// Bytes one sample of `T` occupies in the staging file.
    const SAMPLE_BYTES: usize = std::mem::size_of::<T>();

    /// Map a zero-filled anonymous region sized for a `width x height x
    /// channels` planar image of `T` samples. Fails with the OS's error if
    /// the address space (or, on Windows, the commit charge) cannot be
    /// reserved; the memory itself is only committed page by page as bands
    /// are written.
    pub fn create(width: u64, height: u64, channels: u64) -> io::Result<Self> {
        let total_bytes = width * height * channels * Self::SAMPLE_BYTES as u64;
        let total_bytes = usize::try_from(total_bytes).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("staging area of {total_bytes} bytes exceeds the address space"),
            )
        })?;
        let mmap = MmapMut::map_anon(total_bytes)?;
        Ok(Self {
            mmap,
            width,
            height,
            channels,
            coverage: vec![vec![false; height as usize]; channels as usize],
            _sample: std::marker::PhantomData,
        })
    }

    /// Size of the staging area in bytes:
    /// `width * height * channels * size_of::<T>()`.
    #[cfg(test)]
    pub fn byte_len(&self) -> usize {
        self.mmap.len()
    }

    /// Byte offset of the start of channel `c`'s plane.
    fn channel_offset(&self, c: u64) -> usize {
        (c * self.width * self.height * Self::SAMPLE_BYTES as u64) as usize
    }

    /// Copy one band (a run of consecutive rows of one channel) into the
    /// staging file. `rows_in_band = samples.len() / width`; `samples.len()`
    /// must be an exact multiple of `width` ([`StagingError::NotRowAligned`]
    /// otherwise), the channel and the resulting row range must lie within
    /// the image ([`StagingError::OutOfRange`]), and none of those rows may
    /// already be covered ([`StagingError::Overlap`]).
    pub fn write_band(
        &mut self,
        channel: u64,
        start_row: u64,
        samples: &[T],
    ) -> Result<(), StagingError> {
        if self.width == 0 || !(samples.len() as u64).is_multiple_of(self.width) {
            return Err(StagingError::NotRowAligned {
                channel,
                start_row,
                len: samples.len(),
                width: self.width,
            });
        }
        let rows = samples.len() as u64 / self.width;
        let out_of_range = || StagingError::OutOfRange {
            channel,
            start_row,
            end_row: start_row.saturating_add(rows),
            width: self.width,
            height: self.height,
            channels: self.channels,
        };
        if channel >= self.channels {
            return Err(out_of_range());
        }
        let end_row = start_row.checked_add(rows).ok_or_else(out_of_range)?;
        if end_row > self.height {
            return Err(out_of_range());
        }

        let cov = &self.coverage[channel as usize];
        if (start_row..end_row).any(|y| cov[y as usize]) {
            return Err(StagingError::Overlap {
                channel,
                start_row,
                end_row,
            });
        }

        let row_bytes = self.width as usize * Self::SAMPLE_BYTES;
        let offset = self.channel_offset(channel) + start_row as usize * row_bytes;
        let byte_len = samples.len() * Self::SAMPLE_BYTES;
        self.mmap[offset..offset + byte_len].copy_from_slice(bytemuck::cast_slice(samples));

        let cov = &mut self.coverage[channel as usize];
        for y in start_row..end_row {
            cov[y as usize] = true;
        }
        Ok(())
    }

    /// Total rows written so far, summed across all channels (so a fully
    /// covered image reports `height * channels`).
    pub fn rows_received(&self) -> u64 {
        self.coverage
            .iter()
            .map(|rows| rows.iter().filter(|&&covered| covered).count() as u64)
            .sum()
    }

    /// The first `(channel, row)` not yet covered, walking channel-major
    /// then row order (channel 0's rows top to bottom, then channel 1's,
    /// …). `None` once every channel's every row has been written.
    pub fn first_uncovered(&self) -> Option<(u64, u64)> {
        for (c, rows) in self.coverage.iter().enumerate() {
            if let Some(y) = rows.iter().position(|&covered| !covered) {
                return Some((c as u64, y as u64));
            }
        }
        None
    }

    /// True once every row of every channel has been written.
    pub fn is_complete(&self) -> bool {
        self.first_uncovered().is_none()
    }
}

impl<T: Pod> PlanarRows<T> for Staging<T> {
    fn width(&self) -> u64 {
        self.width
    }

    fn height(&self) -> u64 {
        self.height
    }

    fn channels(&self) -> u64 {
        self.channels
    }

    /// Returns row `y` of channel `c`. **Not coverage-aware**: an
    /// uncovered row (one no [`write_band`](Staging::write_band) call has
    /// touched yet) reads back as all-zero — the anonymous mapping
    /// starts zero-filled and this accessor does not consult `coverage` at
    /// all — so a caller must only read through this trait impl once
    /// [`Staging::is_complete`] is `true`. The worker's event loop (task 6)
    /// enforces that shape directly: it only reaches the `write_tiff`
    /// call after its `Finish` handler has confirmed
    /// [`Staging::first_uncovered`] returns `None`, so a partially-covered
    /// image is never streamed through the encoder.
    fn row(&self, c: u64, y: u64) -> &[T] {
        assert!(c < self.channels, "channel {c} out of range");
        assert!(y < self.height, "row {y} out of range");
        let row_bytes = self.width as usize * Self::SAMPLE_BYTES;
        let offset = self.channel_offset(c) + y as usize * row_bytes;
        bytemuck::cast_slice(&self.mmap[offset..offset + row_bytes])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Rows `[start, start+n)` of channel `c`, as `f32` values `c*100 + row`
    /// (distinct per channel and row, easy to eyeball in assertions),
    /// repeated across `width` columns.
    fn band(width: u64, c: u64, start: u64, n: u64) -> Vec<f32> {
        let mut out = Vec::with_capacity((n * width) as usize);
        for row in start..start + n {
            let v = c as f32 * 100.0 + row as f32;
            out.extend(std::iter::repeat_n(v, width as usize));
        }
        out
    }

    #[test]
    fn out_of_order_bands_land_in_the_right_place() {
        let mut s = Staging::<f32>::create(4, 3, 2).unwrap();

        // Deliberately scrambled arrival order across channels and rows.
        s.write_band(1, 2, &band(4, 1, 2, 1)).unwrap(); // ch1 row 2
        s.write_band(0, 0, &band(4, 0, 0, 2)).unwrap(); // ch0 rows 0..2
        s.write_band(1, 0, &band(4, 1, 0, 2)).unwrap(); // ch1 rows 0..2
        s.write_band(0, 2, &band(4, 0, 2, 1)).unwrap(); // ch0 row 2

        assert!(s.is_complete());
        assert_eq!(s.row(0, 0), &[0.0; 4]);
        assert_eq!(s.row(0, 2), &[2.0; 4]);
        assert_eq!(s.row(1, 2), &[102.0; 4]);
    }

    #[test]
    fn overlap_is_rejected() {
        let mut s = Staging::<f32>::create(4, 3, 1).unwrap();
        s.write_band(0, 0, &band(4, 0, 0, 2)).unwrap(); // rows 0..2
        let err = s.write_band(0, 1, &band(4, 0, 1, 2)).unwrap_err(); // rows 1..3, overlaps row 1
        assert_eq!(
            err,
            StagingError::Overlap {
                channel: 0,
                start_row: 1,
                end_row: 3,
            }
        );
    }

    #[test]
    fn out_of_range_channel_and_rows_are_rejected() {
        let mut s = Staging::<f32>::create(4, 3, 2).unwrap();

        // Channel past the end.
        let err = s.write_band(2, 0, &band(4, 2, 0, 1)).unwrap_err();
        assert!(matches!(err, StagingError::OutOfRange { channel: 2, .. }));

        // Row range past the end (height is 3).
        let err = s.write_band(0, 2, &band(4, 0, 2, 2)).unwrap_err();
        assert!(matches!(
            err,
            StagingError::OutOfRange {
                start_row: 2,
                end_row: 4,
                ..
            }
        ));
    }

    #[test]
    fn samples_not_a_multiple_of_width_are_rejected() {
        let mut s = Staging::<f32>::create(4, 3, 1).unwrap();
        let err = s.write_band(0, 0, &[0.0; 5]).unwrap_err();
        assert_eq!(
            err,
            StagingError::NotRowAligned {
                channel: 0,
                start_row: 0,
                len: 5,
                width: 4,
            }
        );
    }

    #[test]
    fn first_uncovered_walks_channel_major_then_row() {
        let mut s = Staging::<f32>::create(4, 3, 2).unwrap();
        assert_eq!(s.first_uncovered(), Some((0, 0)));

        s.write_band(0, 0, &band(4, 0, 0, 1)).unwrap(); // ch0 row 0
        assert_eq!(s.first_uncovered(), Some((0, 1)));

        s.write_band(0, 1, &band(4, 0, 1, 2)).unwrap(); // ch0 rows 1..3: channel 0 complete
        assert_eq!(s.first_uncovered(), Some((1, 0)));

        s.write_band(1, 1, &band(4, 1, 1, 1)).unwrap(); // ch1 row 1, out of order
        assert_eq!(s.first_uncovered(), Some((1, 0)));

        s.write_band(1, 0, &band(4, 1, 0, 1)).unwrap();
        assert_eq!(s.first_uncovered(), Some((1, 2)));

        s.write_band(1, 2, &band(4, 1, 2, 1)).unwrap();
        assert_eq!(s.first_uncovered(), None);
        assert!(s.is_complete());
    }

    #[test]
    fn rows_received_counts_across_channels() {
        let mut s = Staging::<f32>::create(4, 3, 2).unwrap();
        assert_eq!(s.rows_received(), 0);
        s.write_band(0, 0, &band(4, 0, 0, 2)).unwrap();
        assert_eq!(s.rows_received(), 2);
        s.write_band(1, 0, &band(4, 1, 0, 3)).unwrap();
        assert_eq!(s.rows_received(), 5);
        assert!(!s.is_complete()); // ch0 row 2 still missing
    }

    #[test]
    fn planar_rows_returns_exactly_what_was_written() {
        let mut s = Staging::<f32>::create(4, 3, 2).unwrap();
        s.write_band(0, 1, &[10.0, 20.0, 30.0, 40.0]).unwrap();
        s.write_band(1, 2, &[1.0, 2.0, 3.0, 4.0]).unwrap();

        assert_eq!(PlanarRows::<f32>::width(&s), 4);
        assert_eq!(PlanarRows::<f32>::height(&s), 3);
        assert_eq!(PlanarRows::<f32>::channels(&s), 2);
        assert_eq!(s.row(0, 1), &[10.0, 20.0, 30.0, 40.0]);
        assert_eq!(s.row(1, 2), &[1.0, 2.0, 3.0, 4.0]);
    }

    #[test]
    fn staging_is_anonymous_memory_and_writes_nothing_to_disk() {
        // No path in, no file out: the scratch space is an anonymous
        // mapping, so a save never doubles its disk traffic with a staging
        // file's writeback (and a killed worker leaves nothing behind).
        let before = std::fs::read_dir(std::env::temp_dir()).unwrap().count();
        let mut s = Staging::<f32>::create(4, 3, 1).unwrap();
        s.write_band(0, 0, &band(4, 0, 0, 3)).unwrap();
        assert!(s.is_complete());
        drop(s);
        let after = std::fs::read_dir(std::env::temp_dir()).unwrap().count();
        assert_eq!(before, after, "staging must not create files");
    }

    /// Protocol v3's reason for existing, at the staging layer: a `u32`
    /// staging file must hand back the exact bit patterns it was given,
    /// including values `f32` cannot represent (16_777_217 is the first
    /// integer above `f32`'s 24-bit mantissa) and values above `i32::MAX`.
    #[test]
    fn u32_samples_survive_staging_bit_exact() {
        let values: [u32; 4] = [0, 16_777_217, 4_000_000_001, u32::MAX];
        let mut s = Staging::<u32>::create(4, 2, 1).unwrap();
        s.write_band(0, 0, &values).unwrap();
        s.write_band(0, 1, &[7, 8, 9, 10]).unwrap();
        assert!(s.is_complete());
        assert_eq!(s.row(0, 0), &values);
        assert_eq!(s.row(0, 1), &[7u32, 8, 9, 10]);
        // Sized for u32, not f32: 4 * 2 * 1 * 4 bytes.
        assert_eq!(s.byte_len(), 32);
    }

    /// The staging size follows `size_of::<T>()`, not a hard-coded 4 — the
    /// change that makes an `f64` or `u8` session possible at all.
    #[test]
    fn staging_size_follows_the_sample_type() {
        fn size<T: Pod>() -> usize {
            Staging::<T>::create(3, 2, 1).unwrap().byte_len()
        }
        // 3 * 2 * 1 samples, times the sample width.
        assert_eq!(size::<u8>(), 6);
        assert_eq!(size::<u16>(), 12);
        assert_eq!(size::<f32>(), 24);
        assert_eq!(size::<f64>(), 48);
    }
}
