//! Finding the stamps of many files at once with `io_uring`, which submits a batch of `statx` calls in one system call,
//! rather than one system call for each file.
//!
//! It is only for Linux on 64-bit little-endian machines, as the submission and completion entries, and the `statx`
//! buffers, are read and written as words in that layout. A ring which cannot be set up, such as where a kernel is too
//! old or `io_uring` is disabled, leaves the stamps to be found by [`crate::stamp`]'s threads instead.

use std::ffi::{CString, c_int, c_long, c_void};
use std::sync::atomic::{AtomicU32, Ordering};

use crate::stamp::Stamp;

unsafe extern "C" {
    fn syscall(number: c_long, ...) -> c_long;
    fn mmap(addr: *mut c_void, len: usize, prot: c_int, flags: c_int, fd: c_int, offset: i64) -> *mut c_void;
    fn munmap(addr: *mut c_void, len: usize) -> c_int;
    fn close(fd: c_int) -> c_int;
}

// the numbers of the system calls, which are the same on every architecture since io_uring was added
const SYS_IO_URING_SETUP: c_long = 425;
const SYS_IO_URING_ENTER: c_long = 426;

const IORING_OFF_SQ_RING: i64 = 0;
const IORING_OFF_CQ_RING: i64 = 0x800_0000;
const IORING_OFF_SQES: i64 = 0x1000_0000;
const IORING_ENTER_GETEVENTS: c_long = 1;
const IORING_OP_STATX: u64 = 21;

const PROT_READ: c_int = 1;
const PROT_WRITE: c_int = 2;
const MAP_SHARED: c_int = 1;
const MAP_POPULATE: c_int = 0x8000;
const MAP_FAILED: *mut c_void = !0 as *mut c_void;

const AT_FDCWD: i32 = -100;
const STATX_MTIME: u64 = 0x40;
const STATX_SIZE: u64 = 0x200;
const EINTR: c_long = 4;

/// The number of entries of the submission queue, which is the most `statx` calls submitted at once.
const ENTRIES: u32 = 256;

/// `struct io_uring_params`, which `io_uring_setup` fills in.
#[repr(C)]
#[derive(Default)]
struct Params {
    sq_entries: u32,
    cq_entries: u32,
    flags: u32,
    sq_thread_cpu: u32,
    sq_thread_idle: u32,
    features: u32,
    wq_fd: u32,
    resv: [u32; 3],
    /// `struct io_sqring_offsets`: head, tail, ring mask, ring entries, flags, dropped, array, then reserved words.
    sq_off: [u32; 10],
    /// `struct io_cqring_offsets`: head, tail, ring mask, ring entries, overflow, cqes, flags, then reserved words.
    cq_off: [u32; 10],
}

/// A memory mapping of the ring, which is unmapped when it is dropped.
struct Map {
    ptr: *mut c_void,
    len: usize,
}
impl Map {
    fn new(fd: c_int, len: usize, offset: i64) -> std::io::Result<Self> {
        let (prot, flags) = (PROT_READ | PROT_WRITE, MAP_SHARED | MAP_POPULATE);
        // SAFETY: a new shared mapping of the ring's file descriptor, which the kernel checks
        let ptr = unsafe { mmap(std::ptr::null_mut(), len, prot, flags, fd, offset) };
        if ptr == MAP_FAILED {
            return Err(std::io::Error::last_os_error());
        }
        Ok(Self { ptr, len })
    }
    /// Returns a pointer to a `T` at a byte offset in the mapping, which is aligned for it, as the mapping is aligned
    /// to a page and the kernel's offsets are aligned for what is at them.
    ///
    /// # Panics
    ///
    /// If the `T` would not be within the mapping, or the offset is not aligned for it.
    fn at<T>(&self, offset: usize) -> *mut T {
        assert!(offset + size_of::<T>() <= self.len && offset.is_multiple_of(align_of::<T>()));
        // SAFETY: the offset is within the mapping
        unsafe { self.ptr.byte_add(offset) }.cast::<T>()
    }
    /// Returns the word of the ring at an offset, which the kernel reads and writes too.
    fn atomic(&self, offset: u32) -> &AtomicU32 {
        // SAFETY: the word is within the mapping and aligned, and lives as long as `self`
        unsafe { &*self.at::<AtomicU32>(offset as usize) }
    }
    fn word(&self, offset: u32) -> u32 {
        self.atomic(offset).load(Ordering::Relaxed)
    }
}
impl Drop for Map {
    fn drop(&mut self) {
        // SAFETY: the mapping was made by `mmap` with this length, and is no longer used
        unsafe { munmap(self.ptr, self.len) };
    }
}

/// An `io_uring`, with its submission queue, completion queue and submission entries mapped.
pub struct Ring {
    fd: c_int,
    params: Params,
    sq: Map,
    cq: Map,
    sqes: Map,
}

impl Ring {
    /// Set up a ring.
    ///
    /// # Errors
    ///
    /// If `io_uring` is not supported or is disabled, or the ring cannot be mapped.
    pub fn new() -> std::io::Result<Self> {
        let mut params = Params::default();
        // SAFETY: `params` is a `struct io_uring_params`, which the kernel fills in
        let fd = unsafe { syscall(SYS_IO_URING_SETUP, c_long::from(ENTRIES), &raw mut params) };
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let fd = fd as c_int;
        let map = || -> std::io::Result<(Map, Map, Map)> {
            let sq_len = params.sq_off[6] as usize + params.sq_entries as usize * 4;
            let cq_len = params.cq_off[5] as usize + params.cq_entries as usize * 16;
            let sqes_len = params.sq_entries as usize * 64;
            let sq = Map::new(fd, sq_len, IORING_OFF_SQ_RING)?;
            let cq = Map::new(fd, cq_len, IORING_OFF_CQ_RING)?;
            Ok((sq, cq, Map::new(fd, sqes_len, IORING_OFF_SQES)?))
        };
        match map() {
            Ok((sq, cq, sqes)) => Ok(Self { fd, params, sq, cq, sqes }),
            Err(error) => {
                // SAFETY: the file descriptor is the ring's, which is not used again
                unsafe { close(fd) };
                Err(error)
            },
        }
    }
    /// Returns the stamps of files, given by their paths, in the same order, as [`Stamp::of`] finds them.
    pub fn stamps(&mut self, paths: &[&std::path::Path]) -> Vec<Stamp> {
        let mut stamps = vec![Stamp::Missing; paths.len()];
        let entries = self.params.sq_entries as usize;
        for (paths, stamps) in paths.chunks(entries).zip(stamps.chunks_mut(entries)) {
            self.stamp_chunk(paths, stamps);
        }
        stamps
    }
    /// Stamp at most as many files as the submission queue holds, submitting them together.
    fn stamp_chunk(&mut self, paths: &[&std::path::Path], stamps: &mut [Stamp]) {
        use std::os::unix::ffi::OsStrExt;
        // a path with a NUL byte cannot be a file, so it is missing
        let names: Vec<Option<CString>> =
            paths.iter().map(|path| CString::new(path.as_os_str().as_bytes()).ok()).collect();
        // a `struct statx` for each path, which the kernel fills in, as 32 words, which are aligned as it needs
        let mut buffers = vec![[0_u64; 32]; paths.len()];
        let (mask, entries) = (self.word_sq(2), self.params.sq_entries);
        let tail = self.sq.atomic(self.params.sq_off[1]);
        let mut next = tail.load(Ordering::Acquire);
        let mut submitted: u32 = 0;
        for (index, (name, buffer)) in names.iter().zip(&mut buffers).enumerate() {
            let Some(name) = name else {
                continue;
            };
            let slot = next & mask;
            let sqe = [
                IORING_OP_STATX | (u64::from(AT_FDCWD.cast_unsigned()) << 32),
                buffer.as_mut_ptr() as u64,
                name.as_ptr() as u64,
                STATX_MTIME | STATX_SIZE,
                index as u64,
                0,
                0,
                0,
            ];
            assert!(slot < entries);
            // SAFETY: the slot is within the submission entries, which the kernel does not read until the tail moves
            unsafe {
                self.sqes.at::<[u64; 8]>(slot as usize * 64).write(sqe);
                self.sq.at::<u32>(self.params.sq_off[6] as usize + slot as usize * 4).write(slot);
            }
            next = next.wrapping_add(1);
            submitted += 1;
        }
        tail.store(next, Ordering::Release);
        let mut completed = 0;
        while completed < submitted {
            let waiting = submitted - completed;
            let to_submit = if completed == 0 { submitted } else { 0 };
            let (fd, to_submit, waiting) = (c_long::from(self.fd), c_long::from(to_submit), c_long::from(waiting));
            // SAFETY: the ring's file descriptor, with no signal mask
            let entered = unsafe { syscall(SYS_IO_URING_ENTER, fd, to_submit, waiting, IORING_ENTER_GETEVENTS, 0, 0) };
            if entered < 0 && std::io::Error::last_os_error().raw_os_error() != Some(EINTR as i32) {
                // the files which were not stamped are found another way
                for (stamp, path) in stamps.iter_mut().zip(paths) {
                    *stamp = Stamp::of(path);
                }
                return;
            }
            completed += self.reap(&buffers, stamps);
        }
    }
    fn word_sq(&self, index: usize) -> u32 {
        self.sq.word(self.params.sq_off[index])
    }
    /// Take the completions which are ready, returning their number.
    fn reap(&self, buffers: &[[u64; 32]], stamps: &mut [Stamp]) -> u32 {
        let head = self.cq.atomic(self.params.cq_off[0]);
        let tail = self.cq.atomic(self.params.cq_off[1]).load(Ordering::Acquire);
        let mask = self.cq.word(self.params.cq_off[2]);
        let mut current = head.load(Ordering::Relaxed);
        let mut reaped = 0;
        while current != tail {
            let offset = self.params.cq_off[5] as usize + (current & mask) as usize * 16;
            assert!(offset + 16 <= self.cq.len);
            // SAFETY: the completion is within the mapping, and the kernel wrote it before moving the tail
            let user_data = unsafe { self.cq.at::<u64>(offset).read() };
            // SAFETY: as above
            let result = unsafe { self.cq.at::<i32>(offset + 8).read() };
            let index = user_data as usize;
            if result == 0 && let (Some(buffer), Some(stamp)) = (buffers.get(index), stamps.get_mut(index)) {
                *stamp = stamp_of(buffer);
            }
            current = current.wrapping_add(1);
            reaped += 1;
        }
        head.store(current, Ordering::Release);
        reaped
    }
}
impl Drop for Ring {
    fn drop(&mut self) {
        // SAFETY: the file descriptor is the ring's, whose mappings are unmapped after it, which the kernel allows
        unsafe { close(self.fd) };
    }
}

/// Returns the stamp of a file from its `struct statx`, as 32 words: the size is at byte 40, and the seconds and
/// nanoseconds of the modification time at bytes 112 and 120.
fn stamp_of(statx: &[u64; 32]) -> Stamp {
    let mask = statx[0] & 0xffff_ffff;
    if mask & (STATX_MTIME | STATX_SIZE) != STATX_MTIME | STATX_SIZE {
        // a file system without them is stamped as `Stamp::of` stamps it
        return Stamp::Missing;
    }
    let seconds = statx[14].cast_signed();
    let nanos = i64::from((statx[15] & 0xffff_ffff) as u32);
    Stamp::File { mtime: seconds.saturating_mul(1_000_000_000).saturating_add(nanos), size: statx[5] }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stamps_as_stat_does() {
        let Ok(mut ring) = Ring::new() else {
            // io_uring is disabled here, so there is nothing to compare
            return;
        };
        let dir = std::env::temp_dir().join(format!("rig-uring-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        let mut paths = Vec::new();
        // more than one batch, with files missing, a directory, and a symbolic link, which is followed
        for i in 0..600 {
            let path = dir.join(format!("f{i}"));
            if i % 7 != 0 {
                std::fs::write(&path, "x".repeat(i)).unwrap();
            }
            paths.push(path);
        }
        paths.push(dir.join("sub"));
        std::os::unix::fs::symlink(dir.join("f1"), dir.join("link")).unwrap();
        paths.push(dir.join("link"));
        paths.push(dir.join("f\0nul"));
        let refs: Vec<&std::path::Path> = paths.iter().map(std::path::PathBuf::as_path).collect();
        let stamps = ring.stamps(&refs);
        let expected: Vec<Stamp> = paths.iter().map(|path| Stamp::of(path)).collect();
        assert_eq!(stamps, expected);
        assert_eq!(stamps[0], Stamp::Missing);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
