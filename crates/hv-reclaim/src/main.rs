//! The counter-demonstration: a real VM on Hypervisor.framework whose
//! host process *does* give guest memory back to macOS — plus the trap
//! we found on the way.
//!
//! A minimal aarch64 guest (no kernel) dirties every host page of its
//! RAM on a real vCPU, then rings an MMIO doorbell — standing in for a
//! guest's free-page report ("I'm done with this range"). The host
//! answers with the sequence a production VMM would use:
//!
//! ```text
//! hv_vm_unmap(range) → madvise(MADV_FREE_REUSABLE) → hv_vm_map(range)
//! ```
//!
//! `phys_footprint` drops by the full guest RAM size, the VM stays
//! alive, and a second guest pass faults the pages back in on demand.
//! This is the reclaim Virtualization.framework cannot express — same
//! Mac, same kernel, one API layer down.
//!
//! The trap (`--naive`): calling `madvise` while the range is still
//! stage-2 mapped **silently does nothing** for pages the *guest*
//! dirtied — `madvise` returns 0 and the ledger doesn't move. Pages the
//! *host* dirtied (`--host-touch`) reclaim fine either way, which is
//! exactly the difference a host-only calibration would miss. The dirty
//! state guest writes leave in the stage-2 pmap pins the page until the
//! stage-2 mapping is torn down.
//!
//! Advice comparison (all with the unmap→advise→remap sequence):
//! `--advice reusable` (default) drops the footprint; `free` and
//! `dontneed` leave it flat — see `calibrate-madvise`.
//!
//! Safety probes (both use a 1 GiB `MADV_FREE` canary as the positive
//! control — if pressure never discards the canary, the run is reported
//! INCONCLUSIVE instead of as a vacuous all-clear):
//!
//! * `--pressure-check` — after the guest re-touches its reclaimed RAM,
//!   apply real pressure with the guest *parked* and verify its data.
//! * `--hammer` — the stronger form: the guest keeps read-modify-writing
//!   every page while pressure builds, holds, and releases, so guest
//!   stores race the pageout scan itself. Any page discarded at any
//!   moment ends the run with a counter that can't have caught up.
//!
//! Build & run (the hypervisor entitlement accepts ad-hoc signing):
//!
//! ```sh
//! ./run.sh                # the fix
//! ./run.sh --naive        # the trap
//! ./run.sh --repeat 5     # variance of the reclaim cycle
//! ```

mod fleet_sim;
mod guest;
mod hvf;
mod soak;
mod timing;

use guest::{run_hammer, run_read_pass, run_touch_pass, write_code_page, CODE_GPA, RAM_GPA};
use hvf::{
    check, hv_vcpu_create, hv_vcpu_destroy, hv_vcpu_set_reg, hv_vm_create, hv_vm_destroy,
    hv_vm_map, hv_vm_unmap, HvVcpuExitInfo, HV_MEMORY_EXEC, HV_MEMORY_READ, HV_MEMORY_WRITE,
    HV_REG_CPSR, PSTATE_EL1H_MASKED,
};
use ledger::canary::Canary;
use ledger::pressure::PressureGuard;
use ledger::{delta_mib, row, Ledger, PAGE};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// How long pressure is held once the generator reports HOLDING.
const HOLD: Duration = Duration::from_secs(8);
/// Post-release tail during which the hammer keeps sweeping.
const TAIL: Duration = Duration::from_secs(2);

/// How `--time-reclaim` gives an extent back to the OS (see `timing`).
#[derive(Clone, Copy, PartialEq)]
enum ReclaimMode {
    Reusable,
    Munmap,
}

impl ReclaimMode {
    fn name(self) -> &'static str {
        match self {
            Self::Reusable => "reusable",
            Self::Munmap => "munmap",
        }
    }
}

struct Options {
    advice: i32,
    advice_name: &'static str,
    size_gb: usize,
    /// madvise while the range is still stage-2 mapped (the trap),
    /// instead of the unmap → advise → remap sequence (the fix).
    naive: bool,
    /// Dirty the RAM from the host instead of running the guest — the
    /// control that shows why host-only calibration misses the trap.
    host_touch: bool,
    /// Have the guest only READ its RAM (no stores). Separates "the guest
    /// dirtied the page" from "the guest touched it at all": if a
    /// read-only pass pins the ledger too, the trap is about mapping and
    /// wiring, not about dirty state.
    guest_read: bool,
    /// Parked-guest safety probe (see module docs).
    pressure_check: bool,
    /// Concurrent-write safety probe (see module docs).
    hammer: bool,
    /// GiB of dirty memory the pressure generator holds (--pressure-gb).
    pressure_gb: usize,
    /// Repetitions of the dirty→reclaim cycle (--repeat).
    repeat: usize,
    /// Per-phase cost measurement with CSV output (--time-reclaim).
    time_reclaim: bool,
    /// Extent size for the timing matrix, KiB; 0 = the whole range as one
    /// extent (--extent-kb).
    extent_kb: usize,
    /// Which release step the timing triple uses (--reclaim-mode).
    reclaim_mode: ReclaimMode,
    /// Reuse the same mapping across timing cycles instead of resetting to
    /// fresh memory (--steady-state).
    steady_state: bool,
    /// The workload-shaped E3 run (--fleet-sim): K per-service regions,
    /// dirtied on start, reclaimed on stop, 1 Hz trace output.
    fleet_sim: bool,
    services: usize,
    service_mib: usize,
    /// Long-duration multi-vCPU integrity soak (--soak).
    soak: bool,
    soak_minutes: u64,
    vcpus: usize,
}

fn parse_args() -> Options {
    let mut opts = Options {
        advice: libc::MADV_FREE_REUSABLE,
        advice_name: "MADV_FREE_REUSABLE",
        size_gb: 3,
        naive: false,
        host_touch: false,
        guest_read: false,
        pressure_check: false,
        hammer: false,
        pressure_gb: 48,
        repeat: 1,
        time_reclaim: false,
        extent_kb: 0,
        reclaim_mode: ReclaimMode::Reusable,
        steady_state: false,
        fleet_sim: false,
        services: 8,
        service_mib: 256,
        soak: false,
        soak_minutes: 60,
        vcpus: 4,
    };
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--advice" => {
                (opts.advice, opts.advice_name) = match args.next().as_deref() {
                    Some("reusable") => (libc::MADV_FREE_REUSABLE, "MADV_FREE_REUSABLE"),
                    Some("free") => (libc::MADV_FREE, "MADV_FREE"),
                    Some("dontneed") => (libc::MADV_DONTNEED, "MADV_DONTNEED"),
                    other => panic!("unknown advice {other:?} (reusable|free|dontneed)"),
                }
            }
            "--size-gb" => {
                opts.size_gb = args
                    .next()
                    .expect("--size-gb N")
                    .parse()
                    .expect("integer GiB");
            }
            "--naive" => opts.naive = true,
            "--host-touch" => opts.host_touch = true,
            "--guest-read" => opts.guest_read = true,
            "--pressure-check" => opts.pressure_check = true,
            "--hammer" => opts.hammer = true,
            "--pressure-gb" => {
                opts.pressure_gb = args
                    .next()
                    .expect("--pressure-gb N")
                    .parse()
                    .expect("integer GiB");
            }
            "--repeat" => {
                opts.repeat = args.next().expect("--repeat N").parse().expect("integer");
                assert!(opts.repeat >= 1, "--repeat must be >= 1");
            }
            "--time-reclaim" => opts.time_reclaim = true,
            "--extent-kb" => {
                opts.extent_kb = args
                    .next()
                    .expect("--extent-kb N")
                    .parse()
                    .expect("integer KiB");
            }
            "--reclaim-mode" => {
                opts.reclaim_mode = match args.next().as_deref() {
                    Some("reusable") => ReclaimMode::Reusable,
                    Some("munmap") => ReclaimMode::Munmap,
                    other => panic!("unknown reclaim mode {other:?} (reusable|munmap)"),
                }
            }
            "--steady-state" => opts.steady_state = true,
            "--fleet-sim" => opts.fleet_sim = true,
            "--soak" => opts.soak = true,
            "--soak-minutes" => {
                opts.soak_minutes = args
                    .next()
                    .expect("--soak-minutes N")
                    .parse()
                    .expect("integer minutes");
            }
            "--vcpus" => {
                opts.vcpus = args.next().expect("--vcpus N").parse().expect("integer");
                assert!(opts.vcpus >= 1, "--vcpus must be >= 1");
            }
            "--services" => {
                opts.services = args
                    .next()
                    .expect("--services N")
                    .parse()
                    .expect("integer count");
            }
            "--service-mib" => {
                opts.service_mib = args
                    .next()
                    .expect("--service-mib N")
                    .parse()
                    .expect("integer MiB");
            }
            other => panic!("unknown argument {other:?}"),
        }
    }
    if opts.hammer {
        assert!(
            !opts.host_touch && !opts.naive && !opts.pressure_check,
            "--hammer probes the post-reclaim guest state; it excludes \
             --host-touch, --naive and --pressure-check"
        );
    }
    if opts.repeat > 1 {
        assert!(
            !opts.pressure_check && !opts.hammer,
            "--repeat measures the plain reclaim cycle; run the safety \
             probes separately"
        );
    }
    let exclusive =
        usize::from(opts.time_reclaim) + usize::from(opts.fleet_sim) + usize::from(opts.soak);
    if exclusive > 0 {
        assert!(
            exclusive == 1,
            "--time-reclaim, --fleet-sim and --soak are separate modes"
        );
        assert!(
            !opts.naive && !opts.host_touch && !opts.pressure_check && !opts.hammer,
            "--time-reclaim/--fleet-sim/--soak drive the working sequence; \
             they exclude --naive, --host-touch and the short safety probes"
        );
    } else {
        assert!(
            opts.extent_kb == 0 && !opts.steady_state && opts.reclaim_mode == ReclaimMode::Reusable,
            "--extent-kb, --steady-state and --reclaim-mode only apply to \
             --time-reclaim and --soak"
        );
    }
    opts
}

/// The reclaim answer to a doorbell: unmap → advise → remap (or the
/// naive in-place madvise when `--naive`).
fn reclaim(ram: *mut u8, ram_size: usize, opts: &Options) {
    if !opts.naive {
        check(unsafe { hv_vm_unmap(RAM_GPA, ram_size) }, "hv_vm_unmap");
    }
    let rc = unsafe { libc::madvise(ram.cast(), ram_size, opts.advice) };
    assert_eq!(
        rc,
        0,
        "madvise({}) failed: {}",
        opts.advice_name,
        std::io::Error::last_os_error()
    );
    if !opts.naive {
        check(
            unsafe { hv_vm_map(ram, RAM_GPA, ram_size, HV_MEMORY_READ | HV_MEMORY_WRITE) },
            "hv_vm_map remap",
        );
    }
}

fn print_canary(v: &ledger::canary::Verdict) {
    println!(
        "canary (1 GiB MADV_FREE control): {}/{} pages discarded — {}",
        v.discarded,
        v.total,
        if v.conclusive() {
            "pressure reached anonymous memory; the run is conclusive"
        } else {
            "pressure NEVER reached anonymous memory; the run is INCONCLUSIVE \
             (raise --pressure-gb)"
        }
    );
}

fn main() {
    ledger::pressure::maybe_run_generator();
    ledger::assert_host_page_size();

    let opts = parse_args();
    let ram_size = opts.size_gb << 30;
    let pages = ram_size / PAGE;

    if !opts.time_reclaim && !opts.fleet_sim {
        println!(
            "hv-reclaim: {} GiB guest RAM in-process, dirtied by {},\n\
             reclaimed with {}\n",
            opts.size_gb,
            if opts.host_touch {
                "the HOST (control)"
            } else {
                "the guest on a real vCPU"
            },
            if opts.naive {
                format!("{} while still stage-2 mapped (--naive)", opts.advice_name)
            } else {
                format!(
                    "hv_vm_unmap \u{2192} {} \u{2192} hv_vm_map",
                    opts.advice_name
                )
            },
        );

        row("baseline", &Ledger::read());
    }

    // Guest RAM and code are plain anonymous memory of this process.
    let mut ram = mmap_anon(ram_size);
    let code = mmap_anon(PAGE);
    write_code_page(code);

    check(
        unsafe { hv_vm_create(std::ptr::null_mut()) },
        "hv_vm_create",
    );
    check(
        unsafe { hv_vm_map(code, CODE_GPA, PAGE, HV_MEMORY_READ | HV_MEMORY_EXEC) },
        "hv_vm_map(code)",
    );
    check(
        unsafe { hv_vm_map(ram, RAM_GPA, ram_size, HV_MEMORY_READ | HV_MEMORY_WRITE) },
        "hv_vm_map(ram)",
    );

    // The soak owns its own vCPUs: each must be created by the thread that
    // runs it, so it dispatches before the single-vCPU path below.
    if opts.soak {
        soak::run(ram, &opts);
        check(unsafe { hv_vm_destroy() }, "hv_vm_destroy");
        unsafe {
            libc::munmap(ram.cast(), ram_size);
            libc::munmap(code.cast(), PAGE);
        }
        return;
    }

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

    if opts.time_reclaim || opts.fleet_sim {
        if opts.time_reclaim {
            ram = timing::run(vcpu, exit, ram, &opts);
        } else {
            fleet_sim::run(vcpu, exit, ram, &opts);
        }
        check(unsafe { hv_vcpu_destroy(vcpu) }, "hv_vcpu_destroy");
        check(unsafe { hv_vm_destroy() }, "hv_vm_destroy");
        unsafe {
            libc::munmap(ram.cast(), ram_size);
            libc::munmap(code.cast(), PAGE);
        }
        return;
    }

    // Dirty → reclaim, `--repeat` times. Each cycle gets FRESH anonymous
    // memory: re-touching reclaimed pages leaves them in the sticky
    // reusable accounting state where neither the touch nor the next
    // madvise moves the ledger (the re-touch line of a plain run shows
    // that regime), so without the reset every cycle after the first
    // would measure ±0 and the variance would be fiction.
    let mut reclaim_deltas = Vec::new();
    let mut last = Ledger::read();
    for cycle in 1..=opts.repeat {
        if cycle > 1 {
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
            last = Ledger::read();
        }
        let before = last;
        let t0 = Instant::now();
        if opts.host_touch {
            for off in (0..ram_size).step_by(PAGE) {
                // SAFETY: off < ram_size, mapping is writable.
                unsafe { ram.add(off).cast::<u64>().write(off as u64) };
            }
        } else if opts.guest_read {
            run_read_pass(vcpu, exit, ram_size);
        } else {
            run_touch_pass(vcpu, exit, ram_size);
        }
        let touch_secs = t0.elapsed().as_secs_f64();

        let dirtied = Ledger::read();
        reclaim(ram, ram_size, &opts);
        std::thread::sleep(Duration::from_millis(200));
        let reclaimed = Ledger::read();

        let dt = delta_mib(dirtied.phys_footprint, before.phys_footprint);
        let dr = delta_mib(reclaimed.phys_footprint, dirtied.phys_footprint);
        if opts.repeat == 1 {
            println!(
                "\ndirtied {} GiB in {touch_secs:.2}s — {:.0}k page faults/s \
                 (one 8-byte store per 16 KiB page; a fault-rate figure, not bandwidth)\n",
                opts.size_gb,
                pages as f64 / touch_secs / 1000.0,
            );
            row("after RAM dirtied", &dirtied);
            row("after reclaim", &reclaimed);
            println!("\nΔfootprint touch: {dt:+.1} MiB, reclaim: {dr:+.1} MiB");
        } else {
            println!("cycle {cycle:>2}: touch {dt:+8.1} MiB, reclaim {dr:+8.1} MiB");
        }
        reclaim_deltas.push(dr);
        last = reclaimed;
    }
    if opts.repeat > 1 {
        let n = reclaim_deltas.len() as f64;
        let mean = reclaim_deltas.iter().sum::<f64>() / n;
        let min = reclaim_deltas.iter().cloned().fold(f64::INFINITY, f64::min);
        let max = reclaim_deltas
            .iter()
            .cloned()
            .fold(f64::NEG_INFINITY, f64::max);
        println!(
            "\nreclaim Δfootprint over {} cycles: mean {mean:+.1} MiB, \
             min {min:+.1} MiB, max {max:+.1} MiB",
            opts.repeat
        );
    }

    // Pass 2 (guest mode only): the VM is still alive — prove it by
    // letting the guest fault everything back in on demand.
    if !opts.host_touch && opts.repeat == 1 {
        let reclaimed = last;
        run_touch_pass(vcpu, exit, ram_size);
        let retouched = Ledger::read();
        println!();
        row("after guest re-touches its RAM", &retouched);
        println!(
            "Δfootprint re-touch: {:+.1} MiB (paid again only when actually used)",
            delta_mib(retouched.phys_footprint, reclaimed.phys_footprint),
        );

        if opts.pressure_check {
            parked_pressure_check(ram, ram_size, &opts);
        }
        if opts.hammer {
            hammer_check(vcpu, exit, ram, ram_size, &opts);
        }
    }

    println!(
        "\nGuest RAM here is ordinary memory of this process; the stage-2 mapping\n\
         is torn down and rebuilt around the madvise. Nothing about running a VM\n\
         prevents giving memory back — only which process owns the pages, and\n\
         whether the reclaim sequence is the correct one."
    );

    check(unsafe { hv_vcpu_destroy(vcpu) }, "hv_vcpu_destroy");
    check(unsafe { hv_vm_destroy() }, "hv_vm_destroy");
    unsafe {
        libc::munmap(ram.cast(), ram_size);
        libc::munmap(code.cast(), PAGE);
    }
}

/// The rogue-page question, parked form: the guest has live data in pages
/// the ledger may still consider reusable. Does real pressure discard
/// them (data loss), or does the stage-2 dirty state protect them?
fn parked_pressure_check(ram: *mut u8, ram_size: usize, opts: &Options) {
    let canary = Canary::arm(1);
    println!(
        "\ngenerating pressure: dirtying {} GiB in a child, holding {}s ...",
        opts.pressure_gb,
        HOLD.as_secs()
    );
    let guard = ledger::pressure::apply(opts.pressure_gb).expect("pressure generator");
    std::thread::sleep(HOLD);
    drop(guard);
    std::thread::sleep(TAIL);

    // Read the ledger BEFORE the surveys: they read through the host
    // mapping and perturb the per-entry counters.
    let after_pressure = Ledger::read();
    println!();
    row("after real memory pressure", &after_pressure);
    print_canary(&canary.survey());

    // The guest wrote its own GPA into the first word of every page.
    // Verify host-side through our mapping of guest RAM.
    let (mut intact, mut lost) = (0usize, 0usize);
    for off in (0..ram_size).step_by(PAGE) {
        // SAFETY: off < ram_size, mapping is readable.
        let v = unsafe { ram.add(off).cast::<u64>().read() };
        if v == RAM_GPA + off as u64 {
            intact += 1;
        } else {
            lost += 1;
        }
    }
    println!(
        "guest data: {intact}/{} pages intact, {lost} lost",
        ram_size / PAGE
    );
}

/// The rogue-page question, concurrent form: guest stores race the
/// pageout scan itself. Sweeps run while the generator dirties its way
/// up (the scan-heavy window), through the hold, and for a short tail
/// after release.
fn hammer_check(
    vcpu: u64,
    exit: *const HvVcpuExitInfo,
    ram: *mut u8,
    ram_size: usize,
    opts: &Options,
) {
    println!(
        "\nhammer: guest RMW-increments every page continuously while {} GiB\n\
         of pressure builds, holds {}s, and releases (+{}s tail) ...",
        opts.pressure_gb,
        HOLD.as_secs(),
        TAIL.as_secs()
    );
    let canary = Canary::arm(1);

    let (tx, rx) = mpsc::channel();
    let gb = opts.pressure_gb;
    std::thread::spawn(move || {
        let _ = tx.send(ledger::pressure::apply(gb));
    });

    let mut guard: Option<PressureGuard> = None;
    let mut hold_since: Option<Instant> = None;
    let mut released_at: Option<Instant> = None;
    let t0 = Instant::now();
    let sweeps = run_hammer(vcpu, exit, ram_size, |_| {
        if guard.is_none() && released_at.is_none() {
            match rx.try_recv() {
                Ok(Ok(g)) => {
                    guard = Some(g);
                    hold_since = Some(Instant::now());
                }
                Ok(Err(e)) => panic!("pressure generator: {e}"),
                Err(mpsc::TryRecvError::Empty) => {}
                Err(mpsc::TryRecvError::Disconnected) => panic!("pressure thread died"),
            }
        }
        if released_at.is_none() && hold_since.is_some_and(|t| t.elapsed() >= HOLD) {
            guard = None; // drop -> kill the generator
            released_at = Some(Instant::now());
        }
        released_at.is_none_or(|t| t.elapsed() < TAIL)
    });
    let elapsed = t0.elapsed().as_secs_f64();

    // Ledger before the surveys (they perturb it).
    let after = Ledger::read();
    println!();
    row("after pressure under hammer", &after);
    print_canary(&canary.survey());

    // Every page started at base+offset (pass 2) and was incremented once
    // per sweep; a discarded page restarted from zero and cannot match.
    let (mut intact, mut lost) = (0usize, 0usize);
    let mut examples = Vec::new();
    for off in (0..ram_size).step_by(PAGE) {
        // SAFETY: off < ram_size, mapping is readable.
        let v = unsafe { ram.add(off).cast::<u64>().read() };
        if v == RAM_GPA + off as u64 + sweeps {
            intact += 1;
        } else {
            lost += 1;
            if examples.len() < 3 {
                examples.push((off, v));
            }
        }
    }
    println!(
        "guest data under concurrent writes: {intact}/{} pages at the expected\n\
         counter (base+offset+{sweeps}), {lost} lost — {sweeps} full sweeps in {elapsed:.1}s",
        ram_size / PAGE
    );
    for (off, v) in examples {
        println!("  lost page example: offset {off:#x} reads {v:#x}");
    }
}

fn mmap_anon(size: usize) -> *mut u8 {
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
    assert_ne!(ptr, libc::MAP_FAILED, "mmap failed");
    ptr.cast()
}
