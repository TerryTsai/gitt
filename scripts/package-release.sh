#!/bin/sh
# Package an already-built standalone executable and its license notices.
set -eu
target=${1:?Usage: package-release.sh <Rust target> [output-directory]}
output=${2:-dist}
case "$target" in
    x86_64-unknown-linux-musl) arch=x86_64 ;;
    aarch64-unknown-linux-musl) arch=aarch64 ;;
    *) printf 'Unsupported release target: %s\n' "$target" >&2; exit 2 ;;
esac
project=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
mkdir -p "$output"
output=$(CDPATH= cd -- "$output" && pwd)
staging=$(mktemp -d "${TMPDIR:-/tmp}/gitt-package.XXXXXXXX")
trap 'rm -rf "$staging"' 0
trap 'exit 1' HUP INT TERM
mkdir -p "$staging/assets"
cp "$project/target/$target/release/gitt" "$staging/gitt"
cp "$project/LICENSE" "$project/THIRD_PARTY_NOTICES.md" "$staging/"
cp -R "$project/assets/licenses" "$staging/assets/"
if [ -f /usr/share/doc/musl/copyright ]; then
    cp /usr/share/doc/musl/copyright "$staging/assets/licenses/musl-COPYRIGHT"
fi
tar -czf "$output/gitt-linux-$arch.tar.gz" -C "$staging" gitt LICENSE THIRD_PARTY_NOTICES.md assets/licenses
(cd "$output" && sha256sum "gitt-linux-$arch.tar.gz" > SHA256SUMS)
printf '%s\n' "$output/gitt-linux-$arch.tar.gz"
