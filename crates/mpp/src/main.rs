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
    /// Convert an image between formats (XISF -> BigTIFF), carrying the
    /// astrometric solution to a WCS JSON sidecar.
    Convert {
        /// Input image (.xisf).
        input: PathBuf,
        /// Output image (.tif/.tiff, written as BigTIFF).
        output: PathBuf,
        /// Output sample format; defaults to the output format's default.
        #[arg(long, value_enum)]
        bit_depth: Option<BitDepthArg>,
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
}

impl From<BitDepthArg> for BitDepth {
    fn from(v: BitDepthArg) -> Self {
        match v {
            BitDepthArg::F32 => BitDepth::F32,
            BitDepthArg::U16 => BitDepth::U16,
        }
    }
}

fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Convert {
            input,
            output,
            bit_depth,
            no_sidecar,
        } => {
            let opts = ConvertOptions {
                bit_depth: bit_depth.map(Into::into),
                sidecar: !no_sidecar,
                quality: None,
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
