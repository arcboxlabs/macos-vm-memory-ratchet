//! `--fleet`: the workload-shaped run (E3, VZ arm).
//!
//! Instead of one big touch/free, the guest runs a service fleet: K forked
//! processes, each holding its own dirty working set, started 2 s apart,
//! held, then killed one by one — the memory lifecycle of a compose stack
//! coming up and going down. The driver samples the XPC helper at 1 Hz
//! throughout and emits a machine-readable trace:
//!
//! ```text
//! SAMPLE,<t_s>,<footprint_mib>,<resident_mib>
//! MARK,<t_s>,<event>
//! ```
//!
//! On this backend the expectation is the ratchet: the footprint climbs as
//! services start and does not come back down when they stop — not even
//! when the balloon inflates over the freed memory at the end.

use crate::harness::Harness;
use crate::helper;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

const BOOT_TIMEOUT: Duration = Duration::from_secs(60);
const STAGGER: Duration = Duration::from_secs(2);
const IDLE: Duration = Duration::from_secs(10);
const HOLD: Duration = Duration::from_secs(30);
const TAIL: Duration = Duration::from_secs(30);
const BALLOON_WATCH: Duration = Duration::from_secs(30);

pub struct FleetOptions {
    pub services: usize,
    pub service_mib: usize,
    pub guest_gb: usize,
}

fn mark(t0: Instant, event: &str) {
    println!("MARK,{:.1},{event}", t0.elapsed().as_secs_f64());
}

pub fn run(mut vm: Harness, pid: i32, opts: &FleetOptions) {
    let t0 = Instant::now();
    let stop = Arc::new(AtomicBool::new(false));
    let sampler = {
        let stop = Arc::clone(&stop);
        std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                let s = helper::sample(pid);
                println!(
                    "SAMPLE,{:.1},{:.1},{:.1}",
                    t0.elapsed().as_secs_f64(),
                    ledger::mib(s.phys_footprint),
                    ledger::mib(s.resident),
                );
                std::thread::sleep(Duration::from_secs(1));
            }
        })
    };

    mark(t0, "boot");
    std::thread::sleep(IDLE);

    for i in 1..=opts.services {
        vm.send(&format!("guest fleet start 1 {}", opts.service_mib));
        vm.wait("RATCHET FLEET-STARTED", BOOT_TIMEOUT);
        mark(t0, &format!("service-start-{i}"));
        std::thread::sleep(STAGGER);
    }

    mark(t0, "hold");
    std::thread::sleep(HOLD);

    for i in 1..=opts.services {
        vm.send("guest fleet stop-one");
        vm.wait("RATCHET FLEET-STOPPED", BOOT_TIMEOUT);
        mark(t0, &format!("service-stop-{i}"));
        std::thread::sleep(STAGGER);
    }

    mark(t0, "tail");
    std::thread::sleep(TAIL);

    // The balloon over the now-free memory: the strongest thing this
    // backend offers, applied at the most favorable moment.
    let fleet_mib = opts.services * opts.service_mib;
    let target_mib = opts.guest_gb * 1024 - fleet_mib;
    vm.send(&format!("balloon {target_mib}"));
    vm.wait("HARNESS balloon-target", Duration::from_secs(10));
    mark(t0, "balloon");
    std::thread::sleep(BALLOON_WATCH);

    mark(t0, "end");
    stop.store(true, Ordering::Relaxed);
    let _ = sampler.join();
    vm.quit();
}
