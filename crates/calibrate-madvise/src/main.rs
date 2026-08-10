//! Calibrates what each `madvise` advice actually does to the macOS
//! memory-pressure ledger (`phys_footprint`), on dirty anonymous memory.
//!
//! No privileges required. Run it on any Apple Silicon Mac:
//!
//! ```sh
//! cargo run --release -p calibrate-madvise
//! ```
//!
//! Expected result (macOS 26, Apple Silicon): `MADV_DONTNEED` and
//! `MADV_FREE` leave the footprint unchanged; only `MADV_FREE_REUSABLE`
//! moves it — the pages show up in the `reusable` counter instead.

use ledger::{delta_mib, row, Ledger};

const SIZE: usize = 1 << 30; // 1 GiB
const PAGE: usize = 16 * 1024; // Apple Silicon host page

struct Advice {
    name: &'static str,
    value: i32,
}

const ADVICES: &[Advice] = &[
    Advice {
        name: "MADV_DONTNEED",
        value: libc::MADV_DONTNEED,
    },
    Advice {
        name: "MADV_FREE",
        value: libc::MADV_FREE,
    },
    Advice {
        name: "MADV_FREE_REUSABLE",
        value: libc::MADV_FREE_REUSABLE,
    },
];

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

fn touch(ptr: *mut u8, size: usize) {
    for off in (0..size).step_by(PAGE) {
        // SAFETY: off < size, mapping is PAGE-aligned and writable.
        unsafe {
            ptr.add(off)
                .cast::<u64>()
                .write(off as u64 ^ 0xA5A5_A5A5_A5A5_A5A5)
        };
    }
}

fn main() {
    println!("calibrate-madvise: 1 GiB dirty anonymous memory per advice\n");

    let mut summary = Vec::new();

    for advice in ADVICES {
        let before = Ledger::read();
        let ptr = mmap_anon(SIZE);
        touch(ptr, SIZE);
        let touched = Ledger::read();

        let rc = unsafe { libc::madvise(ptr.cast(), SIZE, advice.value) };
        assert_eq!(
            rc,
            0,
            "madvise({}) failed: {}",
            advice.name,
            std::io::Error::last_os_error()
        );
        // The ledger update is synchronous, but give the VM subsystem a beat.
        std::thread::sleep(std::time::Duration::from_millis(200));
        let advised = Ledger::read();

        println!("== {} ==", advice.name);
        row("  after touching 1 GiB", &touched);
        row("  after madvise", &advised);
        println!();

        summary.push((
            advice.name,
            delta_mib(touched.phys_footprint, before.phys_footprint),
            delta_mib(advised.phys_footprint, touched.phys_footprint),
            delta_mib(advised.reusable, touched.reusable),
        ));

        let rc = unsafe { libc::munmap(ptr.cast(), SIZE) };
        assert_eq!(rc, 0, "munmap failed");
    }

    println!("advice                 touch Δfootprint   advise Δfootprint   advise Δreusable");
    for (name, dt, da, dr) in summary {
        println!("{name:<22} {dt:>+13.1} MiB   {da:>+14.1} MiB   {dr:>+13.1} MiB");
    }
    println!(
        "\nReading: an advice \"works\" for VM memory reclaim only if its\n\
         advise Δfootprint is ≈ -1024 MiB. Anything else means the host\n\
         still charges you for memory the guest has given back."
    );
}
