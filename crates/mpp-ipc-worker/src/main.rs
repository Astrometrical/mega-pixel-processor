//! `mpp-ipc-worker`: the PixInsight-side worker process (stub).
//!
//! Task 6 wires this up to the control-channel handshake and pixel-band
//! write loop described in `docs/pi-protocol.md`. For now the binary only
//! exists so the `mpp-ipc-worker` workspace member compiles and `staging`'s
//! own test suite runs.

// Wired in Task 6.
#[allow(dead_code)]
mod staging;

fn main() {}
