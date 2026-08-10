# macos-vm-memory-ratchet

Reproductions and counter-demonstrations for the ArcBox blog post
[*The Balloon Is a Placebo: No Container Runtime Can Give Your Mac's RAM
Back*](https://arcbox.dev/blog/macos-vm-memory-ratchet).

The claims: on macOS, a Linux VM's host cost is the high-water mark of
guest-touched pages; Virtualization.framework's balloon device releases
nothing host-side; and the fix exists one API layer down, on
Hypervisor.framework, where guest RAM belongs to your own process.

Everything here runs on any Apple Silicon Mac. No SIP changes, no
Developer ID — the hypervisor demo is ad-hoc signed.

## The demos

| Demo | Privileges | Shows |
|---|---|---|
| `calibrate-madvise` | none | What each `madvise` advice does to `phys_footprint`: `MADV_DONTNEED` and `MADV_FREE` do nothing; only `MADV_FREE_REUSABLE` moves the ledger. |
| `pressure-discard` | none | What real pressure does to `MADV_FREE` pages vs plain dirty pages — and how deep pressure has to go before it touches either (see findings). |
| `hv-reclaim` | none | A live Hypervisor.framework VM whose host reclaims guest RAM: `hv_vm_unmap → madvise(MADV_FREE_REUSABLE) → hv_vm_map`, footprint −3 GiB, VM keeps running. |

```sh
cargo run --release -p calibrate-madvise
cargo run --release -p pressure-discard   # expect ~30s of system sluggishness
./run.sh                     # hv-reclaim, the fix
./run.sh --naive             # hv-reclaim, the trap (see below)
./run.sh --advice free       # what libkrun ships (ledger-flat)
```

Measured on macOS 26.4, Apple Silicon (M-series). `phys_footprint` is
read via `task_info(TASK_VM_INFO)` — the same ledger macOS pressure
decisions use.

## Results (2026-08-10, macOS 26.4)

`calibrate-madvise`, 1 GiB dirty anonymous memory:

| advice | Δfootprint on advise |
|---|---|
| `MADV_DONTNEED` | ±0 |
| `MADV_FREE` | ±0 |
| `MADV_FREE_REUSABLE` | **−1024 MiB** |

`hv-reclaim`, 3 GiB guest RAM dirtied by a vCPU, then reclaimed:

| sequence | Δfootprint on reclaim |
|---|---|
| `madvise(REUSABLE)` while stage-2 mapped (`--naive`) | **±0 — silent no-op** |
| `hv_vm_unmap → madvise(REUSABLE) → hv_vm_map` | **−3073 MiB** |

### Pressure findings

Getting macOS to *actually steal* pages turned out to be its own result:

- `memory_pressure -S -l critical` (the simulator) steals **zero** pages —
  it only posts notifications. Any experiment built on `-S` is
  measuring nothing.
- `memory_pressure -l critical` (real mode) and a 48 GiB dirty-and-hold
  generator both also reclaimed **zero** of our `MADV_FREE` pages on a
  128 GiB machine — the entire allocation was absorbed by free memory
  and ~56 GiB of file cache; the compressor and swap counters did not
  move. **Anon reclaim begins only after the file-cache slack is
  drained**, so on big-RAM machines the threshold is enormous.
- On a 16 GiB M4 (macOS 26.3.1, ~7 GiB slack), a 12 GiB generator run
  punches through, and the picture is textbook: the 1 GiB `MADV_FREE`
  buffer was discarded **65536/65536 pages to zero**, while the plain
  dirty control survived intact — compressed to 1006 MiB and still
  charged. macOS is perfectly willing to throw away correctly-marked
  pages; nothing on the Virtualization.framework path ever marks them.
- **The rogue-page question is answered: guest writes through stage-2
  reach `pmap_get_refmod`.** After unmap → `MADV_FREE_REUSABLE` → remap
  and a full guest re-dirty (3 GiB of live data sitting sticky-marked
  reusable), the same pressure that annihilated the `MADV_FREE` buffer
  lost **0 of 196608 pages** — the scanner un-marked the re-dirtied
  pages (reusable 3072 → 2131 MiB, footprint 3.2 → 944 MiB) and
  compressed them like any live data. The eager-remap reclaim sequence
  is therefore safe for free-page-reporting semantics; pair re-exposure
  with `MADV_FREE_REUSE` for prompt accounting, not for correctness.

## Findings beyond the blog post

Building the counter-demonstration surfaced two facts we have not seen
documented anywhere:

1. **The guest-dirty trap.** `madvise(MADV_FREE_REUSABLE)` on a range
   that is still `hv_vm_map`'d is a silent no-op *for pages the guest
   dirtied through stage-2* (returns 0, ledger unmoved). Pages the host
   process dirtied reclaim fine under the identical call
   (`--host-touch`) — so a host-only calibration reports the API as
   working and ships a reclaim path that reclaims nothing. The stage-2
   mapping must be torn down around the `madvise`.

2. **Reusable state outlives the reclaim — and that turns out to be
   safe.** After the unmap → advise → remap sequence, pages the guest
   faults back in and re-dirties are *born unmetered*: resident climbs
   back to 3 GiB while `phys_footprint` stays near zero and the
   `reusable` counter absorbs the difference. This stickiness is xnu's
   lazy ledger — it applies to host writes too, and `vm_pageout_scan`'s
   rogue-page fix-up un-marks a written page (via `pmap_get_refmod`)
   before any reclaim decision. Whether *guest* writes through stage-2
   reach `pmap_get_refmod` was the open safety question;
   `./run.sh --pressure-check` answered it **positively** on real
   hardware (see the pressure findings above): 0 pages of guest data
   lost under pressure deep enough to discard every `MADV_FREE` control
   page, with the ledger showing the scanner un-marking exactly the
   re-dirtied range.

## What is deliberately not here (yet)

- **`vz-ratchet`** — the Virtualization.framework side (alloc/free in a
  real Linux guest, footprint sampled on Apple's XPC helper; balloon
  inflation showing a byte-identical footprint). Needs a guest kernel
  and `sudo footprint`; an independent reproduction already exists at
  [thewesjohnson/macos-virtio-balloon-test](https://github.com/thewesjohnson/macos-virtio-balloon-test).
- **libkrun comparison** — libkrun's balloon handles guest free-page
  reports with plain `madvise(MADV_FREE)` on macOS
  ([source](https://github.com/libkrun/libkrun/blob/main/src/devices/src/virtio/balloon/device.rs)),
  which per `calibrate-madvise` never moves the ledger — pressure
  relief, not visible reclaim.

## License

MIT OR Apache-2.0.
