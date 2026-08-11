//! The guest programs and the host loops that drive them.

use crate::hvf::{
    check, hv_vcpu_get_reg, hv_vcpu_run, hv_vcpu_set_reg, HvVcpuExitInfo, EC_DATA_ABORT_LOWER_EL,
    HV_EXIT_REASON_CANCELED, HV_EXIT_REASON_EXCEPTION, HV_REG_PC, HV_REG_X1, HV_REG_X2, HV_REG_X3,
    HV_REG_X4, HV_REG_X5, HV_REG_X6,
};
use ledger::PAGE;

pub const CODE_GPA: u64 = 0x1000_0000;
pub const RAM_GPA: u64 = 0x8000_0000;
pub const DOORBELL_GPA: u64 = 0x0F00_0000; // deliberately unmapped

/// One touch pass. x1 = cursor, x2 = end, x3 = doorbell (set by host).
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
const TOUCH_CODE: [u32; 7] = [
    0xF900_0021, // str x1, [x1]
    0x9140_1021, // add x1, x1, #4, lsl #12
    0xEB02_003F, // cmp x1, x2
    0x54FF_FFA3, // b.lo loop
    0xF900_007F, // str xzr, [x3]
    0xD503_207F, // wfi
    0x17FF_FFFF, // b halt
];

/// The concurrent-write race probe: read-modify-write increment the first
/// word of every page, ring the doorbell after each full sweep, repeat.
/// x2 = end, x3 = doorbell, x4 = completed sweeps, x6 = RAM base.
///
/// A page the kernel discards restarts its counter from zero and can
/// never catch back up to `base + offset + sweeps`, so data loss is
/// visible in the final survey no matter *when* during the run it
/// happened — including while a sweep was actively writing.
///
/// ```text
/// outer: mov x1, x6
/// loop:  ldr x5, [x1]
///        add x5, x5, #1
///        str x5, [x1]
///        add x1, x1, #4, lsl 12
///        cmp x1, x2
///        b.lo loop
///        add x4, x4, #1
///        str x4, [x3]          ; doorbell -> host resumes (PC+4) or stops
///        b outer
/// ```
const HAMMER_CODE: [u32; 10] = [
    0xAA06_03E1, // mov x1, x6
    0xF940_0025, // ldr x5, [x1]
    0x9100_04A5, // add x5, x5, #1
    0xF900_0025, // str x5, [x1]
    0x9140_1021, // add x1, x1, #4, lsl #12
    0xEB02_003F, // cmp x1, x2
    0x54FF_FF63, // b.lo loop
    0x9100_0484, // add x4, x4, #1
    0xF900_0064, // str x4, [x3]
    0x17FF_FFF7, // b outer
];

/// Read-only sweep: load one word per host page, never store. Separates
/// "the guest dirtied it" from "the guest merely touched it" — if a
/// read-only pass also pins the ledger, the trap is about mapping and
/// wiring, not about dirty state.
///
/// ```text
/// loop: ldr x5, [x1]            ; read one host page
///       add x1, x1, #4, lsl 12  ; += 16 KiB
///       cmp x1, x2
///       b.lo loop
///       str xzr, [x3]           ; doorbell
/// halt: wfi
///       b halt
/// ```
const READ_CODE: [u32; 7] = [
    0xF940_0025, // ldr x5, [x1]
    0x9140_1021, // add x1, x1, #4, lsl #12
    0xEB02_003F, // cmp x1, x2
    0x54FF_FFA3, // b.lo loop
    0xF900_007F, // str xzr, [x3]
    0xD503_207F, // wfi
    0x17FF_FFFF, // b halt
];

/// Guest-side integrity check: compare the first word of every host page
/// in a range against an expected value and count the mismatches, without
/// the host ever reading the pages.
///
/// Host-side verification would defeat a long-running probe: reading guest
/// RAM through the host mapping makes the pages host-referenced, which is
/// exactly the state that makes the pageout scan spare them. The guest
/// checking its own memory leaves the pages in the state under test.
///
/// x1 = cursor, x2 = end, x3 = doorbell, x5 = expected, x4 = mismatches.
///
/// ```text
/// loop: ldr x6, [x1]
///       cmp x6, x5
///       b.eq ok
///       add x4, x4, #1
/// ok:   add x1, x1, #4, lsl 12
///       cmp x1, x2
///       b.lo loop
///       str x4, [x3]           ; doorbell; x4 carries the verdict
/// halt: wfi
///       b halt
/// ```
const VERIFY_CODE: [u32; 10] = [
    0xF940_0026, // ldr x6, [x1]
    0xEB05_00DF, // cmp x6, x5
    0x5400_0040, // b.eq ok
    0x9100_0484, // add x4, x4, #1
    0x9140_1021, // ok: add x1, x1, #4, lsl #12
    0xEB02_003F, // cmp x1, x2
    0x54FF_FF43, // b.lo loop
    0xF900_0064, // str x4, [x3]
    0xD503_207F, // wfi
    0x17FF_FFFF, // b halt
];

/// Stamp an absolute value into the first word of every host page in a
/// range. Needed because a reclaimed range does *not* come back zeroed:
/// `MADV_FREE_REUSABLE` means "you may take these", and pages nobody took
/// still hold the guest's old data after the remap. An
/// increment-from-unknown check would therefore be checking nothing; a
/// caller that wants a known state must write one.
///
/// x1 = cursor, x2 = end, x3 = doorbell, x5 = value.
///
/// ```text
/// loop: str x5, [x1]
///       add x1, x1, #4, lsl 12
///       cmp x1, x2
///       b.lo loop
///       str xzr, [x3]
/// halt: wfi
///       b halt
/// ```
const FILL_CODE: [u32; 7] = [
    0xF900_0025, // str x5, [x1]
    0x9140_1021, // add x1, x1, #4, lsl #12
    0xEB02_003F, // cmp x1, x2
    0x54FF_FFA3, // b.lo loop
    0xF900_007F, // str xzr, [x3]
    0xD503_207F, // wfi
    0x17FF_FFFF, // b halt
];

const FILL_OFF: usize = 0x200;
const FILL_GPA: u64 = CODE_GPA + FILL_OFF as u64;
const HAMMER_OFF: usize = 0x80;
const HAMMER_GPA: u64 = CODE_GPA + HAMMER_OFF as u64;
const VERIFY_OFF: usize = 0x180;
const VERIFY_GPA: u64 = CODE_GPA + VERIFY_OFF as u64;
const READ_OFF: usize = 0x100;
const READ_GPA: u64 = CODE_GPA + READ_OFF as u64;

unsafe extern "C" {
    /// libkern/OSCacheControl.h: clean D-cache to the point of unification
    /// and invalidate I-cache for the range. Apple Silicon I/D caches are
    /// not coherent; without this the guest may fetch stale instruction
    /// bytes on a machine where old lines linger.
    fn sys_icache_invalidate(start: *mut std::ffi::c_void, len: usize);
}

/// Write both guest programs into the host-mapped code page.
pub fn write_code_page(code: *mut u8) {
    for (i, insn) in TOUCH_CODE.iter().enumerate() {
        // SAFETY: i*4 < PAGE.
        unsafe { code.add(i * 4).cast::<u32>().write(*insn) };
    }
    for (i, insn) in HAMMER_CODE.iter().enumerate() {
        // SAFETY: HAMMER_OFF + i*4 < PAGE.
        unsafe { code.add(HAMMER_OFF + i * 4).cast::<u32>().write(*insn) };
    }
    for (i, insn) in READ_CODE.iter().enumerate() {
        // SAFETY: READ_OFF + i*4 < PAGE.
        unsafe { code.add(READ_OFF + i * 4).cast::<u32>().write(*insn) };
    }
    for (i, insn) in VERIFY_CODE.iter().enumerate() {
        // SAFETY: VERIFY_OFF + i*4 < PAGE.
        unsafe { code.add(VERIFY_OFF + i * 4).cast::<u32>().write(*insn) };
    }
    for (i, insn) in FILL_CODE.iter().enumerate() {
        // SAFETY: FILL_OFF + i*4 < PAGE.
        unsafe { code.add(FILL_OFF + i * 4).cast::<u32>().write(*insn) };
    }
    // SAFETY: `code` is a valid PAGE-sized mapping we just wrote.
    unsafe { sys_icache_invalidate(code.cast(), PAGE) };
}

enum GuestExit {
    Canceled,
    Doorbell,
}

fn run_to_doorbell(vcpu: u64, exit: *const HvVcpuExitInfo) -> GuestExit {
    check(unsafe { hv_vcpu_run(vcpu) }, "hv_vcpu_run");
    // SAFETY: `exit` is valid for the lifetime of the vcpu.
    let info = unsafe { *exit };
    if info.reason == HV_EXIT_REASON_CANCELED {
        return GuestExit::Canceled;
    }
    let ec = (info.exception.syndrome >> 26) & 0x3F;
    if info.reason == HV_EXIT_REASON_EXCEPTION
        && ec == EC_DATA_ABORT_LOWER_EL
        && info.exception.physical_address == DOORBELL_GPA
    {
        return GuestExit::Doorbell;
    }
    let mut pc = 0u64;
    let _ = unsafe { hv_vcpu_get_reg(vcpu, HV_REG_PC, &raw mut pc) };
    panic!(
        "unexpected exit: reason={} ec={ec:#x} pa={:#x} pc={pc:#x}",
        info.reason, info.exception.physical_address
    );
}

fn read_reg(vcpu: u64, reg: u32) -> u64 {
    let mut v = 0u64;
    check(unsafe { hv_vcpu_get_reg(vcpu, reg, &raw mut v) }, "get reg");
    v
}

/// Arm the guest registers for one full touch pass and run to the doorbell.
pub fn run_touch_pass(vcpu: u64, exit: *const HvVcpuExitInfo, ram_size: usize) {
    run_touch_range(vcpu, exit, 0, ram_size);
}

/// One read-only pass over all of guest RAM: the control that separates
/// dirtying from touching.
pub fn run_read_pass(vcpu: u64, exit: *const HvVcpuExitInfo, ram_size: usize) {
    run_pass(vcpu, exit, READ_GPA, 0, ram_size);
}

/// Touch pass over a sub-range of guest RAM, `[start_off, end_off)`.
pub fn run_touch_range(vcpu: u64, exit: *const HvVcpuExitInfo, start_off: usize, end_off: usize) {
    run_pass(vcpu, exit, CODE_GPA, start_off, end_off);
}

/// One RMW sweep over `[start_off, end_off)`: every page's first word is
/// incremented once. Unlike [`run_hammer`] this returns after a single
/// sweep, so a caller can interleave sweeps with other work on the same
/// vCPU.
pub fn run_sweep_range(vcpu: u64, exit: *const HvVcpuExitInfo, start_off: usize, end_off: usize) {
    check(
        unsafe { hv_vcpu_set_reg(vcpu, HV_REG_X6, RAM_GPA + start_off as u64) },
        "set X6",
    );
    run_pass(vcpu, exit, HAMMER_GPA, start_off, end_off);
}

/// Stamp `value` into the first word of every page in `[start_off, end_off)`.
pub fn run_fill_range(
    vcpu: u64,
    exit: *const HvVcpuExitInfo,
    start_off: usize,
    end_off: usize,
    value: u64,
) {
    check(unsafe { hv_vcpu_set_reg(vcpu, HV_REG_X5, value) }, "set X5");
    run_pass(vcpu, exit, FILL_GPA, start_off, end_off);
}

/// Guest-side integrity check over `[start_off, end_off)`: returns how many
/// pages did NOT hold `expected` in their first word. The host never reads
/// the range, so the pages keep whatever host-side state is under test.
pub fn run_verify_range(
    vcpu: u64,
    exit: *const HvVcpuExitInfo,
    start_off: usize,
    end_off: usize,
    expected: u64,
) -> u64 {
    check(unsafe { hv_vcpu_set_reg(vcpu, HV_REG_X4, 0) }, "clear X4");
    check(
        unsafe { hv_vcpu_set_reg(vcpu, HV_REG_X5, expected) },
        "set X5",
    );
    run_pass(vcpu, exit, VERIFY_GPA, start_off, end_off);
    read_reg(vcpu, HV_REG_X4)
}

fn run_pass(vcpu: u64, exit: *const HvVcpuExitInfo, entry: u64, start_off: usize, end_off: usize) {
    check(unsafe { hv_vcpu_set_reg(vcpu, HV_REG_PC, entry) }, "set PC");
    check(
        unsafe { hv_vcpu_set_reg(vcpu, HV_REG_X1, RAM_GPA + start_off as u64) },
        "set X1",
    );
    check(
        unsafe { hv_vcpu_set_reg(vcpu, HV_REG_X2, RAM_GPA + end_off as u64) },
        "set X2",
    );
    check(
        unsafe { hv_vcpu_set_reg(vcpu, HV_REG_X3, DOORBELL_GPA) },
        "set X3",
    );
    loop {
        match run_to_doorbell(vcpu, exit) {
            GuestExit::Canceled => continue, // spurious: just re-enter
            GuestExit::Doorbell => return,
        }
    }
}

/// Run hammer sweeps until `keep_going(completed_sweeps)` says stop;
/// returns the number of completed sweeps. The vCPU is genuinely running
/// — faulting and writing — the whole time, which is what makes this a
/// race probe rather than a parked-guest measurement. Data aborts do not
/// auto-advance PC on this framework, so resuming steps past the
/// doorbell store by hand.
pub fn run_hammer(
    vcpu: u64,
    exit: *const HvVcpuExitInfo,
    ram_size: usize,
    mut keep_going: impl FnMut(u64) -> bool,
) -> u64 {
    check(
        unsafe { hv_vcpu_set_reg(vcpu, HV_REG_PC, HAMMER_GPA) },
        "set PC",
    );
    check(
        unsafe { hv_vcpu_set_reg(vcpu, HV_REG_X2, RAM_GPA + ram_size as u64) },
        "set X2",
    );
    check(
        unsafe { hv_vcpu_set_reg(vcpu, HV_REG_X3, DOORBELL_GPA) },
        "set X3",
    );
    check(unsafe { hv_vcpu_set_reg(vcpu, HV_REG_X4, 0) }, "set X4");
    check(
        unsafe { hv_vcpu_set_reg(vcpu, HV_REG_X6, RAM_GPA) },
        "set X6",
    );
    loop {
        match run_to_doorbell(vcpu, exit) {
            GuestExit::Canceled => continue,
            GuestExit::Doorbell => {
                let sweeps = read_reg(vcpu, HV_REG_X4);
                if !keep_going(sweeps) {
                    return sweeps;
                }
                let pc = read_reg(vcpu, HV_REG_PC);
                check(
                    unsafe { hv_vcpu_set_reg(vcpu, HV_REG_PC, pc + 4) },
                    "advance PC",
                );
            }
        }
    }
}
