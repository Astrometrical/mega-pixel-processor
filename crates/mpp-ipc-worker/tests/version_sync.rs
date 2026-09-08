//! Tripwire keeping the three release-version sources in exact sync. The
//! module ⇄ worker version handshake compares these strings for equality
//! (docs/pi-protocol.md §8), so a bump that misses one of them would make
//! every packaged run refuse to start:
//!
//! * the workspace `Cargo.toml` version (this binary's `CARGO_PKG_VERSION`),
//! * the PixInsight module's `MPP_MODULE_VERSION_STRING`
//!   (`integration/pixinsight/module/MppVersion.h`),
//! * the C++ host library's `kExpectedWorkerVersion`
//!   (`integration/pixinsight/host/mpp_protocol.h`), which the host stamps
//!   into every `Init` frame.
//!
//! Modeled on MergeMosaic's `crates/mmm-ipc-worker/tests/version_sync.rs`
//! (our own code). Two equality assertions against `CARGO_PKG_VERSION` pin
//! all three legs transitively.
//!
//! A fourth assertion pins the **wire protocol version** across the language
//! boundary: the Rust `IPC_PROTOCOL_VERSION` (`crates/mpp-ipc/src/protocol.rs`)
//! and the C++ host's `kProtocolVersion` (`mpp_protocol.h`) must agree and both
//! equal `2` (the read-direction protocol; see `docs/pi-protocol.md` §8). The
//! worker aborts on a `protocol_version` mismatch in `Init`, so a drift here
//! would break every packaged run exactly like a version-string drift.

use std::path::PathBuf;

fn repo_file(rel: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(rel);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()))
}

/// The value of the first `#define NAME "..."` / `constexpr ... NAME = "...";`
/// style quoted string on the line defining `name` in `text`.
fn quoted_value_on_line(text: &str, name: &str, from: &str) -> String {
    let line = text
        .lines()
        .find(|l| l.contains(name) && l.contains('"'))
        .unwrap_or_else(|| panic!("no quoted {name} definition found in {from}"));
    let mut parts = line.split('"');
    parts.next();
    parts
        .next()
        .unwrap_or_else(|| panic!("malformed {name} line in {from}: {line}"))
        .to_string()
}

/// The unsigned integer literal assigned on the first line that both mentions
/// `name` and has a digit immediately after its `=` (i.e. its definition line,
/// `... name ... = <n> ...`). Skips doc-comment and use-site lines that mention
/// the constant without assigning it a numeric literal (e.g. Rust's
/// `protocol_version: IPC_PROTOCOL_VERSION,` or C++'s
/// `protocol_version = kProtocolVersion;`).
fn int_value_on_line(text: &str, name: &str, from: &str) -> u32 {
    let line = text
        .lines()
        .find(|l| {
            l.contains(name)
                && l.split_once('=').is_some_and(|(_, rhs)| {
                    rhs.trim_start().starts_with(|c: char| c.is_ascii_digit())
                })
        })
        .unwrap_or_else(|| panic!("no numeric {name} definition found in {from}"));
    let rhs = line.split_once('=').unwrap().1;
    let digits: String = rhs
        .trim_start()
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    digits
        .parse()
        .unwrap_or_else(|e| panic!("malformed {name} value in {from}: {line} ({e})"))
}

#[test]
fn protocol_versions_agree_at_v2() {
    let rust = repo_file("crates/mpp-ipc/src/protocol.rs");
    let host = repo_file("integration/pixinsight/host/mpp_protocol.h");
    let rust_version = int_value_on_line(&rust, "IPC_PROTOCOL_VERSION", "protocol.rs");
    let host_version = int_value_on_line(&host, "kProtocolVersion", "mpp_protocol.h");
    assert_eq!(
        rust_version, 2,
        "crates/mpp-ipc/src/protocol.rs IPC_PROTOCOL_VERSION must be 2 (the \
         read-direction wire protocol)"
    );
    assert_eq!(
        host_version, 2,
        "integration/pixinsight/host/mpp_protocol.h kProtocolVersion must be 2 \
         (the read-direction wire protocol)"
    );
    assert_eq!(
        rust_version, host_version,
        "the Rust IPC_PROTOCOL_VERSION and the C++ host kProtocolVersion must \
         agree; the worker aborts on a protocol_version mismatch in Init"
    );
}

#[test]
fn module_version_matches_worker_version() {
    let header = repo_file("integration/pixinsight/module/MppVersion.h");
    let module = quoted_value_on_line(&header, "MPP_MODULE_VERSION_STRING", "MppVersion.h");
    assert_eq!(
        module,
        env!("CARGO_PKG_VERSION"),
        "MppVersion.h MPP_MODULE_VERSION_STRING and the workspace Cargo.toml \
         version must be bumped together (the version handshake compares them)"
    );
}

#[test]
fn host_expected_worker_version_matches_worker_version() {
    let header = repo_file("integration/pixinsight/host/mpp_protocol.h");
    let expected = quoted_value_on_line(&header, "kExpectedWorkerVersion", "mpp_protocol.h");
    assert_eq!(
        expected,
        env!("CARGO_PKG_VERSION"),
        "mpp_protocol.h kExpectedWorkerVersion and the workspace Cargo.toml \
         version must be bumped together (the host stamps it into every Init \
         frame and the worker requires equality)"
    );
}
