//! `--fleet-sim`: the workload-shaped run (E3, HVF arm).
//!
//! The mirror image of vz-ratchet's `--fleet`, on the backend where
//! reclaim works. Guest RAM is divided into K per-service regions; a
//! service "starting" is the guest dirtying its region on the vCPU, a
//! service "stopping" is the host answering with the reclaim triple over
//! that region — the sequence a free-page-reporting-driven VMM would run
//! when the guest reports the freed range. Same trace format as the VZ
//! arm, sampled from this process's own ledger:
//!
//! ```text
//! SAMPLE,<t_s>,<footprint_mib>,<resident_mib>
//! MARK,<t_s>,<event>
//! ```
//!
//! Expectation: the footprint stair-steps down as services stop, tracking
//! the fleet instead of its high-water mark.

use crate::guest::{run_touch_range, RAM_GPA};
use crate::hvf::{check, hv_vm_map, hv_vm_unmap, HvVcpuExitInfo, HV_MEMORY_READ, HV_MEMORY_WRITE};
use crate::Options;
use ledger::Ledger;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

const STAGGER: Duration = Duration::from_secs(2);
const IDLE: Duration = Duration::from_secs(10);
const HOLD: Duration = Duration::from_secs(30);
const TAIL: Duration = Duration::from_secs(30);

fn mark(t0: Instant, event: &str) {
    println!("MARK,{:.1},{event}", t0.elapsed().as_secs_f64());
}

pub fn run(vcpu: u64, exit: *const HvVcpuExitInfo, ram: *mut u8, opts: &Options) {
    let region = opts.service_mib << 20;
    let services = opts.services;
    assert!(
        region * services <= opts.size_gb << 30,
        "fleet must fit in guest RAM (raise --size-gb)"
    );

    let t0 = Instant::now();
    let stop = Arc::new(AtomicBool::new(false));
    let sampler = {
        let stop = Arc::clone(&stop);
        std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                let l = Ledger::read();
                println!(
                    "SAMPLE,{:.1},{:.1},{:.1}",
                    t0.elapsed().as_secs_f64(),
                    ledger::mib(l.phys_footprint),
                    ledger::mib(l.resident),
                );
                std::thread::sleep(Duration::from_secs(1));
            }
        })
    };

    mark(t0, "boot");
    std::thread::sleep(IDLE);

    for i in 0..services {
        run_touch_range(vcpu, exit, i * region, (i + 1) * region);
        mark(t0, &format!("service-start-{}", i + 1));
        std::thread::sleep(STAGGER);
    }

    mark(t0, "hold");
    std::thread::sleep(HOLD);

    for i in 0..services {
        let off = i * region;
        // SAFETY: off + region <= ram_size; `ram` is the guest mapping.
        let host = unsafe { ram.add(off) };
        let gpa = RAM_GPA + off as u64;
        check(unsafe { hv_vm_unmap(gpa, region) }, "hv_vm_unmap(service)");
        let rc = unsafe { libc::madvise(host.cast(), region, libc::MADV_FREE_REUSABLE) };
        assert_eq!(rc, 0, "madvise: {}", std::io::Error::last_os_error());
        check(
            unsafe { hv_vm_map(host, gpa, region, HV_MEMORY_READ | HV_MEMORY_WRITE) },
            "hv_vm_map(service)",
        );
        mark(t0, &format!("service-stop-{}", i + 1));
        std::thread::sleep(STAGGER);
    }

    mark(t0, "tail");
    std::thread::sleep(TAIL);

    mark(t0, "end");
    stop.store(true, Ordering::Relaxed);
    let _ = sampler.join();
}
