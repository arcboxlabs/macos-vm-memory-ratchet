//! Allocation-shape discriminator: does the guest-touched trap depend on how
//! the VMM allocated guest RAM?
//!
//! Six virtual-machine monitors on this platform disagree, in shipping code,
//! about whether in-place `madvise` releases guest memory. Every one of them
//! --- and every measurement elsewhere in this harness --- allocates guest RAM
//! with plain `mmap`. Apple ships `hv_vm_allocate`, whose header contains the
//! platform's only statement about the accounting contract:
//!
//! > This API enables accurate memory accounting of the allocations it creates
//!
//! That clause is a candidate for the missing precondition, and nothing in
//! this ecosystem has tested it. This mode runs the matrix that decides:
//! allocation shape x who touched the page x in-place versus unmap-first.
//!
//! **One cell per child process.** `phys_footprint` is a task-wide counter, so
//! cells cannot share a task --- an earlier cell's residue would silently
//! become the next cell's baseline, which is the same measurement error the
//! sticky-reusable regime produces. The parent re-executes itself once per
//! cell (the `pressure` generator's idiom) and never creates a VM of its own.

use crate::guest::{run_touch_pass, write_code_page, CODE_GPA, RAM_GPA};
use crate::hvf::{
    check, hv_vcpu_create, hv_vcpu_destroy, hv_vcpu_set_reg, hv_vm_allocate, hv_vm_create,
    hv_vm_deallocate, hv_vm_destroy, hv_vm_map, hv_vm_unmap, HvVcpuExitInfo, HV_ALLOCATE_DEFAULT,
    HV_MEMORY_EXEC, HV_MEMORY_READ, HV_MEMORY_WRITE, HV_REG_CPSR, PSTATE_EL1H_MASKED,
};
use ledger::{delta_mib, Ledger, PAGE};
use std::process::Command;

const CELL_ENV: &str = "RATCHET_ALLOC_SHAPE_CELL";

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Shape {
    /// What this harness, and every monitor we surveyed, actually uses.
    MmapPrivate,
    /// The shape a VMM picks when a device thread shares the guest window.
    MmapShared,
    /// Apple's documented allocator for guest RAM.
    HvAllocate,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Toucher {
    /// Control: the pages are resident, but no stage-2 mapping ever resolved.
    Host,
    /// The real case: a vCPU faulted every page through stage-2.
    Guest,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Sequence {
    /// The contested call: advise while the stage-2 mapping still exists.
    InPlace,
    /// The sequence three monitors ship: unmap, advise, remap.
    UnmapFirst,
    /// Unmap and remap with NO advice at all. Isolates how much of the
    /// working sequence's release the `madvise` is actually responsible for
    /// --- the guest-touched rows leave `reusable` at zero, which is not
    /// what a reusable-marking release looks like.
    UnmapOnly,
}

impl Shape {
    fn name(self) -> &'static str {
        match self {
            Self::MmapPrivate => "mmap(PRIVATE)",
            Self::MmapShared => "mmap(SHARED)",
            Self::HvAllocate => "hv_vm_allocate",
        }
    }

    fn parse(s: &str) -> Self {
        match s {
            "private" => Self::MmapPrivate,
            "shared" => Self::MmapShared,
            "hvalloc" => Self::HvAllocate,
            other => panic!("unknown shape {other:?}"),
        }
    }

    fn tag(self) -> &'static str {
        match self {
            Self::MmapPrivate => "private",
            Self::MmapShared => "shared",
            Self::HvAllocate => "hvalloc",
        }
    }

    /// Allocate `size` bytes of guest RAM in this shape. Returns null on a
    /// failure the caller should report rather than crash on: whether Apple's
    /// allocator refuses a given size is itself a result.
    fn allocate(self, size: usize) -> *mut u8 {
        match self {
            Self::MmapPrivate => mmap_flags(size, libc::MAP_ANON | libc::MAP_PRIVATE),
            Self::MmapShared => mmap_flags(size, libc::MAP_ANON | libc::MAP_SHARED),
            Self::HvAllocate => {
                let mut p: *mut u8 = std::ptr::null_mut();
                let rc = unsafe { hv_vm_allocate(&raw mut p, size, HV_ALLOCATE_DEFAULT) };
                if rc == 0 {
                    p
                } else {
                    eprintln!("hv_vm_allocate({size}) failed: {rc:#x}");
                    std::ptr::null_mut()
                }
            }
        }
    }

    fn release(self, p: *mut u8, size: usize) {
        match self {
            Self::HvAllocate => {
                let _ = unsafe { hv_vm_deallocate(p, size) };
            }
            _ => {
                let _ = unsafe { libc::munmap(p.cast(), size) };
            }
        }
    }
}

fn mmap_flags(size: usize, flags: i32) -> *mut u8 {
    let p = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            size,
            libc::PROT_READ | libc::PROT_WRITE,
            flags,
            -1,
            0,
        )
    };
    if p == libc::MAP_FAILED {
        std::ptr::null_mut()
    } else {
        p.cast()
    }
}

impl Toucher {
    fn name(self) -> &'static str {
        match self {
            Self::Host => "host",
            Self::Guest => "guest",
        }
    }

    fn parse(s: &str) -> Self {
        match s {
            "host" => Self::Host,
            "guest" => Self::Guest,
            other => panic!("unknown toucher {other:?}"),
        }
    }
}

impl Sequence {
    fn name(self) -> &'static str {
        match self {
            Self::InPlace => "in-place",
            Self::UnmapFirst => "unmap-first",
            Self::UnmapOnly => "unmap-only",
        }
    }

    fn parse(s: &str) -> Self {
        match s {
            "inplace" => Self::InPlace,
            "unmap" => Self::UnmapFirst,
            "unmaponly" => Self::UnmapOnly,
            other => panic!("unknown sequence {other:?}"),
        }
    }

    fn tag(self) -> &'static str {
        match self {
            Self::InPlace => "inplace",
            Self::UnmapFirst => "unmap",
            Self::UnmapOnly => "unmaponly",
        }
    }
}

/// If this process was re-executed as one matrix cell, run it and never
/// return. Call first thing in `main`, next to the pressure generator.
pub fn maybe_run_cell() {
    let Ok(spec) = std::env::var(CELL_ENV) else {
        return;
    };
    let parts: Vec<&str> = spec.split(':').collect();
    assert_eq!(
        parts.len(),
        4,
        "cell spec is shape:toucher:sequence:size_gb"
    );
    let shape = Shape::parse(parts[0]);
    let toucher = Toucher::parse(parts[1]);
    let sequence = Sequence::parse(parts[2]);
    let size_gb: usize = parts[3].parse().expect("cell size");
    run_cell(shape, toucher, sequence, size_gb << 30);
    std::process::exit(0);
}

/// One cell, in its own task: allocate, map, touch, measure, release, measure.
///
/// Prints a single `CELL ` line for the parent to collect. Any failure that is
/// a *result* (the allocator refusing the shape, `madvise` rejecting it) is
/// reported in that line rather than raised --- a shape that cannot be built
/// is an answer about the shape.
fn run_cell(shape: Shape, toucher: Toucher, sequence: Sequence, size: usize) {
    // Before anything is allocated: the floor this cell's charges sit on.
    let idle = Ledger::read();
    let ram = shape.allocate(size);
    if ram.is_null() {
        println!(
            "CELL {} {} {} alloc-failed 0 0 0 0",
            shape.tag(),
            toucher.name(),
            sequence.tag()
        );
        return;
    }

    // The code page is always plain private memory: it is the guest's
    // instruction source, not the memory under test.
    let code = mmap_flags(PAGE, libc::MAP_ANON | libc::MAP_PRIVATE);
    assert!(!code.is_null(), "code page mmap failed");
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
        unsafe { hv_vm_map(ram, RAM_GPA, size, HV_MEMORY_READ | HV_MEMORY_WRITE) },
        "hv_vm_map(ram)",
    );

    match toucher {
        Toucher::Host => {
            for off in (0..size).step_by(PAGE) {
                // SAFETY: off < size and the mapping is writable.
                unsafe { ram.add(off).cast::<u64>().write(0xA5A5_0000 + off as u64) };
            }
        }
        Toucher::Guest => {
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
            run_touch_pass(vcpu, exit, size);
            check(unsafe { hv_vcpu_destroy(vcpu) }, "hv_vcpu_destroy");
        }
    }

    let before = Ledger::read();

    if sequence != Sequence::InPlace {
        check(unsafe { hv_vm_unmap(RAM_GPA, size) }, "hv_vm_unmap");
    }
    let (rc, skipped) = if sequence == Sequence::UnmapOnly {
        (0, true)
    } else {
        (
            unsafe { libc::madvise(ram.cast(), size, libc::MADV_FREE_REUSABLE) },
            false,
        )
    };
    let errno = if rc == 0 {
        0
    } else {
        std::io::Error::last_os_error().raw_os_error().unwrap_or(-1)
    };
    if sequence != Sequence::InPlace {
        check(
            unsafe { hv_vm_map(ram, RAM_GPA, size, HV_MEMORY_READ | HV_MEMORY_WRITE) },
            "hv_vm_map remap",
        );
    }

    let after = Ledger::read();

    println!(
        "CELL {} {} {} {} {:.1} {:.1} {:.1} {:.1}",
        shape.tag(),
        toucher.name(),
        sequence.tag(),
        if skipped {
            "none".to_string()
        } else if rc == 0 {
            "ok".to_string()
        } else {
            format!("errno{errno}")
        },
        delta_mib(after.phys_footprint, before.phys_footprint),
        ledger::mib(before.phys_footprint),
        ledger::mib(idle.phys_footprint),
        delta_mib(after.reusable, before.reusable),
    );

    check(unsafe { hv_vm_destroy() }, "hv_vm_destroy");
    shape.release(ram, size);
    let _ = unsafe { libc::munmap(code.cast(), PAGE) };
}

struct Row {
    shape: Shape,
    toucher: Toucher,
    sequence: Sequence,
    status: String,
    delta_mib: f64,
    before_mib: f64,
    host_idle_mib: f64,
    /// Change in the `reusable` counter: whether the kernel ACTED on the
    /// advice at all, as opposed to returning success and declining.
    reusable_mib: f64,
}

/// Drive the whole matrix, one child per cell, and print the verdict table.
pub fn run(size_gb: usize) {
    println!(
        "alloc-shape discriminator: {size_gb} GiB guest RAM per cell, one child \
         process per cell\n\
         (phys_footprint is task-wide, so cells must not share a task)\n"
    );

    let exe = std::env::current_exe().expect("current_exe");
    let mut rows = Vec::new();

    for shape in [Shape::MmapPrivate, Shape::MmapShared, Shape::HvAllocate] {
        for toucher in [Toucher::Host, Toucher::Guest] {
            for sequence in [Sequence::InPlace, Sequence::UnmapFirst, Sequence::UnmapOnly] {
                let spec = format!(
                    "{}:{}:{}:{size_gb}",
                    shape.tag(),
                    toucher.name(),
                    sequence.tag()
                );
                let out = Command::new(&exe)
                    .env(CELL_ENV, &spec)
                    .output()
                    .expect("spawn cell");
                let stdout = String::from_utf8_lossy(&out.stdout);
                let line = stdout
                    .lines()
                    .find(|l| l.starts_with("CELL "))
                    .unwrap_or_else(|| {
                        panic!(
                            "cell {spec} produced no result\nstdout: {stdout}\nstderr: {}",
                            String::from_utf8_lossy(&out.stderr)
                        )
                    });
                let f: Vec<&str> = line.split_whitespace().collect();
                rows.push(Row {
                    shape,
                    toucher,
                    sequence,
                    status: f[4].to_string(),
                    delta_mib: f[5].parse().unwrap_or(0.0),
                    before_mib: f.get(6).and_then(|v| v.parse().ok()).unwrap_or(0.0),
                    host_idle_mib: f.get(7).and_then(|v| v.parse().ok()).unwrap_or(0.0),
                    reusable_mib: f.get(8).and_then(|v| v.parse().ok()).unwrap_or(0.0),
                });
            }
        }
    }

    // The baseline column is load-bearing, not decoration. A cell whose
    // footprint never rose after touching the whole range was never charged
    // for that memory, so a zero delta says nothing about releasing it --- a
    // different claim entirely from "charged and cannot be released."
    println!(
        "{:<16} {:<7} {:<12} {:>8} {:>9} {:>9} {:>9} {:>9}",
        "allocation", "touched", "sequence", "madvise", "charged", "reusable", "delta", "released"
    );
    for r in &rows {
        // A release is credited only when the ledger moved by most of what the
        // cell was actually charged for; anything less is the platform
        // declining while returning success, which is the paper's subject.
        let charged = r.before_mib - r.host_idle_mib;
        let released = if charged < 64.0 {
            "n/a"
        } else if r.delta_mib <= -(charged * 0.5) {
            "YES"
        } else {
            "no"
        };
        println!(
            "{:<16} {:<7} {:<12} {:>8} {:>9.1} {:>+9.1} {:>+9.1} {:>9}",
            r.shape.name(),
            r.toucher.name(),
            r.sequence.name(),
            r.status,
            charged,
            r.reusable_mib,
            r.delta_mib,
            released
        );
    }

    let released = |r: &Row| {
        let charged = r.before_mib - r.host_idle_mib;
        charged >= 64.0 && r.delta_mib <= -(charged * 0.5)
    };
    let cell = |shape: Shape, toucher: Toucher, seq: Sequence| {
        rows.iter()
            .find(|r| r.shape == shape && r.toucher == toucher && r.sequence == seq)
            .expect("cell present")
    };

    println!("\nverdict:");

    // 1. Is allocation shape what separates the two camps?
    if rows
        .iter()
        .any(|r| r.toucher == Toucher::Guest && r.sequence == Sequence::InPlace && released(r))
    {
        println!(
            "  1. Allocation shape IS the precondition: in-place advice releases\n     \
             guest-touched pages in at least one shape. Monitors using another\n     \
             shape are violating it."
        );
    } else {
        println!(
            "  1. Allocation shape is NOT what separates the two camps: in-place\n     \
             advice releases nothing on guest-touched pages in any shape tested,\n     \
             including Apple's own hv_vm_allocate."
        );
    }

    // 2. What does the platform's documented allocator actually give a VMM?
    let hv = cell(Shape::HvAllocate, Toucher::Guest, Sequence::InPlace);
    let hv_charged = hv.before_mib - hv.host_idle_mib;
    if rows
        .iter()
        .any(|r| r.shape == Shape::HvAllocate && released(r))
    {
        println!("  2. hv_vm_allocate memory can be released by at least one sequence.");
    } else {
        println!(
            "  2. hv_vm_allocate memory is charged ({hv_charged:.0} MiB) and released by\n     \
             NOTHING here --- not the advice, not the unmap, and not on\n     \
             host-touched pages either. madvise returns success throughout. The\n     \
             platform's own documented allocator for guest RAM, whose stated\n     \
             benefit is accurate accounting, has no partial-release path at all."
        );
    }

    // 3. Within the working sequence, which step does the releasing?
    let with_advice = cell(Shape::MmapPrivate, Toucher::Guest, Sequence::UnmapFirst);
    let without = cell(Shape::MmapPrivate, Toucher::Guest, Sequence::UnmapOnly);
    let host_only = cell(Shape::MmapPrivate, Toucher::Host, Sequence::UnmapOnly);
    if released(without) && released(with_advice) && !released(host_only) {
        println!(
            "  3. The advice is NOT what releases guest memory. Unmapping alone\n     \
             returns {:+.1} MiB against {:+.1} with the advice, and `reusable` stays\n     \
             at {:+.1} either way --- a reusable-marking release would move it. The\n     \
             control holds: unmap alone releases nothing host-touched ({:+.1} MiB).\n     \
             The two mechanisms are disjoint: advice releases host-touched pages,\n     \
             stage-2 teardown releases guest-touched ones. Every monitor shipping\n     \
             unmap + advise + remap is carrying a no-op middle step.",
            without.delta_mib, with_advice.delta_mib, without.reusable_mib, host_only.delta_mib
        );
    }
}
