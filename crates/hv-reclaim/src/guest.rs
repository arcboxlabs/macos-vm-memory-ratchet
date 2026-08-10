//! The guest programs and the host loops that drive them.

use crate::hvf::{
    check, hv_vcpu_get_reg, hv_vcpu_run, hv_vcpu_set_reg, HvVcpuExitInfo, EC_DATA_ABORT_LOWER_EL,
    HV_EXIT_REASON_CANCELED, HV_EXIT_REASON_EXCEPTION, HV_REG_PC, HV_REG_X1, HV_REG_X2, HV_REG_X3,
    HV_REG_X4, HV_REG_X6,
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

const HAMMER_OFF: usize = 0x80;
const HAMMER_GPA: u64 = CODE_GPA + HAMMER_OFF as u64;

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
