//! Raw Hypervisor.framework FFI (aarch64) — just what the demo needs.

use std::ffi::c_void;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct HvVcpuExitException {
    pub syndrome: u64,
    pub virtual_address: u64,
    pub physical_address: u64,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct HvVcpuExitInfo {
    pub reason: u32,
    pub exception: HvVcpuExitException,
}

#[link(name = "Hypervisor", kind = "framework")]
unsafe extern "C" {
    pub fn hv_vm_create(config: *mut c_void) -> i32;
    pub fn hv_vm_destroy() -> i32;
    pub fn hv_vm_map(addr: *mut u8, ipa: u64, size: usize, flags: u64) -> i32;
    pub fn hv_vm_unmap(ipa: u64, size: usize) -> i32;
    pub fn hv_vcpu_create(
        vcpu: *mut u64,
        exit: *mut *const HvVcpuExitInfo,
        config: *mut c_void,
    ) -> i32;
    pub fn hv_vcpu_destroy(vcpu: u64) -> i32;
    pub fn hv_vcpu_run(vcpu: u64) -> i32;
    pub fn hv_vcpu_set_reg(vcpu: u64, reg: u32, value: u64) -> i32;
    pub fn hv_vcpu_get_reg(vcpu: u64, reg: u32, value: *mut u64) -> i32;
}

pub const HV_MEMORY_READ: u64 = 1 << 0;
pub const HV_MEMORY_WRITE: u64 = 1 << 1;
pub const HV_MEMORY_EXEC: u64 = 1 << 2;

pub const HV_REG_X1: u32 = 1;
pub const HV_REG_X2: u32 = 2;
pub const HV_REG_X3: u32 = 3;
pub const HV_REG_X4: u32 = 4;
pub const HV_REG_X6: u32 = 6;
pub const HV_REG_PC: u32 = 31;
pub const HV_REG_CPSR: u32 = 34;

pub const HV_EXIT_REASON_CANCELED: u32 = 0;
pub const HV_EXIT_REASON_EXCEPTION: u32 = 1;
pub const EC_DATA_ABORT_LOWER_EL: u64 = 0x24;

/// EL1h with A/I/F/D masked — how a bare-metal guest starts.
pub const PSTATE_EL1H_MASKED: u64 = 0x3C5;

pub fn check(ret: i32, what: &str) {
    assert_eq!(ret, 0, "{what} failed: {ret:#x}");
}
