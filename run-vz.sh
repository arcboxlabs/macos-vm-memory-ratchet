#!/bin/sh
# Build and run vz-ratchet: the guest /init (static aarch64 musl), the
# Swift VZ harness (ad-hoc signed with the virtualization entitlement),
# and the Rust driver. The guest kernel is fetched once from ArcBox's
# public boot CDN and pinned by sha256; any VZ-bootable arm64 kernel with
# virtio-console + virtio-balloon built in works via VZ_RATCHET_KERNEL.
set -eu
cd "$(dirname "$0")"

KERNEL_URL="https://boot.arcboxcdn.com/asset/v0.8.4/arm64/kernel"
KERNEL_SHA256="dae694675138649c41a652240035b5fe6d09765b200b87df167ea519b7a9f670"
OUT=target/vz-guest
mkdir -p "$OUT" target/release

KERNEL="${VZ_RATCHET_KERNEL:-$OUT/kernel}"
if [ ! -f "$KERNEL" ]; then
    echo "fetching guest kernel (ArcBox boot assets v0.8.4, arm64) ..."
    curl -fL -o "$OUT/kernel.tmp" "$KERNEL_URL"
    echo "$KERNEL_SHA256  $OUT/kernel.tmp" | shasum -a 256 -c -
    mv "$OUT/kernel.tmp" "$OUT/kernel"
fi

rustup target add aarch64-unknown-linux-musl 2>/dev/null || true
# cd first: cargo discovers guest-init/.cargo/config.toml (the rust-lld
# linker) from the working directory, not from --manifest-path.
(cd guest-init && cargo build --release --target aarch64-unknown-linux-musl)

rm -rf "$OUT/root"
mkdir -p "$OUT/root/dev" "$OUT/root/proc"
cp guest-init/target/aarch64-unknown-linux-musl/release/guest-init "$OUT/root/init"
(cd "$OUT/root" && find . | cpio -o -H newc 2>/dev/null) > "$OUT/initramfs.cpio"

/usr/bin/xcrun swiftc -O harness/vz-ratchet.swift -o target/release/vz-harness \
    -framework Virtualization
codesign --force -s - --entitlements harness/vz.entitlements target/release/vz-harness

cargo build --release -p vz-ratchet
exec target/release/vz-ratchet \
    --harness target/release/vz-harness \
    --kernel "$KERNEL" \
    --initramfs "$OUT/initramfs.cpio" \
    "$@"
