//! Host/worker IPC for the PixInsight integration.
//!
//! The PixInsight module (C++, plan B) is the HOST: it creates one shared
//! memory segment, spawns `mpp-ipc-worker`, and streams planar f32 pixel
//! bands through shm slots while control frames flow over the worker's
//! stdio. The wire contract is `docs/pi-protocol.md` — the Rust constants
//! here and the C++ mirror must both match it exactly.

#![warn(missing_docs)]

pub mod shm;
