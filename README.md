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
| `hv-reclaim` | none | A live Hypervisor.framework VM whose host reclaims guest RAM: `hv_vm_unmap → madvise(MADV_FREE_REUSABLE) → hv_vm_map`, footprint −3 GiB, VM keeps running. Safety probes: `--pressure-check` (guest parked under pressure) and `--hammer` (guest writes racing the pageout scan). |
| `vz-ratchet` | none | The Virtualization.framework side, live: a real Linux guest touches N GiB (helper footprint +N), frees it (footprint unmoved — the ratchet), the balloon inflates (guest visibly starves, footprint unmoved — the placebo), and under real pressure the surrendered pages are compressed, not discarded, while an `MADV_FREE` canary dies. |

```sh
cargo run --release -p calibrate-madvise
cargo run --release -p pressure-discard      # expect ~30s of system sluggishness
./run.sh                          # hv-reclaim, the fix
./run.sh --naive                  # hv-reclaim, the trap (see below)
./run.sh --advice free            # what libkrun ships (ledger-flat)
./run.sh --repeat 5               # variance of the reclaim cycle
./run.sh --pressure-check --pressure-gb 12   # parked safety probe
./run.sh --hammer --pressure-gb 12           # concurrent-write race probe
./run-vz.sh                       # vz-ratchet: ratchet + balloon placebo
./run-vz.sh --guest-gb 4 --touch-gb 3 --pressure-gb 12   # + discrimination
```

`vz-ratchet` boots a pinned ArcBox kernel (fetched once from the public
boot CDN; any VZ-bootable arm64 kernel with virtio-console and
virtio-balloon built in works via `VZ_RATCHET_KERNEL`), a ~400 KiB
initramfs whose `/init` is a static Rust binary driven over serial, and
samples Apple's `com.apple.Virtualization.VirtualMachine` XPC helper —
the process that owns guest RAM on VZ — with `proc_pid_rusage`, which
needs **no root** even against Apple's hardened helper.

`phys_footprint` is read via `task_info(TASK_VM_INFO)` — the same ledger
macOS pressure decisions use. Every pressure run arms a 1 GiB
`MADV_FREE` **canary** as its positive control: if pressure never
discards the canary (see the file-cache moat below), the run reports
itself INCONCLUSIVE instead of printing a vacuous all-clear.

## Results

Two machines, labeled throughout:

- **A** — 128 GiB M-series desktop, macOS 26.4
- **B** — 16 GiB M4 Mac mini, macOS 26.3.1

Ledger effects replicate identically on both. Pressure runs are
conclusive only on B — on A the canary survives behind the file-cache
moat and the probes self-report INCONCLUSIVE.

`calibrate-madvise`, 1 GiB dirty anonymous memory (A and B, identical):

| advice | Δfootprint on advise |
|---|---|
| `MADV_DONTNEED` | ±0 |
| `MADV_FREE` | ±0 |
| `MADV_FREE_REUSABLE` | **−1024 MiB** |

`hv-reclaim`, 3 GiB guest RAM dirtied by a vCPU, then reclaimed (A and
B, identical; `--repeat 5` gives mean −3073.5 MiB with min = max — the
ledger step is a deterministic VM operation, not a noisy average):

| sequence | Δfootprint on reclaim |
|---|---|
| `madvise(REUSABLE)` while stage-2 mapped (`--naive`) | **±0 — silent no-op** |
| `hv_vm_unmap → madvise(REUSABLE) → hv_vm_map` | **−3073 MiB** |

Safety probes (machine B, 12 GiB pressure; the canary was conclusive —
all 65536/65536 canary pages discarded — in every run):

| run | guest during pressure | guest data after |
|---|---|---|
| `--pressure-check` | parked at the doorbell | **196608/196608 pages intact** |
| `--hammer` | RMW-incrementing every page — 9155 full sweeps (~1.8 billion stores) racing the scan through build-up, hold, and release | **196608/196608 pages at the exact expected counter, 0 lost** |
| `--naive --pressure-check` | parked | **196608/196608 intact; reusable 0 MiB, 273 MiB compressed** — the naive no-op is a *true* no-op: pages stayed in the protected dirty class, they were never lazily armed for discard |

### Pressure findings

Getting macOS to *actually steal* pages turned out to be its own result:

- `memory_pressure -S -l critical` (the simulator) steals **zero** pages —
  it only posts notifications. Any experiment built on `-S` is
  measuring nothing.
- `memory_pressure -l critical` (real mode) and a 48 GiB dirty-and-hold
  generator both also reclaimed **zero** of our `MADV_FREE` pages on
  machine A — the entire allocation was absorbed by free memory and
  ~56 GiB of file cache; the compressor and swap counters did not
  move. **Anon reclaim begins only after the file-cache slack is
  drained**, so on big-RAM machines the threshold is enormous. (This is
  what the canary exists to catch.)
- On machine B (~7 GiB slack), a 12 GiB generator run punches through,
  and the picture is textbook: the 1 GiB `MADV_FREE` buffer was
  discarded **65536/65536 pages to zero**, while the plain dirty control
  survived intact — compressed to 1006 MiB and still charged. macOS is
  perfectly willing to throw away correctly-marked pages; nothing on the
  Virtualization.framework path ever marks them.
- **The rogue-page question is answered in both forms.** After
  unmap → `MADV_FREE_REUSABLE` → remap and a full guest re-dirty (3 GiB
  of live data sitting sticky-marked reusable), pressure deep enough to
  annihilate the canary lost **0 of 196608 pages** — with the guest
  parked (`--pressure-check`), and with the guest actively
  read-modify-writing every page while the scan ran (`--hammer`: 9155
  sweeps, every page's counter exact at the end, so a discard at *any*
  moment of the run would have shown). The ledger shows the scanner
  un-marking what the guest re-dirtied (hammer run: reusable
  3072 → 2061 MiB, footprint 3 → 1015 MiB) and compressing it like any
  live data. Guest writes through stage-2 evidently reach the pmap dirty
  state the scan consults before discarding. Pair re-exposure with
  `MADV_FREE_REUSE` for prompt accounting, not for correctness.

## Why "placebo" and not "lazy reclaim"

A footprint-flat balloon could still be doing something real — this
repo's own findings show `MADV_FREE` is footprint-flat yet genuinely
discardable, so the ledger alone cannot convict the balloon. The
distinguishing experiment must put both hypotheses under real pressure,
and `vz-ratchet` runs it end to end (machine B, 4 GiB guest, 3 GiB
touched, 12 GiB pressure):

| step | guest `MemAvailable` | helper footprint |
|---|---|---|
| VM booted, guest idle | — | 205 MiB |
| guest touches 3 GiB | — | 3284 MiB |
| guest frees every byte | 3705 MiB | 3284 MiB — **the ratchet** |
| balloon inflates 3 GiB | **639 MiB** — the guest really handed the pages over | 3284 MiB — **the placebo** |
| 12 GiB real pressure | — | **3285 MiB (−0.0)** while the 1 GiB `MADV_FREE` canary is discarded 65536/65536 and helper *resident* drops ~600 MiB (compressed, still charged) |

The balloon fails both tests: no accounting release when it inflates,
*and* no change of reclaim class — pressure deep enough to annihilate
every correctly-marked control page compresses the surrendered pages
instead of discarding them. That is a placebo, not deferred reclaim.
(The larger original measurement — 15.35 GB ballooned on a 16 GiB
guest, byte-identical footprint, macOS 26.4 — is in the blog post; this
repo's version is the turnkey reproduction.)

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

2. **Reusable state outlives the reclaim — and that holds up under
   concurrent writes.** After the unmap → advise → remap sequence, pages
   the guest faults back in and re-dirties are *born unmetered*:
   resident climbs back to 3 GiB while `phys_footprint` stays near zero
   and the `reusable` counter absorbs the difference. This stickiness is
   xnu's lazy ledger — it applies to host writes too, and
   `vm_pageout_scan`'s rogue-page fix-up un-marks a written page before
   any reclaim decision. Whether *guest* writes through stage-2 reach
   that dirty state was the open safety question; `--pressure-check`
   (parked) and `--hammer` (writes racing the scan) both answered it
   positively on real hardware — see the safety-probe table above.

   Two limits worth stating plainly. The protection rests on scan-time
   behavior of a private advice flag with no documented contract,
   observed on two OS builds — treat it as measured behavior, not an API
   guarantee. And once remapped, guest refaults are invisible to the
   VMM (stage-2 faults are handled entirely by xnu), so there is no
   host-side hook from which to issue `MADV_FREE_REUSE`: the sticky
   accounting state is permanent by construction, which is exactly why
   the scan-time protection had to be tested this hard.

## What is deliberately not here (yet)

- **libkrun comparison** — libkrun's balloon handles guest free-page
  reports with plain `madvise(MADV_FREE)` on macOS
  ([source](https://github.com/libkrun/libkrun/blob/main/src/devices/src/virtio/balloon/device.rs)),
  which per `calibrate-madvise` never moves the ledger — pressure
  relief, not visible reclaim. (An independent balloon reproduction
  also exists at
  [thewesjohnson/macos-virtio-balloon-test](https://github.com/thewesjohnson/macos-virtio-balloon-test).)

## License

MIT OR Apache-2.0.
