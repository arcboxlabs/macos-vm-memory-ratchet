//! The counter-demonstration: a real VM on Hypervisor.framework whose
//! host process *does* give guest memory back to macOS — plus the trap
//! we found on the way.
//!
//! A minimal aarch64 guest (seven instructions, no kernel) dirties every
//! host page of its RAM on a real vCPU, then rings an MMIO doorbell —
//! standing in for a guest's free-page report ("I'm done with this
//! range"). The host answers with the sequence a production VMM would
//! use:
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
//! Build & run (the hypervisor entitlement accepts ad-hoc signing):
//!
//! ```sh
//! ./run.sh                # the fix
//! ./run.sh --naive        # the trap
//! ```

use ledger::{delta_mib, row, Ledger};
use std::ffi::c_void;
use std::time::Instant;

// --- Hypervisor.framework FFI (aarch64) ---------------------------------

#[repr(C)]
#[derive(Clone, Copy)]
struct HvVcpuExitException {
    syndrome: u64,
    virtual_address: u64,
    physical_address: u64,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct HvVcpuExitInfo {
    reason: u32,
    exception: HvVcpuExitException,
}

#[link(name = "Hypervisor", kind = "framework")]
unsafe extern "C" {
    fn hv_vm_create(config: *mut c_void) -> i32;
    fn hv_vm_destroy() -> i32;
    fn hv_vm_map(addr: *mut u8, ipa: u64, size: usize, flags: u64) -> i32;
    fn hv_vm_unmap(ipa: u64, size: usize) -> i32;
    fn hv_vcpu_create(vcpu: *mut u64, exit: *mut *const HvVcpuExitInfo, config: *mut c_void)
        -> i32;
    fn hv_vcpu_destroy(vcpu: u64) -> i32;
    fn hv_vcpu_run(vcpu: u64) -> i32;
    fn hv_vcpu_set_reg(vcpu: u64, reg: u32, value: u64) -> i32;
    fn hv_vcpu_get_reg(vcpu: u64, reg: u32, value: *mut u64) -> i32;
}

const HV_MEMORY_READ: u64 = 1 << 0;
const HV_MEMORY_WRITE: u64 = 1 << 1;
const HV_MEMORY_EXEC: u64 = 1 << 2;

const HV_REG_X1: u32 = 1;
const HV_REG_X2: u32 = 2;
const HV_REG_X3: u32 = 3;
const HV_REG_PC: u32 = 31;
const HV_REG_CPSR: u32 = 34;

const HV_EXIT_REASON_CANCELED: u32 = 0;
const HV_EXIT_REASON_EXCEPTION: u32 = 1;
const EC_DATA_ABORT_LOWER_EL: u64 = 0x24;

/// EL1h with A/I/F/D masked — how a bare-metal guest starts.
const PSTATE_EL1H_MASKED: u64 = 0x3C5;

// --- Guest layout --------------------------------------------------------

const CODE_GPA: u64 = 0x1000_0000;
const RAM_GPA: u64 = 0x8000_0000;
const DOORBELL_GPA: u64 = 0x0F00_0000; // deliberately unmapped
const PAGE: usize = 16 * 1024;

/// The whole guest. x1 = cursor, x2 = end, x3 = doorbell (set by host).
///
/// ```text
/// loop: str x1, [x1]           ; dirty one host page
///       add x1, x1, #4, lsl 12 ; += 16 KiB
///       cmp x1, x2
///       b.lo loop
///       str xzr, [x3]          ; doorbell -> data abort -> host
/// halt: wfi
///       b halt
/// ```
const GUEST_CODE: [u32; 7] = [
    0xF900_0021, // str x1, [x1]
    0x9140_1021, // add x1, x1, #4, lsl #12
    0xEB02_003F, // cmp x1, x2
    0x54FF_FFA3, // b.lo loop
    0xF900_007F, // str xzr, [x3]
    0xD503_207F, // wfi
    0x17FF_FFFF, // b halt
];

fn check(ret: i32, what: &str) {
    assert_eq!(ret, 0, "{what} failed: {ret:#x}");
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
    /// After the guest re-touches its RAM, apply real memory pressure and
    /// verify the guest's data survives (the "rogue page" question).
    pressure_check: bool,
    /// GiB of dirty memory the pressure generator holds (--pressure-gb).
    pressure_gb: usize,
}

fn parse_args() -> Options {
    let mut opts = Options {
        advice: libc::MADV_FREE_REUSABLE,
        advice_name: "MADV_FREE_REUSABLE",
        size_gb: 3,
        naive: false,
        host_touch: false,
        pressure_check: false,
        pressure_gb: 48,
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
            "--pressure-check" => opts.pressure_check = true,
            "--pressure-gb" => {
                opts.pressure_gb = args
                    .next()
                    .expect("--pressure-gb N")
                    .parse()
                    .expect("integer GiB");
            }
            other => panic!("unknown argument {other:?}"),
        }
    }
    opts
}

/// Arm the guest registers for one full touch pass and run to the doorbell.
fn run_touch_pass(vcpu: u64, exit: *const HvVcpuExitInfo, ram_size: usize) {
    check(
        unsafe { hv_vcpu_set_reg(vcpu, HV_REG_PC, CODE_GPA) },
        "set PC",
    );
    check(
        unsafe { hv_vcpu_set_reg(vcpu, HV_REG_X1, RAM_GPA) },
        "set X1",
    );
    check(
        unsafe { hv_vcpu_set_reg(vcpu, HV_REG_X2, RAM_GPA + ram_size as u64) },
        "set X2",
    );
    check(
        unsafe { hv_vcpu_set_reg(vcpu, HV_REG_X3, DOORBELL_GPA) },
        "set X3",
    );
    loop {
        check(unsafe { hv_vcpu_run(vcpu) }, "hv_vcpu_run");
        // SAFETY: `exit` is valid for the lifetime of the vcpu.
        let info = unsafe { *exit };
        // Spurious cancellation: just re-enter the guest.
        if info.reason == HV_EXIT_REASON_CANCELED {
            continue;
        }
        let ec = (info.exception.syndrome >> 26) & 0x3F;
        if info.reason == HV_EXIT_REASON_EXCEPTION
            && ec == EC_DATA_ABORT_LOWER_EL
            && info.exception.physical_address == DOORBELL_GPA
        {
            return;
        }
        let mut pc = 0u64;
        let _ = unsafe { hv_vcpu_get_reg(vcpu, HV_REG_PC, &raw mut pc) };
        panic!(
            "unexpected exit: reason={} ec={ec:#x} pa={:#x} pc={pc:#x}",
            info.reason, info.exception.physical_address
        );
    }
}

fn main() {
    ledger::pressure::maybe_run_generator();

    let Options {
        advice,
        advice_name,
        size_gb,
        naive,
        host_touch,
        pressure_check,
        pressure_gb,
    } = parse_args();
    let ram_size = size_gb << 30;

    println!(
        "hv-reclaim: {size_gb} GiB guest RAM in-process, dirtied by {},\n\
         reclaimed with {}\n",
        if host_touch {
            "the HOST (control)"
        } else {
            "the guest on a real vCPU"
        },
        if naive {
            format!("{advice_name} while still stage-2 mapped (--naive)")
        } else {
            format!("hv_vm_unmap \u{2192} {advice_name} \u{2192} hv_vm_map")
        },
    );

    let baseline = Ledger::read();
    row("baseline", &baseline);

    // Guest RAM and code are plain anonymous memory of this process.
    let ram = mmap_anon(ram_size);
    let code = mmap_anon(PAGE);
    for (i, insn) in GUEST_CODE.iter().enumerate() {
        // SAFETY: i*4 < PAGE.
        unsafe { code.add(i * 4).cast::<u32>().write(*insn) };
    }

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

    // Pass 1: dirty every host page of guest RAM.
    let start = Instant::now();
    if host_touch {
        for off in (0..ram_size).step_by(PAGE) {
            // SAFETY: off < ram_size, mapping is writable.
            unsafe { ram.add(off).cast::<u64>().write(off as u64) };
        }
    } else {
        run_touch_pass(vcpu, exit, ram_size);
    }
    let touch = start.elapsed();

    let dirtied = Ledger::read();
    println!(
        "\ndirtied {size_gb} GiB in {:.2}s ({:.1} GiB/s)\n",
        touch.as_secs_f64(),
        size_gb as f64 / touch.as_secs_f64()
    );
    row("after RAM dirtied", &dirtied);

    // The doorbell means "I'm done with this memory". Answer it.
    if !naive {
        check(
            unsafe { hv_vm_unmap(RAM_GPA, ram_size) },
            "hv_vm_unmap(ram)",
        );
    }
    let rc = unsafe { libc::madvise(ram.cast(), ram_size, advice) };
    assert_eq!(
        rc,
        0,
        "madvise({advice_name}) failed: {}",
        std::io::Error::last_os_error()
    );
    if !naive {
        check(
            unsafe { hv_vm_map(ram, RAM_GPA, ram_size, HV_MEMORY_READ | HV_MEMORY_WRITE) },
            "hv_vm_map(ram) remap",
        );
    }
    std::thread::sleep(std::time::Duration::from_millis(200));

    let reclaimed = Ledger::read();
    row("after reclaim", &reclaimed);
    println!(
        "\nΔfootprint touch: {:+.1} MiB, reclaim: {:+.1} MiB",
        delta_mib(dirtied.phys_footprint, baseline.phys_footprint),
        delta_mib(reclaimed.phys_footprint, dirtied.phys_footprint),
    );

    // Pass 2 (guest mode only): the VM is still alive — prove it by
    // letting the guest fault everything back in on demand.
    if !host_touch {
        run_touch_pass(vcpu, exit, ram_size);
        let retouched = Ledger::read();
        println!();
        row("after guest re-touches its RAM", &retouched);
        println!(
            "Δfootprint re-touch: {:+.1} MiB (paid again only when actually used)",
            delta_mib(retouched.phys_footprint, reclaimed.phys_footprint),
        );

        // The rogue-page question: the guest has live data in pages the
        // ledger may still consider reusable. Does real pressure discard
        // them (data loss), or does the stage-2 dirty state protect them?
        if pressure_check {
            // Deterministic pressure: a child of this binary dirties
            // --pressure-gb GiB and holds it (see ledger::pressure for why
            // Apple's memory_pressure tool is not used).
            println!(
                "\ngenerating pressure: dirtying {pressure_gb} GiB in a child, holding 8s ..."
            );
            let guard = ledger::pressure::apply(pressure_gb).expect("pressure generator");
            std::thread::sleep(std::time::Duration::from_secs(8));
            drop(guard);
            std::thread::sleep(std::time::Duration::from_secs(2));

            // Read the ledger BEFORE the survey: the survey reads through
            // the host mapping and perturbs the per-entry counters.
            let after_pressure = Ledger::read();
            println!();
            row("after real memory pressure", &after_pressure);

            // The guest wrote its own GPA into the first word of every
            // page. Verify host-side through our mapping of guest RAM.
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
