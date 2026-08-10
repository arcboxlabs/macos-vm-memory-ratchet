//! `--time-reclaim`: what the reclaim sequence *costs*.
//!
//! The other modes establish that `hv_vm_unmap → madvise(MADV_FREE_REUSABLE)
//! → hv_vm_map` works and is safe. A free-page-reporting-driven VMM would run
//! that triple once per reported extent, so the design question is latency
//! versus extent size: one triple over the whole range, or thousands of small
//! triples as reports arrive. This mode measures each phase separately and
//! emits CSV.
//!
//! Two reclaim modes:
//!
//! * `reusable` — the sequence above (footprint drops, pages stay mapped
//!   host-side, fault-back revives the same objects).
//! * `munmap` — `hv_vm_unmap → munmap → mmap(MAP_FIXED) → hv_vm_map`: tear
//!   the host mapping out entirely and replace it with fresh zero-fill. The
//!   contents-are-garbage semantics are identical for reclaim-after-free;
//!   the costs (map-entry churn, fault-back path) are not, which is what the
//!   `regions` columns record.
//!
//! Two cycle regimes:
//!
//! * default — every cycle gets fresh anonymous memory (like `--repeat`),
//!   so the ledger delta validates each cycle. This is the first-reclaim
//!   cost of a cold object.
//! * `--steady-state` — the same mapping is dirtied and reclaimed over and
//!   over, the regime a reporting loop actually runs in. Under `reusable`
//!   the ledger goes sticky after the first cycle (re-touched reusable pages
//!   stop moving the accounting) — the `settle_mib` column documents that
//!   rather than pretending to validate.
//!
//! Timing uses `std::time::Instant`, which on macOS is
//! `clock_gettime_nsec_np(CLOCK_UPTIME_RAW)` — the `mach_absolute_time`
//! clock. Phase columns are summed nanoseconds across all extents of the
//! cycle.

use crate::guest::{run_touch_pass, RAM_GPA};
use crate::hvf::{check, hv_vm_map, hv_vm_unmap, HvVcpuExitInfo, HV_MEMORY_READ, HV_MEMORY_WRITE};
use crate::{mmap_anon, Options, ReclaimMode};
use ledger::{delta_mib, Ledger, PAGE};
use std::time::{Duration, Instant};

/// One cycle's phase totals, in nanoseconds.
struct Phases {
    unmap: u128,
    release: u128,
    remap: u128,
}

/// Returns the current RAM mapping (cycle resets replace it; the caller
/// still owns the final munmap).
pub fn run(vcpu: u64, exit: *const HvVcpuExitInfo, mut ram: *mut u8, opts: &Options) -> *mut u8 {
    let ram_size = opts.size_gb << 30;
    let extent = match opts.extent_kb {
        0 => ram_size,
        kb => kb * 1024,
    };
    assert!(
        extent.is_multiple_of(PAGE) && ram_size.is_multiple_of(extent),
        "--extent-kb must be a multiple of {} and divide the RAM size",
        PAGE / 1024
    );
    let extents = ram_size / extent;

    println!(
        "# hv-reclaim --time-reclaim: {} GiB, {} extent(s) of {} KiB, mode {}, {}, {} cycle(s)",
        opts.size_gb,
        extents,
        extent / 1024,
        opts.reclaim_mode.name(),
        if opts.steady_state {
            "steady-state (same mapping every cycle)"
        } else {
            "fresh mapping every cycle"
        },
        opts.repeat,
    );
    println!(
        "# phase columns are summed ns across all extents; release = {}",
        match opts.reclaim_mode {
            ReclaimMode::Reusable => "madvise(MADV_FREE_REUSABLE)",
            ReclaimMode::Munmap => "munmap + mmap(MAP_FIXED)",
        }
    );
    println!(
        "mode,size_gb,extent_kb,extents,cycle,touch_ns,unmap_ns,release_ns,remap_ns,\
         faultback_ns,settle_mib,regions_before,regions_after"
    );

    for cycle in 1..=opts.repeat {
        if cycle > 1 && !opts.steady_state {
            check(
                unsafe { hv_vm_unmap(RAM_GPA, ram_size) },
                "hv_vm_unmap (cycle reset)",
            );
            unsafe { libc::munmap(ram.cast(), ram_size) };
            ram = mmap_anon(ram_size);
            check(
                unsafe { hv_vm_map(ram, RAM_GPA, ram_size, HV_MEMORY_READ | HV_MEMORY_WRITE) },
                "hv_vm_map (cycle reset)",
            );
        }

        let t = Instant::now();
        run_touch_pass(vcpu, exit, ram_size);
        let touch_ns = t.elapsed().as_nanos();

        let before = Ledger::read();
        let phases = reclaim_extents(ram, ram_size, extent, opts.reclaim_mode, opts.advice);
        std::thread::sleep(Duration::from_millis(200));
        let after = Ledger::read();

        // Fault everything back in from the guest: the per-GiB revival cost
        // of whichever backing state the mode left behind.
        let t = Instant::now();
        run_touch_pass(vcpu, exit, ram_size);
        let faultback_ns = t.elapsed().as_nanos();

        println!(
            "{},{},{},{},{},{},{},{},{},{},{:.1},{},{}",
            opts.reclaim_mode.name(),
            opts.size_gb,
            extent / 1024,
            extents,
            cycle,
            touch_ns,
            phases.unmap,
            phases.release,
            phases.remap,
            faultback_ns,
            delta_mib(after.phys_footprint, before.phys_footprint),
            before.regions,
            after.regions,
        );
    }
    ram
}

/// Run the per-extent reclaim triple over the whole range, timing each
/// phase. The triple runs extent-at-a-time — the shape a reporting-driven
/// reclaim path has, since extents arrive one report at a time.
fn reclaim_extents(
    ram: *mut u8,
    ram_size: usize,
    extent: usize,
    mode: ReclaimMode,
    advice: i32,
) -> Phases {
    let mut p = Phases {
        unmap: 0,
        release: 0,
        remap: 0,
    };
    for off in (0..ram_size).step_by(extent) {
        // SAFETY: off + extent <= ram_size; `ram` is a valid mapping.
        let host = unsafe { ram.add(off) };
        let gpa = RAM_GPA + off as u64;

        let t = Instant::now();
        check(unsafe { hv_vm_unmap(gpa, extent) }, "hv_vm_unmap(extent)");
        p.unmap += t.elapsed().as_nanos();

        let t = Instant::now();
        match mode {
            ReclaimMode::Reusable => {
                let rc = unsafe { libc::madvise(host.cast(), extent, advice) };
                assert_eq!(rc, 0, "madvise: {}", std::io::Error::last_os_error());
            }
            ReclaimMode::Munmap => {
                let rc = unsafe { libc::munmap(host.cast(), extent) };
                assert_eq!(rc, 0, "munmap: {}", std::io::Error::last_os_error());
                let p2 = unsafe {
                    libc::mmap(
                        host.cast(),
                        extent,
                        libc::PROT_READ | libc::PROT_WRITE,
                        libc::MAP_ANON | libc::MAP_PRIVATE | libc::MAP_FIXED,
                        -1,
                        0,
                    )
                };
                assert_eq!(p2, host.cast(), "mmap(MAP_FIXED) moved or failed");
            }
        }
        p.release += t.elapsed().as_nanos();

        let t = Instant::now();
        check(
            unsafe { hv_vm_map(host, gpa, extent, HV_MEMORY_READ | HV_MEMORY_WRITE) },
            "hv_vm_map(extent)",
        );
        p.remap += t.elapsed().as_nanos();
    }
    p
}
