//! Named shared-memory segment: creation, mapping, and whole-segment
//! byte-level access.
//!
//! Ported from the sibling project's `mmm-core::ipc::shm`
//! (`~/dev/MergeMosaic/crates/mmm-core/src/ipc/shm.rs`). Per `mpp-ipc`'s
//! wire contract (`docs/pi-protocol.md` §6), the host (PixInsight module,
//! Plan B) [`ShmSegment::create`]s one segment sized `slot_bytes * slots`
//! and hands its name to the worker in the `Init` message; the worker
//! [`ShmSegment::open`]s the same segment. Slot arithmetic
//! (`slot_offset(i) = slot_bytes * i`) is *not* this module's concern —
//! callers index into the whole-segment byte slice returned by
//! [`ShmSegment::as_slice`] / [`ShmSegment::as_mut_slice`] themselves.
//!
//! **Omitted relative to the MMM source this was ported from:** MMM's
//! `SlotLayout` type, and its f32-typed, offset+len `slice`/`slice_mut`
//! accessors with their `checked_range` bounds/alignment enforcement.
//! Those lived on MMM's `ShmSegment` because it *was* the slot-aware
//! accessor; here the slot layout lives one level up (implemented against
//! the raw bytes this module hands back), so this module only needs to
//! create/open a segment and expose its bytes. `mmm`'s `attach` is renamed
//! `open` here to match this crate's target interface.

use std::io;

/// Errors from creating, opening, or mapping a named shared-memory segment.
#[derive(Debug, thiserror::Error)]
pub enum ShmError {
    /// [`ShmSegment::create`] failed: the name was rejected, the OS refused
    /// to create the named object, or sizing it (`ftruncate` / the Windows
    /// equivalent) failed.
    #[error("failed to create shared-memory segment {name:?}: {source}")]
    Create {
        /// The name that was being created.
        name: String,
        /// The underlying OS (or validation) error.
        #[source]
        source: io::Error,
    },
    /// [`ShmSegment::open`] failed: no segment exists under that name, or
    /// the existing segment is smaller than the requested size.
    #[error("failed to open shared-memory segment {name:?}: {source}")]
    Open {
        /// The name that was being opened.
        name: String,
        /// The underlying OS (or validation) error.
        #[source]
        source: io::Error,
    },
    /// Mapping an already-created-or-opened segment's memory into this
    /// process failed.
    #[error("failed to map shared-memory segment {name:?}: {source}")]
    Map {
        /// The segment name.
        name: String,
        /// The underlying OS error.
        #[source]
        source: io::Error,
    },
}

impl ShmError {
    fn create(name: &str, source: impl Into<io::Error>) -> Self {
        Self::Create {
            name: name.to_string(),
            source: source.into(),
        }
    }

    fn open(name: &str, source: impl Into<io::Error>) -> Self {
        Self::Open {
            name: name.to_string(),
            source: source.into(),
        }
    }

    fn map(name: &str, source: impl Into<io::Error>) -> Self {
        Self::Map {
            name: name.to_string(),
            source: source.into(),
        }
    }
}

/// Portable shm-name validation, enforced by [`ShmSegment::create`] on
/// every platform so a name works everywhere or nowhere: POSIX requires a
/// single leading `/`; macOS additionally caps names at 31 characters
/// (`PSHMNAMLEN`) and fails longer ones with a bare `EINVAL` — enforcing
/// the limit here turns that into a clear message on the developer's
/// platform instead of a macOS-only CI failure. Ported from
/// `validate_shm_name` in MMM's `mmm-core::ipc::shm` (itself mirroring
/// `validate_shm_name` in MMM's C++ host, `mmm_shm.cpp`).
#[cfg(any(unix, windows))]
fn validate_shm_name(name: &str) -> io::Result<()> {
    const MAX_LEN: usize = 31; // macOS PSHMNAMLEN
    if name.len() < 2 || !name.starts_with('/') || name[1..].contains('/') || name.len() > MAX_LEN {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "shm name {name:?} is invalid: it must start with '/', contain no other \
                 slashes, and be at most {MAX_LEN} characters (the macOS PSHMNAMLEN \
                 limit, enforced on every platform)"
            ),
        ));
    }
    Ok(())
}

/// A named shared-memory segment mapped into this process.
///
/// The host [`create`](ShmSegment::create)s the segment; the worker
/// [`open`](ShmSegment::open)s the same name. Both sides read/write the
/// mapping through [`as_slice`](ShmSegment::as_slice) /
/// [`as_mut_slice`](ShmSegment::as_mut_slice); it is the caller's
/// responsibility to only read a byte range after the host has filled it
/// (via the control-channel `Band`/`BandAck` handshake — see
/// `docs/pi-protocol.md` §6) and to keep concurrent accesses to disjoint
/// ranges.
#[cfg(unix)]
#[derive(Debug)]
pub struct ShmSegment {
    name: String,
    is_creator: bool,
    ptr: std::ptr::NonNull<std::ffi::c_void>,
    /// Logical size in bytes, as requested by the caller (distinct from the
    /// OS allocation, which is clamped to a minimum of 1 byte — see
    /// `create`/`open`).
    size: usize,
}

// SAFETY: `ShmSegment` wraps a raw pointer into a shared-memory mapping, so
// it does not get `Send`/`Sync` for free. The pointer is never subject to
// interior mutability from this type's own API — `as_mut_slice` requires
// `&mut self`, so ordinary borrow-checker exclusivity governs writes — and
// the underlying mapping is valid memory shared by the OS across processes
// and threads alike, so moving or sharing a `ShmSegment` across threads is
// sound. (MMM's Unix impl got this for free from `memmap2::MmapMut`, which
// is itself `Send + Sync`; this port maps directly via `nix::sys::mman`
// instead, since `mpp-ipc` does not depend on `memmap2`, so the impls are
// spelled out explicitly here.)
#[cfg(unix)]
unsafe impl Send for ShmSegment {}
#[cfg(unix)]
unsafe impl Sync for ShmSegment {}

#[cfg(unix)]
impl ShmSegment {
    /// Create a new shared-memory segment named `name`, sized `size` bytes,
    /// and map it into this process.
    ///
    /// Any stale segment left behind under `name` (e.g. from a crashed
    /// previous run) is unlinked first so `create` never fails with
    /// "already exists".
    ///
    /// macOS limits POSIX shm object names to 31 characters (`PSHMNAMLEN`),
    /// including the leading `/`; `shm_open`/`shm_unlink` fail with
    /// `EINVAL` beyond that. `validate_shm_name` enforces the limit on
    /// every platform up front, so a long ad hoc name fails with a clear
    /// message rather than only on macOS.
    pub fn create(name: &str, size: usize) -> Result<Self, ShmError> {
        use nix::fcntl::OFlag;
        use nix::sys::mman::{shm_open, shm_unlink};
        use nix::sys::stat::Mode;

        validate_shm_name(name).map_err(|e| ShmError::create(name, e))?;

        // Best-effort cleanup of a stale segment left behind by a crashed
        // prior run: ENOENT is the normal case, and any other failure is
        // deliberately ignored too — the shm_open below reports the real,
        // actionable error for this name.
        let _ = shm_unlink(name);

        let fd = shm_open(
            name,
            OFlag::O_CREAT | OFlag::O_EXCL | OFlag::O_RDWR,
            Mode::S_IRUSR | Mode::S_IWUSR,
        )
        .map_err(|e| ShmError::create(name, e))?;

        // A zero-byte segment is a legitimate degenerate case (e.g. a job
        // with no bands to transfer). macOS's `mmap` rejects a
        // zero-length mapping with EINVAL, so allocate (and map) a minimum
        // of 1 byte that is never accessed — `size` (below) stays the
        // caller's requested `size`, not this allocation, so `as_slice`/
        // `as_mut_slice` still hand back a zero-length slice for a
        // zero-byte segment.
        let alloc = size.max(1);

        if let Err(e) = nix::unistd::ftruncate(&fd, alloc as libc::off_t) {
            // We already created the (now zero-length) named object;
            // leaving it behind would only be cleaned up by a future
            // `create` of the same name unlinking it first. Unlink it now
            // instead so a failed `create` doesn't leak the shm object.
            let _ = shm_unlink(name);
            return Err(ShmError::create(name, e));
        }

        let file = std::fs::File::from(fd);
        // Map an explicit length rather than the file's fstat-derived
        // length: macOS page-rounds a POSIX shm object's size up to the
        // page size (16 KiB on Apple Silicon, vs. 4 KiB on Linux), so
        // mapping "the whole file" would give a mapping longer than `size`
        // and drift the segment's logical size away from what the caller
        // asked for. An explicit length maps exactly `alloc` (== `size`,
        // except in the zero-byte case above) on every platform.
        let ptr = unsafe {
            nix::sys::mman::mmap(
                None,
                std::num::NonZeroUsize::new(alloc).expect("alloc is at least 1"),
                nix::sys::mman::ProtFlags::PROT_READ | nix::sys::mman::ProtFlags::PROT_WRITE,
                nix::sys::mman::MapFlags::MAP_SHARED,
                &file,
                0,
            )
        }
        .map_err(|e| ShmError::map(name, e))?;
        // `file` (and the fd it owns) is dropped here; the mapping it
        // produced remains valid independent of the fd's lifetime.

        Ok(ShmSegment {
            name: name.to_string(),
            is_creator: true,
            ptr,
            size,
        })
    }

    /// Open an existing shared-memory segment named `name`, sized `size`
    /// bytes, and map it into this process.
    ///
    /// The segment must already exist (created by
    /// [`create`](ShmSegment::create) in another process); `open` never
    /// creates, resizes, or unlinks it — only the creator owns the
    /// segment's size. The underlying object must be *at least* `size`
    /// bytes (on Linux the creator's `ftruncate` makes it exactly `size`;
    /// on macOS the OS page-rounds a shm object's size up, so it may be
    /// larger) — a too-small object is rejected rather than silently
    /// mapping a truncated segment. The mapping `open` produces is always
    /// exactly `size` bytes, regardless of the underlying object's actual
    /// (possibly page-rounded) size.
    pub fn open(name: &str, size: usize) -> Result<Self, ShmError> {
        use nix::fcntl::OFlag;
        use nix::sys::mman::shm_open;
        use nix::sys::stat::Mode;

        let fd = shm_open(name, OFlag::O_RDWR, Mode::S_IRUSR | Mode::S_IWUSR)
            .map_err(|e| ShmError::open(name, e))?;

        let file = std::fs::File::from(fd);

        // Validate the underlying object is large enough before mapping:
        // an exact-equality check here would fail spuriously on macOS,
        // where the creator's `ftruncate(size)` gets rounded up to the
        // page size by the OS.
        let actual = file.metadata().map_err(|e| ShmError::open(name, e))?.len();
        if actual < size as u64 {
            return Err(ShmError::open(
                name,
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "shm segment {name} is {actual} bytes, but open expected at least {size}"
                    ),
                ),
            ));
        }

        // See the matching comment in `create`: a zero-byte segment is a
        // legitimate degenerate case, and macOS's `mmap` rejects a
        // zero-length mapping, so map at least 1 byte (never accessed).
        let alloc = size.max(1);

        // Map an explicit length (see the matching comment in `create`) so
        // the mapping is exactly `alloc` bytes, independent of the
        // underlying object's (possibly page-rounded) actual size.
        let ptr = unsafe {
            nix::sys::mman::mmap(
                None,
                std::num::NonZeroUsize::new(alloc).expect("alloc is at least 1"),
                nix::sys::mman::ProtFlags::PROT_READ | nix::sys::mman::ProtFlags::PROT_WRITE,
                nix::sys::mman::MapFlags::MAP_SHARED,
                &file,
                0,
            )
        }
        .map_err(|e| ShmError::map(name, e))?;

        Ok(ShmSegment {
            name: name.to_string(),
            is_creator: false,
            ptr,
            size,
        })
    }

    /// Borrow the whole segment as bytes.
    pub fn as_slice(&self) -> &[u8] {
        // SAFETY: `ptr` was mapped for `size` bytes (or, for a zero-byte
        // segment, a never-accessed 1-byte allocation with `size == 0`) by
        // `create`/`open` and stays valid for the lifetime of `self`.
        unsafe { std::slice::from_raw_parts(self.ptr.as_ptr() as *const u8, self.size) }
    }

    /// Mutably borrow the whole segment as bytes.
    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        // SAFETY: as `as_slice`, plus `&mut self` gives exclusive access
        // for the duration of the borrow.
        unsafe { std::slice::from_raw_parts_mut(self.ptr.as_ptr() as *mut u8, self.size) }
    }

    /// The segment's name, as passed to [`create`](Self::create) or
    /// [`open`](Self::open).
    pub fn name(&self) -> &str {
        &self.name
    }
}

#[cfg(unix)]
impl Drop for ShmSegment {
    fn drop(&mut self) {
        let alloc = self.size.max(1);
        // SAFETY: `ptr` was mapped for exactly `alloc` bytes by
        // `create`/`open` and is unmapped exactly once, here.
        let _ = unsafe { nix::sys::mman::munmap(self.ptr, alloc) };
        // Only the creator removes the underlying named object so that an
        // opened-but-not-created segment's drop doesn't yank it out from
        // under a still-running host (or a second worker).
        if self.is_creator {
            let _ = nix::sys::mman::shm_unlink(self.name.as_str());
        }
    }
}

/// Windows named shared-memory segment (a pagefile-backed file mapping),
/// mirroring the POSIX [`ShmSegment`]: the host `create`s a named mapping
/// and the worker `open`s the same name. The name carried in the JSON
/// `Init` message is the POSIX-style `/name`; both sides normalize it
/// identically to a Windows object name (see [`win_object_name`]).
#[cfg(windows)]
#[derive(Debug)]
pub struct ShmSegment {
    name: String,
    is_creator: bool,
    handle: windows_sys::Win32::Foundation::HANDLE,
    base: *mut u8,
    /// Logical size in bytes, as requested by the caller (distinct from
    /// the OS allocation, which is clamped to a minimum of 1 byte — see
    /// `create`/`open`).
    size: usize,
}

// SAFETY: identical contract to the Unix impl above (see its comment). The
// raw `base` pointer is only ever handed out as `&[u8]`/`&mut [u8]` through
// `as_slice`/`as_mut_slice`, which follow ordinary borrow-checker
// exclusivity (`as_mut_slice` requires `&mut self`); the `HANDLE` is only
// closed on `Drop`.
#[cfg(windows)]
unsafe impl Send for ShmSegment {}
#[cfg(windows)]
unsafe impl Sync for ShmSegment {}

/// Normalize a POSIX-style shm name (`/mpp-foo`) to a Windows object name
/// (`Local\mpp-foo`). Applied identically here and in the C++ host so the
/// name string exchanged in the `Init` message round-trips.
#[cfg(windows)]
fn win_object_name(name: &str) -> Vec<u16> {
    let base = name.strip_prefix('/').unwrap_or(name);
    let full = format!("Local\\{base}");
    full.encode_utf16().chain(std::iter::once(0)).collect()
}

#[cfg(windows)]
impl ShmSegment {
    /// Create a new named pagefile-backed segment of `size` bytes and map it.
    pub fn create(name: &str, size: usize) -> Result<Self, ShmError> {
        use windows_sys::Win32::Foundation::{GetLastError, INVALID_HANDLE_VALUE};
        use windows_sys::Win32::System::Memory::{
            CreateFileMappingW, FILE_MAP_ALL_ACCESS, MapViewOfFile, PAGE_READWRITE,
        };
        // Windows file mappings have no PSHMNAMLEN limit, but the guard
        // runs here too so a name is valid on every platform or none.
        validate_shm_name(name).map_err(|e| ShmError::create(name, e))?;
        let wname = win_object_name(name);
        // A zero-byte segment is a legitimate degenerate case (e.g. a job
        // with no bands to transfer). `CreateFileMappingW` rejects a
        // zero-size mapping with ERROR_INVALID_PARAMETER, so allocate (and
        // map) a minimum of 1 byte that is never accessed — `self.size`
        // stays the caller's requested `size` (not this allocation), so
        // `as_slice`/`as_mut_slice` still hand back a zero-length slice
        // for a zero-byte segment.
        let alloc = size.max(1) as u64;
        let hi = (alloc >> 32) as u32;
        let lo = (alloc & 0xFFFF_FFFF) as u32;
        // SAFETY: FFI; INVALID_HANDLE_VALUE requests a pagefile-backed mapping.
        let handle = unsafe {
            CreateFileMappingW(
                INVALID_HANDLE_VALUE,
                std::ptr::null(),
                PAGE_READWRITE,
                hi,
                lo,
                wname.as_ptr(),
            )
        };
        if handle.is_null() {
            let e = unsafe { GetLastError() };
            return Err(ShmError::create(
                name,
                io::Error::from_raw_os_error(e as i32),
            ));
        }
        // SAFETY: FFI; map the whole allocation ([0, alloc)).
        let view = unsafe { MapViewOfFile(handle, FILE_MAP_ALL_ACCESS, 0, 0, alloc as usize) };
        if view.Value.is_null() {
            let e = unsafe { GetLastError() };
            unsafe { windows_sys::Win32::Foundation::CloseHandle(handle) };
            return Err(ShmError::map(name, io::Error::from_raw_os_error(e as i32)));
        }
        Ok(ShmSegment {
            name: name.to_string(),
            is_creator: true,
            handle,
            base: view.Value as *mut u8,
            size,
        })
    }

    /// Open an existing named segment created by [`create`](Self::create).
    ///
    /// The mapping is opened by name and a view of `size` bytes is mapped;
    /// requesting more than the creator allocated fails, so an oversized
    /// `size` is rejected. Unlike the POSIX impl, Windows offers no cheap
    /// exact-size assertion, so a *smaller* `size` within a larger segment
    /// is accepted — the creator's size is the contract.
    pub fn open(name: &str, size: usize) -> Result<Self, ShmError> {
        use windows_sys::Win32::Foundation::{CloseHandle, GetLastError};
        use windows_sys::Win32::System::Memory::{
            FILE_MAP_ALL_ACCESS, MapViewOfFile, OpenFileMappingW,
        };
        let wname = win_object_name(name);
        // SAFETY: FFI.
        let handle = unsafe { OpenFileMappingW(FILE_MAP_ALL_ACCESS, 0, wname.as_ptr()) };
        if handle.is_null() {
            let e = unsafe { GetLastError() };
            return Err(ShmError::open(name, io::Error::from_raw_os_error(e as i32)));
        }
        // See the matching comment in `create`: a zero-byte segment is a
        // legitimate degenerate case, and `MapViewOfFile` rejects a
        // zero-size view, so map at least 1 byte (never accessed); `size`
        // (below) stays the caller's requested size, not this allocation.
        let alloc = size.max(1);
        // SAFETY: FFI.
        let view = unsafe { MapViewOfFile(handle, FILE_MAP_ALL_ACCESS, 0, 0, alloc) };
        if view.Value.is_null() {
            let e = unsafe { GetLastError() };
            unsafe { CloseHandle(handle) };
            return Err(ShmError::map(name, io::Error::from_raw_os_error(e as i32)));
        }
        Ok(ShmSegment {
            name: name.to_string(),
            is_creator: false,
            handle,
            base: view.Value as *mut u8,
            size,
        })
    }

    /// Borrow the whole segment as bytes.
    pub fn as_slice(&self) -> &[u8] {
        // SAFETY: `base` was mapped for at least `size` bytes by
        // `create`/`open` and stays valid for the lifetime of `self`.
        unsafe { std::slice::from_raw_parts(self.base, self.size) }
    }

    /// Mutably borrow the whole segment as bytes.
    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        // SAFETY: as `as_slice`, plus `&mut self` gives exclusive access
        // for the duration of the borrow.
        unsafe { std::slice::from_raw_parts_mut(self.base, self.size) }
    }

    /// The segment's name, as passed to [`create`](Self::create) or
    /// [`open`](Self::open).
    pub fn name(&self) -> &str {
        &self.name
    }
}

#[cfg(windows)]
impl Drop for ShmSegment {
    fn drop(&mut self) {
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::Memory::{MEMORY_MAPPED_VIEW_ADDRESS, UnmapViewOfFile};
        // SAFETY: `base`/`handle` came from a successful map in
        // create/open. A Windows file mapping is refcounted by open
        // handles and vanishes when the last handle closes — there is no
        // `shm_unlink` analog, so both the creator and openers simply
        // unmap + close. `is_creator` (and `name`) are retained for
        // symmetry with the Unix impl and for this debug trace; they
        // drive no special teardown.
        tracing::trace!(
            name = %self.name,
            is_creator = self.is_creator,
            "dropping ShmSegment"
        );
        unsafe {
            UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS {
                Value: self.base as *mut core::ffi::c_void,
            });
            CloseHandle(self.handle);
        }
    }
}

/// A named shared-memory segment.
///
/// Shared memory is a POSIX/Windows-only transport; this platform stub
/// exists only so `mpp-ipc` compiles on other targets. Every operation
/// fails with [`ShmError`].
#[cfg(all(not(unix), not(windows)))]
#[derive(Debug)]
pub struct ShmSegment {
    _unused: (),
}

#[cfg(all(not(unix), not(windows)))]
impl ShmSegment {
    /// Always fails: shared memory is not supported on this platform.
    pub fn create(name: &str, _size: usize) -> Result<Self, ShmError> {
        Err(ShmError::create(
            name,
            io::Error::new(
                io::ErrorKind::Unsupported,
                "shared memory is not supported on this platform",
            ),
        ))
    }

    /// Always fails: shared memory is not supported on this platform.
    pub fn open(name: &str, _size: usize) -> Result<Self, ShmError> {
        Err(ShmError::open(
            name,
            io::Error::new(
                io::ErrorKind::Unsupported,
                "shared memory is not supported on this platform",
            ),
        ))
    }

    /// Unreachable: no `ShmSegment` can be constructed on this platform.
    pub fn as_slice(&self) -> &[u8] {
        unreachable!("ShmSegment cannot be constructed on non-unix, non-windows platforms")
    }

    /// Unreachable: no `ShmSegment` can be constructed on this platform.
    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        unreachable!("ShmSegment cannot be constructed on non-unix, non-windows platforms")
    }

    /// Unreachable: no `ShmSegment` can be constructed on this platform.
    pub fn name(&self) -> &str {
        unreachable!("ShmSegment cannot be constructed on non-unix, non-windows platforms")
    }
}

#[cfg(all(test, windows))]
mod win_tests {
    use super::*;

    // Names here follow the portable rule `validate_shm_name` enforces on
    // every platform (leading '/', no other slashes, <= 31 chars) even
    // though Windows itself doesn't care — `win_object_name` strips the
    // slash when building the `Local\...` mapping name.

    #[test]
    fn create_write_open_read_same_bytes() {
        let name = format!("/mpp-wt-{}", std::process::id());
        let size = 4096usize;
        let mut host = ShmSegment::create(&name, size).unwrap();
        host.as_mut_slice()[..4].copy_from_slice(&[1, 2, 3, 4]);
        let worker = ShmSegment::open(&name, size).unwrap();
        assert_eq!(&worker.as_slice()[..4], &[1, 2, 3, 4]);
    }

    #[test]
    fn zero_byte_segment_creates_with_empty_slice() {
        // A degenerate zero-byte segment; `CreateFileMappingW` rejects a
        // zero-size mapping, so `create` clamps its OS allocation to a
        // minimum of 1 byte. The segment still reports (and enforces) a
        // logical size of 0.
        let name = format!("/mpp-wt-zero-{}", std::process::id());
        let host = ShmSegment::create(&name, 0).unwrap();
        assert_eq!(host.as_slice(), &[] as &[u8]);
    }

    #[test]
    fn opened_segment_drop_does_not_unlink() {
        let name = format!("/mpp-wt-dropunlink-{}", std::process::id());
        let host = ShmSegment::create(&name, 64).unwrap();
        {
            let worker = ShmSegment::open(&name, 64).unwrap();
            drop(worker);
        }
        // Only the creator's drop tears down the mapping object; an
        // opener's drop must leave it intact for the host (and any other
        // opener) to keep using.
        assert!(ShmSegment::open(&name, 64).is_ok());
        drop(host);
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn create_write_open_read_same_bytes() {
        let name = format!("/mpp-shm-test-{}", std::process::id());
        let size = 4096usize;
        let mut host = ShmSegment::create(&name, size).unwrap();
        host.as_mut_slice()[..4].copy_from_slice(&[1, 2, 3, 4]);
        let worker = ShmSegment::open(&name, size).unwrap();
        assert_eq!(&worker.as_slice()[..4], &[1, 2, 3, 4]);
        // Cleanup happens on host drop (unlink); worker drop just unmaps.
    }

    #[test]
    fn zero_byte_segment_creates_with_empty_slice() {
        // A degenerate zero-byte segment; macOS's `mmap` rejects a
        // zero-length mapping, so `create` clamps its OS allocation to a
        // minimum of 1 byte. The segment still reports (and enforces) a
        // logical size of 0.
        let name = format!("/mpp-shm-test-zero-{}", std::process::id());
        let host = ShmSegment::create(&name, 0).unwrap();
        assert_eq!(host.as_slice(), &[] as &[u8]);
    }

    /// Names must be portable across every platform we ship: leading '/'
    /// only, and at most 31 chars — the macOS PSHMNAMLEN limit, enforced
    /// everywhere so a too-long name fails with this clear message on the
    /// developer's platform instead of a bare EINVAL only on macOS CI.
    #[test]
    fn create_rejects_unportable_names() {
        for name in [
            "/mpp-this-name-is-well-over-thirty-one-chars",
            "no-leading-slash",
            "/mpp/embedded/slashes",
        ] {
            let err = ShmSegment::create(name, 4096).unwrap_err();
            assert!(
                err.to_string().contains("shm name"),
                "{name} not refused clearly: {err}"
            );
        }
    }

    #[test]
    fn create_unlinks_stale_segment_before_recreating() {
        let name = format!("/mpp-shm-stale-{}", std::process::id());
        let first = ShmSegment::create(&name, 64).unwrap();
        // Leak the first segment's Rust value without running its Drop
        // (which would unlink it itself), simulating a crashed prior run
        // that leaves the named object behind.
        std::mem::forget(first);

        // A second `create` of the same name must succeed by unlinking the
        // stale object first, not fail with "already exists".
        let second = ShmSegment::create(&name, 64).unwrap();
        drop(second);
    }

    #[test]
    fn opened_segment_drop_does_not_unlink() {
        let name = format!("/mpp-shm-dropunlink-{}", std::process::id());
        let host = ShmSegment::create(&name, 64).unwrap();
        {
            let worker = ShmSegment::open(&name, 64).unwrap();
            drop(worker);
        }
        // Only the creator's drop unlinks the named object; an opener's
        // drop must leave it intact for the host (and any other opener)
        // to keep using.
        assert!(ShmSegment::open(&name, 64).is_ok());
        drop(host);
    }

    #[test]
    fn open_rejects_undersized_existing_segment() {
        let name = format!("/mpp-shm-undersize-{}", std::process::id());
        let host = ShmSegment::create(&name, 64).unwrap();
        // `open` requires the object to be AT LEAST the requested size, and
        // the OS may page-round the creator's 64 bytes up (macOS rounds a
        // POSIX shm object to its 16 KiB page size, so opening with 128
        // would legitimately succeed there). Request a size beyond any
        // plausible page rounding so the rejection is observable on every
        // platform.
        let err = ShmSegment::open(&name, 1 << 20).unwrap_err();
        assert!(
            err.to_string().contains("expected at least 1048576"),
            "error should mention the required size: {err}"
        );
        drop(host);
    }

    #[test]
    fn name_returns_the_name_passed_to_create() {
        let name = format!("/mpp-shm-name-{}", std::process::id());
        let host = ShmSegment::create(&name, 64).unwrap();
        assert_eq!(host.name(), name);
    }
}
