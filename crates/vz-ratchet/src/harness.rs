//! Driving the Swift VZ harness over its stdio line protocol.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

pub struct Harness {
    child: Child,
    stdin: ChildStdin,
    lines: Receiver<String>,
}

impl Harness {
    pub fn spawn(
        harness: &str,
        kernel: &str,
        initramfs: &str,
        memory_mib: usize,
        verbose: bool,
    ) -> Self {
        let mut child = Command::new(harness)
            .args([kernel, initramfs, &memory_mib.to_string()])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("spawn vz harness");
        let stdin = child.stdin.take().expect("piped stdin");
        let stdout = child.stdout.take().expect("piped stdout");
        let (tx, lines) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                let line = line.trim_end_matches('\r').to_string();
                // Protocol lines are always shown; kernel spew only with -v.
                if verbose || line.starts_with("RATCHET ") || line.starts_with("HARNESS ") {
                    println!("| {line}");
                }
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        Self {
            child,
            stdin,
            lines,
        }
    }

    pub fn send(&mut self, cmd: &str) {
        writeln!(self.stdin, "{cmd}").expect("write to harness");
    }

    /// Wait for a line starting with `prefix`; returns the rest of it.
    pub fn wait(&self, prefix: &str, timeout: Duration) -> String {
        let deadline = Instant::now() + timeout;
        loop {
            let remaining = deadline
                .checked_duration_since(Instant::now())
                .unwrap_or_else(|| panic!("timed out waiting for {prefix:?}"));
            match self.lines.recv_timeout(remaining) {
                Ok(line) => {
                    if let Some(rest) = line.strip_prefix(prefix) {
                        return rest.trim().to_string();
                    }
                }
                Err(RecvTimeoutError::Timeout) => panic!("timed out waiting for {prefix:?}"),
                Err(RecvTimeoutError::Disconnected) => {
                    panic!("harness exited while waiting for {prefix:?}")
                }
            }
        }
    }

    /// Ask the guest for `MemAvailable`, in KiB.
    pub fn guest_mem_available_kib(&mut self) -> u64 {
        self.send("guest mem");
        self.wait("RATCHET MEM", Duration::from_secs(15))
            .parse()
            .expect("parse guest MemAvailable")
    }

    pub fn quit(mut self) {
        self.send("quit");
        let _ = self.child.wait();
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
