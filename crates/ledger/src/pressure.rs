//! A deterministic memory-pressure generator.
//!
//! Apple's `memory_pressure` tool turned out to be useless for these
//! experiments: `-S` simulation only posts notifications (measured: zero
//! pages stolen), and even the real `-l critical` mode never allocated
//! deep enough on a large-RAM machine to make the pageout scan touch our
//! buffers. So we generate pressure the way the original ArcBox
//! calibration did: a child process dirties tens of GiB of anonymous
//! memory and holds it, forcing the kernel to find memory elsewhere —
//! e.g. by discarding correctly-marked reclaimable pages of the parent.
//!
//! The child is this same binary, re-executed with `RATCHET_PRESSURE_GEN_GB`
//! set. Call [`maybe_run_generator`] first thing in `main`.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};

const GEN_ENV: &str = "RATCHET_PRESSURE_GEN_GB";
const PAGE: usize = 16 * 1024;

/// If this process was spawned as a pressure child, run the generator and
/// never return: dirty N GiB, print `HOLDING`, then sleep until killed.
pub fn maybe_run_generator() {
    let Ok(gb) = std::env::var(GEN_ENV) else {
        return;
    };
    let gb: usize = gb.parse().expect("pressure GiB");
    let chunk = 1usize << 30;
    let mut held = Vec::new();
    for i in 0..gb {
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                chunk,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_ANON | libc::MAP_PRIVATE,
                -1,
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            eprintln!("pressure-gen: mmap failed at {i} GiB, holding what we have");
            break;
        }
        for off in (0..chunk).step_by(PAGE) {
            // SAFETY: off < chunk, mapping is writable.
            unsafe {
                ptr.cast::<u8>()
                    .add(off)
                    .cast::<u64>()
                    .write(0xDEAD_0000 + off as u64)
            };
        }
        held.push(ptr);
        eprintln!("pressure-gen: {} GiB dirtied", i + 1);
    }
    println!("HOLDING");
    std::io::stdout().flush().ok();
    loop {
        std::thread::sleep(std::time::Duration::from_secs(1));
    }
}

/// A running pressure child; dropping it releases all pressure memory.
pub struct PressureGuard(Child);

impl Drop for PressureGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Spawn the pressure child and block until it reports `HOLDING` (all
/// `gb` GiB dirtied and resident/compressed somewhere).
pub fn apply(gb: usize) -> std::io::Result<PressureGuard> {
    let exe = std::env::current_exe()?;
    let mut child = Command::new(exe)
        .env(GEN_ENV, gb.to_string())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()?;
    let stdout = child.stdout.take().expect("piped stdout");
    let mut lines = BufReader::new(stdout).lines();
    match lines.next() {
        Some(Ok(line)) if line == "HOLDING" => Ok(PressureGuard(child)),
        other => {
            let _ = child.kill();
            let _ = child.wait();
            Err(std::io::Error::other(format!(
                "pressure child did not reach HOLDING: {other:?}"
            )))
        }
    }
}
