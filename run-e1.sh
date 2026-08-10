#!/bin/sh
# The reclaim-cost matrix (E1): both reclaim modes across extent sizes,
# plus the steady-state regime at free-page-reporting-plausible
# granularity. One log per config into $1 (default data-e1/), with a
# manifest of machine, OS, and harness commit.
#
# 0 in the extent list means the whole range as a single extent. 4 KiB
# does not appear: the host page is 16 KiB, so sub-page extents cannot
# exist on this platform.
set -eu
cd "$(dirname "$0")"
out="${1:-data-e1}"
mkdir -p "$out"

cargo build --release -p hv-reclaim
codesign --force -s - --entitlements crates/hv-reclaim/hv.entitlements target/release/hv-reclaim

size="${E1_SIZE_GB:-3}"
repeat="${E1_REPEAT:-5}"

{
    date -u
    sw_vers
    sysctl -n machdep.cpu.brand_string hw.memsize
    echo "harness $(git rev-parse HEAD)"
    echo "size_gb=$size repeat=$repeat"
} > "$out/manifest.txt"

for mode in reusable munmap; do
    for ext in 0 32768 2048 256 64 16; do
        label="$mode-ext$ext"
        echo "== $label"
        target/release/hv-reclaim --time-reclaim --size-gb "$size" \
            --repeat "$repeat" --reclaim-mode "$mode" --extent-kb "$ext" \
            | tee "$out/$label.txt"
    done
done

for mode in reusable munmap; do
    echo "== steady-$mode-ext2048"
    target/release/hv-reclaim --time-reclaim --size-gb "$size" \
        --extent-kb 2048 --steady-state --repeat 10 --reclaim-mode "$mode" \
        | tee "$out/steady-$mode-ext2048.txt"
done
