//! A positive control for pressure experiments.
//!
//! The file-cache moat (README, "Pressure findings") means "0 pages
//! lost" is meaningful only if the pressure demonstrably reached
//! anonymous memory at all — on a large-RAM machine the generator's
//! entire allocation can be absorbed by draining file cache, and every
//! survey then reports a vacuous all-clear. The canary is a dirty
//! buffer marked `MADV_FREE`, the most eagerly discarded class this
//! repo has measured: if pressure discards the canary, it reached the
//! pages under test; if the canary survives, the run proves nothing and
//! the caller must say so instead of printing a verdict.

use crate::PAGE;

const PATTERN: u64 = 0xC0DE_D00D_FEED_FACE;

/// A dirty, `MADV_FREE`-marked buffer whose survival tells whether
/// pressure ever reached anonymous memory.
pub struct Canary {
    ptr: *mut u8,
    size: usize,
}

/// What pressure did to the canary.
pub struct Verdict {
    pub discarded: usize,
    pub total: usize,
}

impl Verdict {
    /// Pressure demonstrably reached anonymous memory: at least half the
    /// canary was discarded.
    pub fn conclusive(&self) -> bool {
        self.discarded * 2 >= self.total
    }
}

impl Canary {
    /// Dirty `gib` GiB of anonymous memory and mark it `MADV_FREE`.
    pub fn arm(gib: usize) -> Self {
        let size = gib << 30;
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                size,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_ANON | libc::MAP_PRIVATE,
                -1,
                0,
            )
        };
        assert_ne!(ptr, libc::MAP_FAILED, "canary mmap failed");
        let ptr: *mut u8 = ptr.cast();
        for off in (0..size).step_by(PAGE) {
            // SAFETY: off < size, mapping is writable.
            unsafe { ptr.add(off).cast::<u64>().write(PATTERN ^ off as u64) };
        }
        let rc = unsafe { libc::madvise(ptr.cast(), size, libc::MADV_FREE) };
        assert_eq!(rc, 0, "canary madvise(MADV_FREE) failed");
        Self { ptr, size }
    }

    /// Count pages whose marker no longer reads back (discarded to zero).
    pub fn survey(&self) -> Verdict {
        let total = self.size / PAGE;
        let mut discarded = 0;
        for off in (0..self.size).step_by(PAGE) {
            // SAFETY: off < size, mapping is readable.
            let v = unsafe { self.ptr.add(off).cast::<u64>().read() };
            if v != PATTERN ^ off as u64 {
                discarded += 1;
            }
        }
        Verdict { discarded, total }
    }
}

impl Drop for Canary {
    fn drop(&mut self) {
        // SAFETY: ptr/size are the mapping created in `arm`.
        unsafe { libc::munmap(self.ptr.cast(), self.size) };
    }
}
