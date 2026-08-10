//! Shows what real memory pressure does to `MADV_FREE` pages versus
//! plain dirty pages: the former are discarded (reads come back zero),
//! the latter are compressed and preserved.
//!
//! This is the control experiment behind the claim that macOS is
//! perfectly willing to throw away pages that are *marked* discardable —
//! the Virtualization.framework balloon path just never marks them.
//!
//! ```sh
//! cargo run --release -p pressure-discard
//! ```
//!
//! Pressure comes from this repo's own generator (`ledger::pressure`): a
//! child process dirties `--pressure-gb` GiB (default 48) and holds it
//! for a few seconds. Apple's `memory_pressure` tool is NOT used — its
//! `-S` simulation steals no pages at all, and even real `-l critical`
//! never allocated deep enough on a 128 GiB machine to touch our buffers
//! (both measured: exactly 0 pages reclaimed). Expect the machine to be
//! sluggish for ~15–30 s.

use ledger::{row, Ledger};

const SIZE: usize = 1 << 30; // 1 GiB per buffer
const PAGE: usize = 16 * 1024;
const PATTERN: u64 = 0xC0DE_D00D_FEED_FACE;
const HOLD_SECS: u64 = 8;

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

fn fill(ptr: *mut u8, size: usize) {
    for off in (0..size).step_by(PAGE) {
        // SAFETY: off < size, mapping is writable.
        unsafe { ptr.add(off).cast::<u64>().write(PATTERN ^ off as u64) };
    }
}

/// Count pages whose marker survived (non-zero) vs were discarded to zero.
fn survey(ptr: *const u8, size: usize) -> (usize, usize) {
    let (mut intact, mut zeroed) = (0, 0);
    for off in (0..size).step_by(PAGE) {
        // SAFETY: off < size, mapping is readable.
        let v = unsafe { ptr.add(off).cast::<u64>().read() };
        if v == PATTERN ^ off as u64 {
            intact += 1;
        } else if v == 0 {
            zeroed += 1;
        }
    }
    (intact, zeroed)
}

fn main() {
    ledger::pressure::maybe_run_generator();

    let args: Vec<String> = std::env::args().collect();
    let pressure_gb: usize = args
        .iter()
        .position(|a| a == "--pressure-gb")
        .and_then(|i| args.get(i + 1))
        .map(|v| v.parse().expect("--pressure-gb N"))
        .unwrap_or(48);

    println!("pressure-discard: two 1 GiB dirty buffers; A gets MADV_FREE, B stays dirty\n");

    let a = mmap_anon(SIZE);
    let b = mmap_anon(SIZE);
    fill(a, SIZE);
    fill(b, SIZE);

    let rc = unsafe { libc::madvise(a.cast(), SIZE, libc::MADV_FREE) };
    assert_eq!(rc, 0, "madvise(MADV_FREE) failed");

    row("before pressure", &Ledger::read());

    println!("\ngenerating pressure: dirtying {pressure_gb} GiB in a child, then holding {HOLD_SECS}s ...\n");
    let guard = ledger::pressure::apply(pressure_gb).expect("pressure generator");
    std::thread::sleep(std::time::Duration::from_secs(HOLD_SECS));
    drop(guard);

    // Let the pageout thread settle before surveying.
    std::thread::sleep(std::time::Duration::from_secs(2));
    row("after pressure", &Ledger::read());

    let (a_intact, a_zeroed) = survey(a, SIZE);
    let (b_intact, b_zeroed) = survey(b, SIZE);
    let pages = SIZE / PAGE;

    println!();
    println!("buffer A (MADV_FREE):  {a_intact:>5}/{pages} pages intact, {a_zeroed:>5} discarded to zero");
    println!("buffer B (plain dirty): {b_intact:>5}/{pages} pages intact, {b_zeroed:>5} discarded to zero");
    println!(
        "\nReading: pressure discards correctly-marked pages (A) while dirty\n\
         pages (B) survive — compressed, still charged to you. A VM balloon\n\
         that never marks surrendered pages leaves them all in class B."
    );

    unsafe {
        libc::munmap(a.cast(), SIZE);
        libc::munmap(b.cast(), SIZE);
    }
}
