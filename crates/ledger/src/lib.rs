//! Self-process memory ledger readings via `task_info(TASK_VM_INFO)`.
//!
//! `phys_footprint` is the number macOS itself uses for memory-pressure
//! decisions (it is what Activity Monitor's "Memory" column approximates,
//! and what `footprint(1)` reports). Everything in this repo is judged
//! against it.

/// Apple Silicon host page size. Every touch/survey stride in this repo
/// uses it; [`assert_host_page_size`] pins it at startup.
pub const PAGE: usize = 16 * 1024;

/// `task_vm_info` up to and including `phys_footprint` (rev1).
/// Layout from `<mach/task_info.h>`; the kernel copies out at most the
/// count we pass, so truncating after `phys_footprint` is safe.
#[repr(C)]
#[derive(Default, Clone, Copy)]
struct TaskVmInfo {
    virtual_size: u64,
    region_count: i32,
    page_size: i32,
    resident_size: u64,
    resident_size_peak: u64,
    device: u64,
    device_peak: u64,
    internal: u64,
    internal_peak: u64,
    external: u64,
    external_peak: u64,
    reusable: u64,
    reusable_peak: u64,
    purgeable_volatile_pmap: u64,
    purgeable_volatile_resident: u64,
    purgeable_volatile_virtual: u64,
    compressed: u64,
    compressed_peak: u64,
    compressed_lifetime: u64,
    phys_footprint: u64,
}

const TASK_VM_INFO: u32 = 22;

unsafe extern "C" {
    fn task_info(task: u32, flavor: u32, info: *mut i32, count: *mut u32) -> i32;
    static mach_task_self_: u32;
}

/// One reading of the ledger counters this repo cares about, in bytes.
#[derive(Clone, Copy, Debug)]
pub struct Ledger {
    /// The memory-pressure ledger. The one that matters.
    pub phys_footprint: u64,
    /// Resident pages (includes pages that are clean/reclaimable).
    pub resident: u64,
    /// Pages marked reusable (`MADV_FREE_REUSABLE`) — resident but already
    /// off the footprint ledger and reclaimable without I/O.
    pub reusable: u64,
    /// Bytes held by the compressor on this task's behalf.
    pub compressed: u64,
}

impl Ledger {
    /// Read the current task's ledger. Panics on Mach errors — this is a
    /// measurement tool; a failed reading must never be silently zero.
    pub fn read() -> Self {
        let mut info = TaskVmInfo::default();
        let full = (size_of::<TaskVmInfo>() / size_of::<u32>()) as u32;
        let mut count = full;
        let kr = unsafe {
            task_info(
                mach_task_self_,
                TASK_VM_INFO,
                (&raw mut info).cast::<i32>(),
                &raw mut count,
            )
        };
        assert_eq!(kr, 0, "task_info(TASK_VM_INFO) failed: {kr}");
        // The kernel writes back how many words it copied; anything short
        // of the full struct would leave `phys_footprint` as stale zeros.
        assert_eq!(count, full, "task_info copied {count} of {full} words");
        Self {
            phys_footprint: info.phys_footprint,
            resident: info.resident_size,
            reusable: info.reusable,
            compressed: info.compressed,
        }
    }
}

pub mod canary;
pub mod pressure;

/// Assert the host page size matches [`PAGE`]. On a hypothetical 4 KiB
/// host the 16 KiB strides would silently skip pages; fail loudly instead.
pub fn assert_host_page_size() {
    let sys = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    assert_eq!(sys as usize, PAGE, "host page size {sys} != {PAGE}");
}

/// Bytes → mebibytes, for display.
pub fn mib(bytes: u64) -> f64 {
    bytes as f64 / (1024.0 * 1024.0)
}

/// Signed delta in MiB between two byte counts.
pub fn delta_mib(after: u64, before: u64) -> f64 {
    (after as f64 - before as f64) / (1024.0 * 1024.0)
}

/// Print a labeled ledger row.
pub fn row(label: &str, l: &Ledger) {
    println!(
        "{label:<34} footprint {:>9.1} MiB   resident {:>9.1} MiB   reusable {:>9.1} MiB   compressed {:>9.1} MiB",
        mib(l.phys_footprint),
        mib(l.resident),
        mib(l.reusable),
        mib(l.compressed),
    );
}
