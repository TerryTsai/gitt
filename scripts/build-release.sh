#!/bin/sh
# Requires Rust's musl target and a musl-capable C toolchain. No container required.
set -eu
target=${1:-x86_64-unknown-linux-musl}
case "$target" in
  x86_64-unknown-linux-musl|aarch64-unknown-linux-musl) ;;
  *) echo 'Choose x86_64-unknown-linux-musl or aarch64-unknown-linux-musl.' >&2; exit 2;;
esac
cargo build --locked --release --target "$target"
binary="target/$target/release/gitt"
if readelf -l "$binary" | grep -q INTERP || readelf -d "$binary" | grep -q NEEDED; then
  echo 'Release is not standalone: dynamic interpreter or shared-library dependency found.' >&2
  exit 1
fi
ls -l "$binary"
