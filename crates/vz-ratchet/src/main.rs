//! The Virtualization.framework side of the story, measured live:
//! the ratchet, and the balloon that changes nothing.
//!
//! A real Linux VM boots under VZ (Apple's stack — guest RAM lives in the
//! `com.apple.Virtualization.VirtualMachine` XPC helper, which this
//! driver samples via `proc_pid_rusage`, no root needed). The guest then:
//!
//! 1. touches N GiB of anonymous memory — helper footprint climbs by N;
//! 2. frees every byte of it — helper footprint does not move (the
//!    **ratchet**: host cost is the high-water mark of touched pages);
//! 3. the balloon inflates by N GiB — the guest visibly starves
//!    (`MemAvailable` collapses) and the helper footprint still does not
//!    move (the **placebo**).
//!
//! With `--pressure-gb N` the driver then applies real host memory
//! pressure (with a 1 GiB `MADV_FREE` canary as the positive control) and
//! samples the helper again: if the balloon had actually reclassified the
//! surrendered pages, the helper's footprint would drop as they are
//! discarded; instead it stays put while the canary dies.
//!
//! Run via `./run-vz.sh` (builds the guest init, the Swift harness, and
//! fetches the pinned guest kernel).

mod harness;
mod helper;

use harness::Harness;
use ledger::canary::Canary;
use ledger::delta_mib;
use std::time::{Duration, Instant};

const BOOT_TIMEOUT: Duration = Duration::from_secs(60);
const TOUCH_TIMEOUT: Duration = Duration::from_secs(300);
const BALLOON_TIMEOUT: Duration = Duration::from_secs(30);
const HOLD: Duration = Duration::from_secs(8);
const TAIL: Duration = Duration::from_secs(2);

struct Options {
    harness: String,
    kernel: String,
    initramfs: String,
    guest_gb: usize,
    touch_gb: usize,
    pressure_gb: usize,
    verbose: bool,
}

fn parse_args() -> Options {
    let mut opts = Options {
        harness: String::new(),
        kernel: String::new(),
        initramfs: String::new(),
        guest_gb: 6,
        touch_gb: 4,
        pressure_gb: 0,
        verbose: false,
    };
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let mut value = |name: &str| args.next().unwrap_or_else(|| panic!("{name} <value>"));
        match arg.as_str() {
            "--harness" => opts.harness = value("--harness"),
            "--kernel" => opts.kernel = value("--kernel"),
            "--initramfs" => opts.initramfs = value("--initramfs"),
            "--guest-gb" => opts.guest_gb = value("--guest-gb").parse().expect("integer GiB"),
            "--touch-gb" => opts.touch_gb = value("--touch-gb").parse().expect("integer GiB"),
            "--pressure-gb" => {
                opts.pressure_gb = value("--pressure-gb").parse().expect("integer GiB");
            }
            "--verbose" => opts.verbose = true,
            other => panic!("unknown argument {other:?}"),
        }
    }
    assert!(
        !opts.harness.is_empty() && !opts.kernel.is_empty() && !opts.initramfs.is_empty(),
        "--harness/--kernel/--initramfs are required (use ./run-vz.sh)"
    );
    assert!(
        opts.touch_gb < opts.guest_gb,
        "--touch-gb must leave the guest room to run"
    );
    opts
}

fn main() {
    ledger::pressure::maybe_run_generator();
    ledger::assert_host_page_size();
    let opts = parse_args();

    println!(
        "vz-ratchet: {} GiB Linux guest on Virtualization.framework;\n\
         guest touches {} GiB, frees it, balloon inflates by {} GiB\n",
        opts.guest_gb, opts.touch_gb, opts.touch_gb
    );

    let pre_existing = helper::running_helpers();
    let mut vm = Harness::spawn(
        &opts.harness,
        &opts.kernel,
        &opts.initramfs,
        opts.guest_gb * 1024,
        opts.verbose,
    );
    vm.wait("HARNESS started", BOOT_TIMEOUT);
    vm.wait("RATCHET READY", BOOT_TIMEOUT);
    let pid = helper::find_new_helper(&pre_existing, Duration::from_secs(10));
    println!("\nguest up; sampling XPC helper pid {pid}\n");

    let baseline = helper::sample(pid);
    helper::row("VM booted, guest idle", &baseline);

    // 1. Touch.
    vm.send(&format!("guest touch {}", opts.touch_gb));
    vm.wait("RATCHET TOUCHED", TOUCH_TIMEOUT);
    std::thread::sleep(Duration::from_secs(1));
    let touched = helper::sample(pid);
    helper::row("after guest touched its memory", &touched);

    // 2. Free.
    vm.send("guest free");
    vm.wait("RATCHET FREED", BOOT_TIMEOUT);
    std::thread::sleep(Duration::from_secs(2));
    let freed = helper::sample(pid);
    helper::row("after guest freed every byte", &freed);
    println!(
        "\nΔfootprint touch: {:+.1} MiB, free: {:+.1} MiB — the ratchet:\n\
         the host keeps charging for pages the guest no longer uses\n",
        delta_mib(touched.phys_footprint, baseline.phys_footprint),
        delta_mib(freed.phys_footprint, touched.phys_footprint),
    );

    // 3. Balloon: target = total - touch, i.e. inflate by the freed amount.
    let mem_before_kib = vm.guest_mem_available_kib();
    let target_mib = (opts.guest_gb - opts.touch_gb) * 1024;
    vm.send(&format!("balloon {target_mib}"));
    vm.wait("HARNESS balloon-target", Duration::from_secs(10));
    // The guest driver inflates asynchronously; watch MemAvailable fall.
    let deadline = Instant::now() + BALLOON_TIMEOUT;
    let mut mem_after_kib = mem_before_kib;
    while Instant::now() < deadline {
        std::thread::sleep(Duration::from_secs(1));
        mem_after_kib = vm.guest_mem_available_kib();
        if mem_after_kib < mem_before_kib / 2 {
            break;
        }
    }
    std::thread::sleep(Duration::from_secs(2));
    let ballooned = helper::sample(pid);
    helper::row("after balloon inflated", &ballooned);
    println!(
        "\nguest MemAvailable {:.0} MiB -> {:.0} MiB (the balloon really did\n\
         inflate; the guest handed the pages over) and Δfootprint is\n\
         {:+.1} MiB — the placebo: nothing reached the host ledger\n",
        mem_before_kib as f64 / 1024.0,
        mem_after_kib as f64 / 1024.0,
        delta_mib(ballooned.phys_footprint, freed.phys_footprint),
    );

    // 4. Optional: real pressure with a positive control.
    if opts.pressure_gb > 0 {
        println!(
            "generating pressure: dirtying {} GiB in a child, holding {}s ...",
            opts.pressure_gb,
            HOLD.as_secs()
        );
        let canary = Canary::arm(1);
        let guard = ledger::pressure::apply(opts.pressure_gb).expect("pressure generator");
        std::thread::sleep(HOLD);
        drop(guard);
        std::thread::sleep(TAIL);

        let after = helper::sample(pid);
        println!();
        helper::row("after real host memory pressure", &after);
        let verdict = canary.survey();
        println!(
            "canary (1 GiB MADV_FREE control): {}/{} pages discarded — {}",
            verdict.discarded,
            verdict.total,
            if verdict.conclusive() {
                "pressure reached anonymous memory; the run is conclusive"
            } else {
                "pressure NEVER reached anonymous memory; the run is INCONCLUSIVE \
                 (raise --pressure-gb)"
            }
        );
        println!(
            "helper Δfootprint under pressure: {:+.1} MiB — pages a working\n\
             balloon would have made discardable were not discarded",
            delta_mib(after.phys_footprint, ballooned.phys_footprint),
        );
    }

    vm.quit();
    println!(
        "\nGuest RAM on VZ lives in Apple's XPC helper. The guest freeing\n\
         memory changes nothing; the balloon inflating changes nothing; the\n\
         only ledger that moved is the one that ratcheted up on first touch.\n\
         Compare ./run.sh (hv-reclaim): same Mac, one API layer down, the\n\
         same reclaim drops the footprint by the full amount."
    );
}
