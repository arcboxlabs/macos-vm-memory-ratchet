#!/bin/sh
# Build, ad-hoc sign with the hypervisor entitlement, and run hv-reclaim.
set -eu
cd "$(dirname "$0")"
cargo build --release -p hv-reclaim
codesign --force -s - --entitlements crates/hv-reclaim/hv.entitlements target/release/hv-reclaim
exec target/release/hv-reclaim "$@"
