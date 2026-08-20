//! Mega Pixel Processor CLI — thin frontend over `mpp-core`.

use clap::Parser;

/// Mega Pixel Processor: big astro image conversion and DZI publishing.
#[derive(Parser)]
#[command(name = "mpp", version, about)]
struct Cli {}

fn main() {
    Cli::parse();
    println!(
        "mpp {} — scaffold; no subcommands yet (roadmap: docs/DESIGN.md)",
        env!("CARGO_PKG_VERSION")
    );
}
