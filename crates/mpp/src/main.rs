//! Mega Pixel Processor CLI — thin frontend over `mpp-core`.

use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand, ValueEnum};
use mpp_core::convert::{ConvertOptions, convert};
use mpp_core::formats::tiff::BitDepth;

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
        /// Output image (.tif/.tiff, written as BigTIFF).
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
    },
}

/// CLI mirror of [`BitDepth`] (keeps clap out of mpp-core).
#[derive(Clone, Copy, ValueEnum)]
enum BitDepthArg {
    /// 32-bit float samples (lossless from XISF Float32).
    F32,
    /// 16-bit unsigned samples (clamped to [0, 1], scaled).
    U16,
    /// 8-bit unsigned samples (clamped to [0, 1], scaled).
    U8,
}

impl From<BitDepthArg> for BitDepth {
    fn from(v: BitDepthArg) -> Self {
        match v {
            BitDepthArg::F32 => BitDepth::F32,
            BitDepthArg::U16 => BitDepth::U16,
            BitDepthArg::U8 => BitDepth::U8,
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
        } => {
            let opts = ConvertOptions {
                bit_depth: bit_depth.map(Into::into),
                sidecar: !no_sidecar,
                quality,
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
    }
    Ok(())
}
