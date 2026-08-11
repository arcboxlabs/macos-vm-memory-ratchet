//! `--soak`: the long-duration, multi-vCPU integrity test.
//!
//! The short probes (`--pressure-check`, `--hammer`) answer "does conclusive
//! pressure discard live guest data?" in about ten seconds on one vCPU. That
//! is enough to characterize the mechanism and not enough to license a
//! production VMM, because the failure class we are trying to exclude is
//! rare and intermittent: Mozilla's macOS zeroing bug needed tens of GB and
//! thousands of threads to show up at all. A ten-second single-vCPU run
//! would miss it.
//!
//! This mode is the same question asked at duty cycle. Guest RAM is split
//! into one slot per vCPU, each slot half `hot` and half `cold`:
//!
//! * **hot** is written by its vCPU forever and must never change value
//!   except by that vCPU's own writes. It is the canary for collateral
//!   damage: reclaiming one region must not perturb another.
//! * **cold** models a range the guest has reported free. The host runs the
//!   reclaim sequence on it *while every other vCPU is executing*, which is
//!   the concurrency a reporting-driven VMM actually has; the guest then
//!   re-touches it and re-verifies. Losing a page here after the guest has
//!   taken it back is the direct analogue of the Mozilla failure.
//!
//! Verification runs **inside the guest** (see `VERIFY_CODE`). Checking from
//! the host would read every page through the host mapping, making it
//! host-referenced — precisely the state that makes the pageout scan spare
//! it. A host-side checker would therefore hide the effect it is looking
//! for.
//!
//! The detector gets a positive control of its own before the run starts:
//! the host corrupts one page and the guest checker must report exactly one
//! mismatch. A soak that cannot see planted damage proves nothing.
//!
//! Output is a page-check count, so the result can be stated as a bound on
//! the defect rate rather than as an unquantified "no loss observed".

use crate::guest::{run_fill_range, run_sweep_range, run_verify_range, RAM_GPA};
use crate::hvf::{
    check, hv_vcpu_create, hv_vcpu_destroy, hv_vcpu_set_reg, hv_vm_map, hv_vm_unmap,
    HvVcpuExitInfo, HV_MEMORY_READ, HV_MEMORY_WRITE, HV_REG_CPSR, PSTATE_EL1H_MASKED,
};
use crate::{Options, ReclaimMode};
use ledger::{mib, Ledger, PAGE};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Sweeps of the hot region between reclaim cycles of the cold region.
const HOT_SWEEPS_PER_CYCLE: u64 = 4;
/// Pressure is applied in bursts: this long on, this long off.
const PRESSURE_ON: Duration = Duration::from_secs(20);
const PRESSURE_OFF: Duration = Duration::from_secs(20);

/// Guest RAM pointer shared with the worker threads.
///
/// SAFETY: every thread touches only its own disjoint slot, except for the
/// one-page corruption in the detector self-test, which runs before the
/// threads start.
#[derive(Clone, Copy)]
struct RamPtr(*mut u8);
// SAFETY: see the type docs — slots are disjoint and never aliased.
unsafe impl Send for RamPtr {}

#[derive(Default)]
struct Stats {
    sweeps: AtomicU64,
    page_checks: AtomicU64,
    mismatches: AtomicU64,
    reclaims: AtomicU64,
    reclaimed_mib: Mutex<Vec<f64>>,
}

pub fn run(ram: *mut u8, opts: &Options) {
    let ram_size = opts.size_gb << 30;
    let vcpus = opts.vcpus.max(1);
    let slot = ram_size / vcpus;
    assert!(
        slot >= 2 * PAGE * 16,
        "--size-gb too small to split across {vcpus} vCPUs"
    );
    let half = (slot / 2) & !(PAGE - 1);
    let deadline = Instant::now() + Duration::from_secs(opts.soak_minutes * 60);

    println!(
        "soak: {} vCPU(s), {} GiB guest RAM, {} min, release = {}\n\
         each vCPU owns {:.0} MiB hot (must never change) + {:.0} MiB cold\n\
         (reclaimed while the other vCPUs run, then re-touched and re-verified)\n",
        vcpus,
        opts.size_gb,
        opts.soak_minutes,
        opts.reclaim_mode.name(),
        mib(half as u64),
        mib(half as u64),
    );

    let stats = Arc::new(Stats::default());
    let stop = Arc::new(AtomicBool::new(false));

    // Pressure burst cycle, so the scan is running for a large fraction of
    // the soak rather than once at the end.
    let pressure = {
        let stop = Arc::clone(&stop);
        let gb = opts.pressure_gb;
        std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                let guard = ledger::pressure::apply(gb);
                let until = Instant::now() + PRESSURE_ON;
                while Instant::now() < until && !stop.load(Ordering::Relaxed) {
                    std::thread::sleep(Duration::from_millis(200));
                }
                drop(guard);
                let until = Instant::now() + PRESSURE_OFF;
                while Instant::now() < until && !stop.load(Ordering::Relaxed) {
                    std::thread::sleep(Duration::from_millis(200));
                }
            }
        })
    };

    let ram = RamPtr(ram);
    let workers: Vec<_> = (0..vcpus)
        .map(|k| {
            let stats = Arc::clone(&stats);
            let mode = opts.reclaim_mode;
            let advice = opts.advice;
            std::thread::spawn(move || worker(k, slot, half, ram, mode, advice, deadline, &stats))
        })
        .collect();

    // Progress line every 30s: enough to see liveness, cheap enough not to
    // perturb anything.
    while Instant::now() < deadline {
        std::thread::sleep(Duration::from_secs(30));
        let l = Ledger::read();
        println!(
            "[{:>5.0}s] sweeps {:>8}  page-checks {:>12}  mismatches {}  reclaims {:>6}  \
             footprint {:>8.1} MiB",
            deadline
                .saturating_duration_since(Instant::now())
                .as_secs_f64()
                .mul_add(-1.0, (opts.soak_minutes * 60) as f64),
            stats.sweeps.load(Ordering::Relaxed),
            stats.page_checks.load(Ordering::Relaxed),
            stats.mismatches.load(Ordering::Relaxed),
            stats.reclaims.load(Ordering::Relaxed),
            mib(l.phys_footprint),
        );
    }

    for w in workers {
        let _ = w.join();
    }
    stop.store(true, Ordering::Relaxed);
    let _ = pressure.join();

    let checks = stats.page_checks.load(Ordering::Relaxed);
    let bad = stats.mismatches.load(Ordering::Relaxed);
    let reclaimed = stats.reclaimed_mib.lock().expect("stats");
    let total_reclaimed: f64 = reclaimed.iter().sum();
    println!(
        "\nsoak complete: {} page-checks, {bad} mismatches, {} reclaim cycles \
         ({:.1} GiB released in total)",
        checks,
        stats.reclaims.load(Ordering::Relaxed),
        total_reclaimed / 1024.0,
    );
    if bad == 0 {
        // A negative result is only as strong as its denominator; state it.
        println!(
            "no corruption in {checks} checked pages. At 95% confidence that\n\
             bounds the per-page-check defect rate below {:.2e} (rule of three).",
            3.0 / checks as f64,
        );
    } else {
        println!("CORRUPTION DETECTED — {bad} page-checks failed. Investigate before trusting this sequence.");
    }
}

#[allow(clippy::too_many_arguments)]
fn worker(
    k: usize,
    slot: usize,
    half: usize,
    ram: RamPtr,
    mode: ReclaimMode,
    advice: i32,
    deadline: Instant,
    stats: &Stats,
) {
    // Each vCPU must be created by the thread that runs it.
    let mut vcpu: u64 = 0;
    let mut exit: *const HvVcpuExitInfo = std::ptr::null();
    check(
        unsafe { hv_vcpu_create(&raw mut vcpu, &raw mut exit, std::ptr::null_mut()) },
        "hv_vcpu_create",
    );
    check(
        unsafe { hv_vcpu_set_reg(vcpu, HV_REG_CPSR, PSTATE_EL1H_MASKED) },
        "set CPSR",
    );

    let hot = (k * slot, k * slot + half);
    let cold = (k * slot + half, k * slot + 2 * half);
    let pages_per_half = (half / PAGE) as u64;

    // Bring both halves under the guest and establish the invariant.
    let mut hot_sweeps = 0u64;
    let mut cycle = 0u64;
    run_sweep_range(vcpu, exit, hot.0, hot.1);
    hot_sweeps += 1;

    if k == 0 {
        detector_self_test(vcpu, exit, ram, hot, hot_sweeps);
    }

    while Instant::now() < deadline {
        for _ in 0..HOT_SWEEPS_PER_CYCLE {
            run_sweep_range(vcpu, exit, hot.0, hot.1);
            hot_sweeps += 1;
            stats.sweeps.fetch_add(1, Ordering::Relaxed);
            let bad = run_verify_range(vcpu, exit, hot.0, hot.1, hot_sweeps);
            stats
                .page_checks
                .fetch_add(pages_per_half, Ordering::Relaxed);
            stats.mismatches.fetch_add(bad, Ordering::Relaxed);
        }

        // The cold half: touch it, hand it back, take it again, verify.
        // The stamp rotates so a page left over from the previous cycle is
        // as much a failure as a zeroed one.
        cycle += 1;
        let stamp = (k as u64) << 32 | cycle;
        run_sweep_range(vcpu, exit, cold.0, cold.1);
        stats.sweeps.fetch_add(1, Ordering::Relaxed);

        let before = Ledger::read();
        reclaim(ram, cold, mode, advice);
        let after = Ledger::read();
        stats.reclaims.fetch_add(1, Ordering::Relaxed);
        stats
            .reclaimed_mib
            .lock()
            .expect("stats")
            .push(ledger::delta_mib(
                before.phys_footprint,
                after.phys_footprint,
            ));

        // Take the range back and put a known value in it. Reclaimed pages
        // are NOT returned zeroed, so the guest must write the state it
        // then checks for.
        run_fill_range(vcpu, exit, cold.0, cold.1, stamp);
        let bad = run_verify_range(vcpu, exit, cold.0, cold.1, stamp);
        stats
            .page_checks
            .fetch_add(pages_per_half, Ordering::Relaxed);
        stats.mismatches.fetch_add(bad, Ordering::Relaxed);

        // ... and it must still be there after the scan has had a go at it.
        // This is the Mozilla failure mode: zeroed *after* the application
        // took the memory back.
        std::thread::sleep(Duration::from_millis(250));
        let bad = run_verify_range(vcpu, exit, cold.0, cold.1, stamp);
        stats
            .page_checks
            .fetch_add(pages_per_half, Ordering::Relaxed);
        stats.mismatches.fetch_add(bad, Ordering::Relaxed);
    }

    check(unsafe { hv_vcpu_destroy(vcpu) }, "hv_vcpu_destroy");
}

/// Plant one corrupt page and require the guest checker to find it. Without
/// this the soak's negative result is indistinguishable from a checker that
/// never worked.
fn detector_self_test(
    vcpu: u64,
    exit: *const HvVcpuExitInfo,
    ram: RamPtr,
    hot: (usize, usize),
    expected: u64,
) {
    let victim = hot.0 + PAGE; // not the first page, to catch off-by-one starts
                               // SAFETY: victim is inside this worker's own hot half.
    let saved = unsafe { ram.0.add(victim).cast::<u64>().read() };
    // SAFETY: same page; restored immediately below.
    unsafe { ram.0.add(victim).cast::<u64>().write(saved ^ 0xDEAD_BEEF) };
    let found = run_verify_range(vcpu, exit, hot.0, hot.1, expected);
    // SAFETY: restore the value the guest expects.
    unsafe { ram.0.add(victim).cast::<u64>().write(saved) };
    assert_eq!(
        found, 1,
        "integrity detector self-test failed: planted 1 corrupt page, checker reported {found}"
    );
    let clean = run_verify_range(vcpu, exit, hot.0, hot.1, expected);
    assert_eq!(
        clean, 0,
        "detector reports {clean} mismatches on clean memory"
    );
    println!("detector self-test: planted corruption found, clean memory clean\n");
}

fn reclaim(ram: RamPtr, range: (usize, usize), mode: ReclaimMode, advice: i32) {
    let len = range.1 - range.0;
    let gpa = RAM_GPA + range.0 as u64;
    // SAFETY: range is inside the guest RAM mapping.
    let host = unsafe { ram.0.add(range.0) };

    check(unsafe { hv_vm_unmap(gpa, len) }, "hv_vm_unmap(soak)");
    match mode {
        ReclaimMode::Reusable => {
            let rc = unsafe { libc::madvise(host.cast(), len, advice) };
            assert_eq!(rc, 0, "madvise: {}", std::io::Error::last_os_error());
        }
        ReclaimMode::Munmap => {
            let rc = unsafe { libc::munmap(host.cast(), len) };
            assert_eq!(rc, 0, "munmap: {}", std::io::Error::last_os_error());
            let p = unsafe {
                libc::mmap(
                    host.cast(),
                    len,
                    libc::PROT_READ | libc::PROT_WRITE,
                    libc::MAP_ANON | libc::MAP_PRIVATE | libc::MAP_FIXED,
                    -1,
                    0,
                )
            };
            assert_eq!(p, host.cast(), "mmap(MAP_FIXED) moved or failed");
        }
    }
    check(
        unsafe { hv_vm_map(host, gpa, len, HV_MEMORY_READ | HV_MEMORY_WRITE) },
        "hv_vm_map(soak)",
    );
}
