//! Finding and sampling Apple's VM XPC helper.
//!
//! On Virtualization.framework the guest RAM does not live in the app's
//! process — it lives in `com.apple.Virtualization.VirtualMachine`, a
//! per-VM XPC helper spawned on `VZVirtualMachine.start`. That helper is
//! the process the ratchet charges, so it is the process we sample.
//!
//! `proc_pid_rusage` reads another same-uid process's `phys_footprint`
//! without root (verified against Apple's hardened helper) — no
//! `sudo footprint` needed.

use std::collections::HashSet;
use std::process::Command;
use std::time::{Duration, Instant};

const HELPER_PATTERN: &str = "com.apple.Virtualization.VirtualMachine";
const RUSAGE_INFO_V2: libc::c_int = 2;

/// `struct rusage_info_v2` from `<libproc.h>` — stable ABI.
#[repr(C)]
#[derive(Default)]
struct RusageInfoV2 {
    uuid: [u8; 16],
    user_time: u64,
    system_time: u64,
    pkg_idle_wkups: u64,
    interrupt_wkups: u64,
    pageins: u64,
    wired_size: u64,
    resident_size: u64,
    phys_footprint: u64,
    proc_start_abstime: u64,
    proc_exit_abstime: u64,
    child_user_time: u64,
    child_system_time: u64,
    child_pkg_idle_wkups: u64,
    child_interrupt_wkups: u64,
    child_pageins: u64,
    child_elapsed_abstime: u64,
    diskio_bytesread: u64,
    diskio_byteswritten: u64,
}

unsafe extern "C" {
    fn proc_pid_rusage(
        pid: libc::c_int,
        flavor: libc::c_int,
        buffer: *mut libc::c_void,
    ) -> libc::c_int;
}

/// One footprint/resident reading of the helper, in bytes.
#[derive(Clone, Copy)]
pub struct HelperLedger {
    pub phys_footprint: u64,
    pub resident: u64,
}

pub fn sample(pid: i32) -> HelperLedger {
    let mut info = RusageInfoV2::default();
    // SAFETY: buffer is a valid rusage_info_v2 and the flavor matches.
    let rc = unsafe { proc_pid_rusage(pid, RUSAGE_INFO_V2, (&raw mut info).cast()) };
    assert_eq!(
        rc,
        0,
        "proc_pid_rusage({pid}) failed: {} (helper gone?)",
        std::io::Error::last_os_error()
    );
    HelperLedger {
        phys_footprint: info.phys_footprint,
        resident: info.resident_size,
    }
}

/// Print a labeled helper-ledger row, `ledger::row`-style.
pub fn row(label: &str, l: &HelperLedger) {
    println!(
        "{label:<40} helper footprint {:>9.1} MiB   resident {:>9.1} MiB",
        ledger::mib(l.phys_footprint),
        ledger::mib(l.resident),
    );
}

/// Pids of every VM helper currently running (other VMs on the machine).
pub fn running_helpers() -> HashSet<i32> {
    let out = Command::new("pgrep")
        .args(["-f", HELPER_PATTERN])
        .output()
        .expect("pgrep");
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.trim().parse().ok())
        .collect()
}

/// Wait for the helper that did not exist before our VM started.
pub fn find_new_helper(pre_existing: &HashSet<i32>, timeout: Duration) -> i32 {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if let Some(&pid) = running_helpers().difference(pre_existing).next() {
            return pid;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    panic!("no new {HELPER_PATTERN} process appeared within {timeout:?}");
}
