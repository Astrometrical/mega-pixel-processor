//! Mega Pixel Processor CLI — thin frontend over `mpp-core`.

use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand, ValueEnum};
use mpp_core::convert::{ConvertOptions, convert};
use mpp_core::formats::tiff::BitDepth;
use mpp_core::formats::tiff_write::{Compression, Container, Layout, TiffWriteOptions};
use mpp_core::publish::{PublishMode, PublishOptions, TileFormat, publish};

/// Mega Pixel Processor: big astro image conversion and DZI publishing.
#[derive(Parser)]
#[command(name = "mpp", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Convert an image (XISF -> BigTIFF natively; PNG/JPEG/WebP/AVIF/JXL via
    /// the vips CLI), carrying the astrometric solution to a WCS JSON sidecar.
    Convert {
        /// Input image (.xisf).
        input: PathBuf,
        /// Output image (.tif/.tiff/.png/.jpg/.webp/.avif/.jxl; extension selects the format).
        output: PathBuf,
        /// Output sample format; defaults to the output format's default.
        #[arg(long, value_enum)]
        bit_depth: Option<BitDepthArg>,
        /// Lossy-format quality, 1-100 (JPEG/WebP/AVIF/JXL only).
        #[arg(long, value_parser = clap::value_parser!(u8).range(1..=100))]
        quality: Option<u8>,
        /// Do not write the WCS sidecar even when the input is solved.
        #[arg(long)]
        no_sidecar: bool,
        /// Block compression for TIFF output.
        #[arg(long, value_enum, default_value_t = CompressionArg::None)]
        compression: CompressionArg,
        /// Deflate level 1-9 (buckets to fast/balanced/best); ignored unless
        /// --compression deflate.
        #[arg(long, value_parser = clap::value_parser!(u8).range(1..=9))]
        deflate_level: Option<u8>,
        /// Apply the horizontal differencing predictor. Integer sample
        /// formats only.
        #[arg(long)]
        predictor: bool,
        /// Write tiles instead of strips.
        #[arg(long)]
        tiled: bool,
        /// Tile edge in pixels; a power of two in 128..=1024.
        #[arg(long, default_value_t = 256)]
        tile_size: u32,
        /// TIFF container variant.
        #[arg(long, value_enum, default_value_t = ContainerArg::Auto)]
        container: ContainerArg,
    },
    /// Publish an image as a self-hosted web viewer: DZI tile pyramid (or a
    /// single image below 4096 px) + Astrometrical-branded OpenSeadragon
    /// page. Requires the vips CLI (libvips-tools).
    Publish {
        /// Input image (.xisf, or any 8/16-bit vips-readable format).
        input: PathBuf,
        /// Output directory for the static site (created if missing).
        outdir: PathBuf,
        /// Pyramid vs single-image viewer; auto picks by size (<= 4096 px -> single).
        #[arg(long, value_enum, default_value_t = ModeArg::Auto)]
        mode: ModeArg,
        /// Tile image format for DZI mode.
        #[arg(long, value_enum, default_value_t = TileFormatArg::Jpg)]
        tile_format: TileFormatArg,
        /// JPEG tile quality, 1-100 (default 90); error with --tile-format png.
        #[arg(long, value_parser = clap::value_parser!(u8).range(1..=100))]
        tile_quality: Option<u8>,
        /// Page title (default: input file stem).
        #[arg(long)]
        title: Option<String>,
        /// Do not write the WCS sidecar even when the input is solved.
        #[arg(long)]
        no_sidecar: bool,
    },
}

/// CLI mirror of [`BitDepth`] (keeps clap out of mpp-core).
#[derive(Clone, Copy, ValueEnum)]
enum BitDepthArg {
    /// 32-bit float samples (lossless from XISF Float32).
    F32,
    /// 64-bit float samples (BigTIFF/TIFF only).
    F64,
    /// 32-bit unsigned samples (BigTIFF/TIFF only; clamped to [0, 1], scaled).
    U32,
    /// 16-bit unsigned samples (clamped to [0, 1], scaled).
    U16,
    /// 8-bit unsigned samples (clamped to [0, 1], scaled).
    U8,
}

impl From<BitDepthArg> for BitDepth {
    fn from(v: BitDepthArg) -> Self {
        match v {
            BitDepthArg::F32 => BitDepth::F32,
            BitDepthArg::F64 => BitDepth::F64,
            BitDepthArg::U32 => BitDepth::U32,
            BitDepthArg::U16 => BitDepth::U16,
            BitDepthArg::U8 => BitDepth::U8,
        }
    }
}

/// CLI mirror of [`mpp_core::formats::tiff_write::Compression`].
#[derive(Clone, Copy, ValueEnum)]
enum CompressionArg {
    /// No compression (default).
    None,
    /// LZW, lossless and widely supported.
    Lzw,
    /// Deflate/zlib, lossless, usually the smallest.
    Deflate,
    /// PackBits run-length encoding.
    Packbits,
}

impl From<CompressionArg> for Compression {
    fn from(v: CompressionArg) -> Self {
        match v {
            CompressionArg::None => Compression::None,
            CompressionArg::Lzw => Compression::Lzw,
            CompressionArg::Deflate => Compression::Deflate,
            CompressionArg::Packbits => Compression::Packbits,
        }
    }
}

/// CLI mirror of [`mpp_core::formats::tiff_write::Container`].
#[derive(Clone, Copy, ValueEnum)]
enum ContainerArg {
    /// Classic TIFF when the image fits, BigTIFF otherwise (default).
    Auto,
    /// Classic 32-bit TIFF; fails for images past 4 GB.
    Classic,
    /// BigTIFF, always.
    Big,
}

impl From<ContainerArg> for Container {
    fn from(v: ContainerArg) -> Self {
        match v {
            ContainerArg::Auto => Container::Auto,
            ContainerArg::Classic => Container::Classic,
            ContainerArg::Big => Container::Big,
        }
    }
}

/// CLI mirror of [`PublishMode`] (keeps clap out of mpp-core).
#[derive(Clone, Copy, ValueEnum)]
enum ModeArg {
    /// Pick Single or Dzi automatically, from image size (<= 4096 px -> single).
    Auto,
    /// Deep Zoom Image tile pyramid, for large images.
    Dzi,
    /// Single full-resolution image file, for small images.
    Single,
}

impl From<ModeArg> for PublishMode {
    fn from(v: ModeArg) -> Self {
        match v {
            ModeArg::Auto => PublishMode::Auto,
            ModeArg::Dzi => PublishMode::Dzi,
            ModeArg::Single => PublishMode::Single,
        }
    }
}

/// CLI mirror of [`TileFormat`] (keeps clap out of mpp-core).
#[derive(Clone, Copy, ValueEnum)]
enum TileFormatArg {
    /// JPEG tiles (lossy; default).
    Jpg,
    /// PNG tiles (lossless).
    Png,
}

impl From<TileFormatArg> for TileFormat {
    fn from(v: TileFormatArg) -> Self {
        match v {
            TileFormatArg::Jpg => TileFormat::Jpg,
            TileFormatArg::Png => TileFormat::Png,
        }
    }
}

fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Convert {
            input,
            output,
            bit_depth,
            quality,
            no_sidecar,
            compression,
            deflate_level,
            predictor,
            tiled,
            tile_size,
            container,
        } => {
            let layout = if tiled {
                Layout::Tiles { size: tile_size }
            } else {
                Layout::Strips
            };
            let opts = ConvertOptions {
                bit_depth: bit_depth.map(Into::into),
                sidecar: !no_sidecar,
                quality,
                tiff: TiffWriteOptions {
                    container: container.into(),
                    layout,
                    compression: compression.into(),
                    deflate_level,
                    predictor,
                    ..Default::default()
                },
            };
            let report = convert(&input, &output, &opts)?;
            println!(
                "converted {}x{}x{} {} -> {}",
                report.width,
                report.height,
                report.channels,
                input.display(),
                output.display()
            );
            match (&report.sidecar, report.wcs_found) {
                (Some(p), _) => println!("wrote WCS sidecar {}", p.display()),
                (None, true) => println!("WCS sidecar disabled (--no-sidecar)"),
                (None, false) => println!("no astrometric solution found; no sidecar"),
            }
        }
        Command::Publish {
            input,
            outdir,
            mode,
            tile_format,
            tile_quality,
            title,
            no_sidecar,
        } => {
            let opts = PublishOptions {
                mode: mode.into(),
                tile_format: tile_format.into(),
                tile_quality,
                title,
                sidecar: !no_sidecar,
            };
            let report = publish(&input, &outdir, &opts)?;
            println!(
                "published {}x{} as {:?} -> {}",
                report.width,
                report.height,
                report.mode_used,
                report.index.display()
            );
            match (&report.sidecar, report.wcs_found) {
                (Some(p), _) => println!("wrote WCS sidecar {}", p.display()),
                (None, true) => println!("WCS sidecar disabled (--no-sidecar)"),
                (None, false) => println!("no astrometric solution found; no sidecar"),
            }
            println!(
                "serve the directory with any static host, e.g.: python3 -m http.server -d {}",
                outdir.display()
            );
        }
    }
    Ok(())
}
