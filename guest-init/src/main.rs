//! PID 1 of the vz-ratchet guest: a tiny command server on the virtio
//! console. The host drives it line-by-line over serial:
//!
//! ```text
//! touch N   ->  RATCHET TOUCHED N   (mmap N GiB anon, write every 4 KiB)
//! free      ->  RATCHET FREED       (munmap everything held)
//! mem       ->  RATCHET MEM <kib>   (MemAvailable from /proc/meminfo)
//! ```
//!
//! It prints `RATCHET READY` once the console is open. Kernel log lines
//! share the same serial channel; the host filters on the `RATCHET `
//! prefix.

use std::ffi::CString;
use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, Write};
use std::time::Duration;

const GIB: usize = 1 << 30;
/// 4 KiB stride covers every host 16 KiB page for any guest page size.
const STRIDE: usize = 4096;

fn mount(fstype: &str, dir: &str) {
    let _ = std::fs::create_dir_all(dir);
    let fs = CString::new(fstype).unwrap();
    let d = CString::new(dir).unwrap();
    // SAFETY: valid NUL-terminated strings for source, target, and type.
    let rc = unsafe { libc::mount(fs.as_ptr(), d.as_ptr(), fs.as_ptr(), 0, std::ptr::null()) };
    if rc != 0 {
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() != Some(libc::EBUSY) {
            panic!("mount {fstype} on {dir}: {err}");
        }
    }
}

fn open_console(write: bool) -> std::fs::File {
    // The virtio console node appears once the driver binds; retry briefly.
    for _ in 0..100 {
        if let Ok(f) = OpenOptions::new()
            .read(!write)
            .write(write)
            .open("/dev/hvc0")
        {
            return f;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    panic!("/dev/hvc0 never appeared");
}

fn mem_available_kib() -> u64 {
    let meminfo = std::fs::read_to_string("/proc/meminfo").expect("read /proc/meminfo");
    for line in meminfo.lines() {
        if let Some(rest) = line.strip_prefix("MemAvailable:") {
            return rest
                .trim()
                .trim_end_matches("kB")
                .trim()
                .parse()
                .expect("parse MemAvailable");
        }
    }
    panic!("MemAvailable not in /proc/meminfo");
}

fn main() {
    mount("devtmpfs", "/dev");
    mount("proc", "/proc");

    let mut out = open_console(true);
    let input = open_console(false);
    let mut held: Vec<(*mut libc::c_void, usize)> = Vec::new();

    writeln!(out, "RATCHET READY").unwrap();
    for line in BufReader::new(input).lines() {
        let line = line.unwrap();
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let reply = if let Some(n) = line.strip_prefix("touch ") {
            let gib: usize = n.trim().parse().unwrap_or(0);
            let size = gib * GIB;
            // SAFETY: fresh anonymous mapping request.
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
            assert!(ptr != libc::MAP_FAILED, "guest mmap({gib} GiB) failed");
            for off in (0..size).step_by(STRIDE) {
                // SAFETY: off < size, mapping is writable.
                unsafe { ptr.cast::<u8>().add(off).write(0xA5) };
            }
            held.push((ptr, size));
            format!("TOUCHED {gib}")
        } else if line == "free" {
            for (ptr, size) in held.drain(..) {
                // SAFETY: exactly the mapping created above.
                unsafe { libc::munmap(ptr, size) };
            }
            "FREED".to_string()
        } else if line == "mem" {
            format!("MEM {}", mem_available_kib())
        } else {
            format!("ERR unknown command: {line}")
        };
        writeln!(out, "RATCHET {reply}").unwrap();
    }
}
