//! Timing harness for the TIFF encoder and decoder on a synthesised image.
//!
//! Not a test: run it by hand (release profile, or the numbers mean nothing)
//! before and after a performance change.
//!
//! ```sh
//! cargo run --release -p mpp-core --example tiff_bench -- 8000 8000 3 u16 none strips
//! cargo run --release -p mpp-core --example tiff_bench -- 8000 8000 3 f32 deflate tiles
//! ```
//!
//! Arguments: width height channels depth(u8|u16|u32|f32|f64)
//! compression(none|lzw|deflate|packbits) layout(strips|tiles). Writes to
//! the system temp dir and deletes the file afterwards.

use std::path::PathBuf;
use std::time::Instant;

use mpp_core::formats::PlanarRows;
use mpp_core::formats::tiff_read::TiffReader;
use mpp_core::formats::tiff_write::{
    BitDepth, Compression, Layout, Sample, TiffWriteOptions, write_tiff_native,
};

/// A planar image whose sample values vary smoothly, so compressors have
/// something realistic to chew on (pure noise defeats them, constants are
/// unrealistically easy).
struct Synth<T> {
    w: u64,
    h: u64,
    planes: Vec<Vec<T>>,
}

impl<T: Sample> Synth<T> {
    fn new(w: u64, h: u64, c: u64) -> Self {
        let planes = (0..c)
            .map(|ch| {
                (0..w * h)
                    .map(|i| {
                        let x = (i % w) as f32 / w as f32;
                        let y = (i / w) as f32 / h as f32;
                        let v = 0.5 + 0.4 * ((x * 37.0 + ch as f32).sin() * (y * 23.0).cos());
                        // A little per-pixel texture so runs are short.
                        let n = ((i.wrapping_mul(2654435761) >> 13) & 0xff) as f32 / 4096.0;
                        T::from_unit_f32((v + n).clamp(0.0, 1.0))
                    })
                    .collect()
            })
            .collect();
        Self { w, h, planes }
    }
}

impl<T: Sample> PlanarRows<T> for Synth<T> {
    fn width(&self) -> u64 {
        self.w
    }
    fn height(&self) -> u64 {
        self.h
    }
    fn channels(&self) -> u64 {
        self.planes.len() as u64
    }
    fn row(&self, c: u64, y: u64) -> &[T] {
        let s = (y * self.w) as usize;
        &self.planes[c as usize][s..s + self.w as usize]
    }
}

fn run<T: Sample>(w: u64, h: u64, c: u64, opts: &TiffWriteOptions, path: &PathBuf) {
    let t = Instant::now();
    let img = Synth::<T>::new(w, h, c);
    let bytes = w * h * c * std::mem::size_of::<T>() as u64;
    eprintln!(
        "synth   {:>8.2} s   ({:.1} MiB)",
        t.elapsed().as_secs_f64(),
        bytes as f64 / (1024.0 * 1024.0)
    );

    let t = Instant::now();
    write_tiff_native(&img, path, opts).expect("write");
    let dt = t.elapsed().as_secs_f64();
    let size = std::fs::metadata(path).unwrap().len();
    eprintln!(
        "encode  {:>8.2} s   {:.0} MiB/s in, {:.1} MiB out",
        dt,
        bytes as f64 / (1024.0 * 1024.0) / dt,
        size as f64 / (1024.0 * 1024.0)
    );

    let t = Instant::now();
    let mut reader = TiffReader::open(path).expect("open");
    let mut rows_seen = 0u64;
    reader
        .read_bands(&mut |_c, _y, rows, _bytes| {
            rows_seen += u64::from(rows);
            Ok(())
        })
        .expect("read");
    let dt = t.elapsed().as_secs_f64();
    assert_eq!(rows_seen, h * c);
    eprintln!(
        "decode  {:>8.2} s   {:.0} MiB/s out",
        dt,
        bytes as f64 / (1024.0 * 1024.0) / dt
    );
}

fn main() {
    let a: Vec<String> = std::env::args().skip(1).collect();
    if a.len() != 6 {
        eprintln!("usage: tiff_bench W H C depth compression layout");
        std::process::exit(2);
    }
    let (w, h, c): (u64, u64, u64) = (
        a[0].parse().unwrap(),
        a[1].parse().unwrap(),
        a[2].parse().unwrap(),
    );
    let depth = match a[3].as_str() {
        "u8" => BitDepth::U8,
        "u16" => BitDepth::U16,
        "u32" => BitDepth::U32,
        "f32" => BitDepth::F32,
        "f64" => BitDepth::F64,
        other => panic!("unknown depth {other}"),
    };
    let compression = match a[4].as_str() {
        "none" => Compression::None,
        "lzw" => Compression::Lzw,
        "deflate" => Compression::Deflate,
        "packbits" => Compression::Packbits,
        other => panic!("unknown compression {other}"),
    };
    let layout = match a[5].as_str() {
        "strips" => Layout::Strips,
        "tiles" => Layout::Tiles { size: 256 },
        other => panic!("unknown layout {other}"),
    };
    let opts = TiffWriteOptions {
        bit_depth: depth,
        compression,
        layout,
        ..Default::default()
    };
    let path = std::env::temp_dir().join(format!("mpp-tiff-bench-{}.tf8", std::process::id()));
    eprintln!("{w}x{h}x{c} {depth:?} {compression:?} {layout:?}");
    match depth {
        BitDepth::U8 => run::<u8>(w, h, c, &opts, &path),
        BitDepth::U16 => run::<u16>(w, h, c, &opts, &path),
        BitDepth::U32 => run::<u32>(w, h, c, &opts, &path),
        BitDepth::F32 => run::<f32>(w, h, c, &opts, &path),
        BitDepth::F64 => run::<f64>(w, h, c, &opts, &path),
    }
    let _ = std::fs::remove_file(&path);
}
