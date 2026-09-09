//! Mmap-backed planar f32 staging for out-of-order pixel-band arrival.
//!
//! The host (PixInsight module) delivers pixel bands over shared memory in
//! whatever order its own row-generation happens to produce them — not
//! necessarily row 0 first, not necessarily one channel at a time. [`Staging`]
//! gives each band a home: a memory-mapped scratch file sized for the whole
//! image (`width * height * channels * 4` bytes, planar layout — one
//! contiguous plane per channel, matching [`mpp_core::formats::xisf::XisfImage`]),
//! plus a per-channel, per-row coverage bitmap so callers can tell when
//! enough of the image has arrived to resume a paused write, and so a band
//! landing on already-written rows is rejected rather than silently
//! corrupting the file.
//!
//! Once complete (or complete enough for the writer's next strip),
//! [`Staging`] itself implements [`PlanarRows`], so it drops straight into
//! `mpp-core`'s streaming writers in place of an [`XisfImage`](mpp_core::formats::xisf::XisfImage).

use std::fs::{File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};

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

/// Mmap-backed planar f32 staging for out-of-order band arrival.
///
/// Backed by a scratch file (see [`create`](Staging::create)) rather than an
/// in-memory buffer: staged images run to multiple gigabytes, and mapping a
/// file lets the OS page staged data out under memory pressure instead of
/// forcing it all resident. The file is deleted on [`remove`](Staging::remove)
/// (or best-effort on `Drop` if the caller never calls it).
pub struct Staging {
    path: PathBuf,
    mmap: MmapMut,
    width: u64,
    height: u64,
    channels: u64,
    /// Per-channel, per-row coverage: `coverage[c][y]` is `true` once row
    /// `y` of channel `c` has been written.
    coverage: Vec<Vec<bool>>,
    /// Set by [`remove`](Staging::remove) so `Drop` doesn't attempt (and
    /// silently ignore failing) a second deletion.
    removed: bool,
}

impl Staging {
    /// Create a new staging file at `path`, sized for a `width x height x
    /// channels` planar f32 image, and map it read/write. The file is
    /// created (or truncated, if it already exists) and grown to the full
    /// image size up front — writes never resize it.
    pub fn create(path: &Path, width: u64, height: u64, channels: u64) -> io::Result<Self> {
        let total_bytes = width * height * channels * 4;
        let file: File = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)?;
        file.set_len(total_bytes)?;
        // SAFETY: `file` is a plain scratch file this process just created
        // and owns exclusively; no other process can be mutating it
        // concurrently, so a mapped-file's usual "external truncation is UB"
        // caveat doesn't apply here in practice.
        let mmap = unsafe { MmapMut::map_mut(&file)? };
        Ok(Self {
            path: path.to_path_buf(),
            mmap,
            width,
            height,
            channels,
            coverage: vec![vec![false; height as usize]; channels as usize],
            removed: false,
        })
    }

    /// Byte offset of the start of channel `c`'s plane.
    fn channel_offset(&self, c: u64) -> usize {
        (c * self.width * self.height * 4) as usize
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
        samples: &[f32],
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

        let row_bytes = self.width as usize * 4;
        let offset = self.channel_offset(channel) + start_row as usize * row_bytes;
        let byte_len = samples.len() * 4;
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

    /// Delete the staging file. Consumes `self`; the same deletion also
    /// happens best-effort on `Drop` if this is never called.
    pub fn remove(mut self) -> io::Result<()> {
        self.removed = true;
        std::fs::remove_file(&self.path)
    }
}

impl Drop for Staging {
    fn drop(&mut self) {
        if !self.removed {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

impl PlanarRows<f32> for Staging {
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
    /// touched yet) reads back as all-zero — the mmap'd staging file
    /// starts zero-filled and this accessor does not consult `coverage` at
    /// all — so a caller must only read through this trait impl once
    /// [`Staging::is_complete`] is `true`. The worker's event loop (task 6)
    /// enforces that shape directly: it only reaches the `write_bigtiff`
    /// call after its `Finish` handler has confirmed
    /// [`Staging::first_uncovered`] returns `None`, so a partially-covered
    /// image is never streamed through the encoder.
    fn row(&self, c: u64, y: u64) -> &[f32] {
        assert!(c < self.channels, "channel {c} out of range");
        assert!(y < self.height, "row {y} out of range");
        let row_bytes = self.width as usize * 4;
        let offset = self.channel_offset(c) + y as usize * row_bytes;
        bytemuck::cast_slice(&self.mmap[offset..offset + row_bytes])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_path(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join("mpp-ipc-worker-staging-tests");
        std::fs::create_dir_all(&dir).unwrap();
        dir.join(format!("{tag}-{}-{}.stage", std::process::id(), tag.len()))
    }

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
        let path = tmp_path("out-of-order");
        let mut s = Staging::create(&path, 4, 3, 2).unwrap();

        // Deliberately scrambled arrival order across channels and rows.
        s.write_band(1, 2, &band(4, 1, 2, 1)).unwrap(); // ch1 row 2
        s.write_band(0, 0, &band(4, 0, 0, 2)).unwrap(); // ch0 rows 0..2
        s.write_band(1, 0, &band(4, 1, 0, 2)).unwrap(); // ch1 rows 0..2
        s.write_band(0, 2, &band(4, 0, 2, 1)).unwrap(); // ch0 row 2

        assert!(s.is_complete());
        assert_eq!(s.row(0, 0), &[0.0; 4]);
        assert_eq!(s.row(0, 2), &[2.0; 4]);
        assert_eq!(s.row(1, 2), &[102.0; 4]);

        s.remove().unwrap();
    }

    #[test]
    fn overlap_is_rejected() {
        let path = tmp_path("overlap");
        let mut s = Staging::create(&path, 4, 3, 1).unwrap();
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
        s.remove().unwrap();
    }

    #[test]
    fn out_of_range_channel_and_rows_are_rejected() {
        let path = tmp_path("out-of-range");
        let mut s = Staging::create(&path, 4, 3, 2).unwrap();

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

        s.remove().unwrap();
    }

    #[test]
    fn samples_not_a_multiple_of_width_are_rejected() {
        let path = tmp_path("not-aligned");
        let mut s = Staging::create(&path, 4, 3, 1).unwrap();
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
        s.remove().unwrap();
    }

    #[test]
    fn first_uncovered_walks_channel_major_then_row() {
        let path = tmp_path("first-uncovered");
        let mut s = Staging::create(&path, 4, 3, 2).unwrap();
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

        s.remove().unwrap();
    }

    #[test]
    fn rows_received_counts_across_channels() {
        let path = tmp_path("rows-received");
        let mut s = Staging::create(&path, 4, 3, 2).unwrap();
        assert_eq!(s.rows_received(), 0);
        s.write_band(0, 0, &band(4, 0, 0, 2)).unwrap();
        assert_eq!(s.rows_received(), 2);
        s.write_band(1, 0, &band(4, 1, 0, 3)).unwrap();
        assert_eq!(s.rows_received(), 5);
        assert!(!s.is_complete()); // ch0 row 2 still missing
        s.remove().unwrap();
    }

    #[test]
    fn planar_rows_returns_exactly_what_was_written() {
        let path = tmp_path("planar-rows");
        let mut s = Staging::create(&path, 4, 3, 2).unwrap();
        s.write_band(0, 1, &[10.0, 20.0, 30.0, 40.0]).unwrap();
        s.write_band(1, 2, &[1.0, 2.0, 3.0, 4.0]).unwrap();

        assert_eq!(PlanarRows::<f32>::width(&s), 4);
        assert_eq!(PlanarRows::<f32>::height(&s), 3);
        assert_eq!(PlanarRows::<f32>::channels(&s), 2);
        assert_eq!(s.row(0, 1), &[10.0, 20.0, 30.0, 40.0]);
        assert_eq!(s.row(1, 2), &[1.0, 2.0, 3.0, 4.0]);

        s.remove().unwrap();
    }

    #[test]
    fn remove_deletes_the_staging_file() {
        let path = tmp_path("remove");
        let s = Staging::create(&path, 4, 3, 1).unwrap();
        assert!(path.exists());
        s.remove().unwrap();
        assert!(!path.exists());
    }

    #[test]
    fn drop_without_remove_best_effort_deletes_the_file() {
        let path = tmp_path("drop");
        let s = Staging::create(&path, 4, 3, 1).unwrap();
        assert!(path.exists());
        drop(s);
        assert!(!path.exists());
    }
}
