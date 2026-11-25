#!/bin/bash
# Exchange I/O tests on real file systems (disk images, never a real drive; mounted outside /Volumes, so
# Media Composer and Finder don't see them):
#   1. tests/exchange_io.rs on APFS, case-sensitive APFS, exFAT and HFS+
#   2. tests/exchange_faults.rs: full drive, drive filling up mid-copy, read-only drive, drive pulled out, app killed (APFS sender, exFAT receiver)
# Usage: scripts/exchange-disks.sh [name filter for the fault tests]
set -eu
REPO="$(cd "$(dirname "$0")/.." && pwd)"
WORK="$(mktemp -d "${TMPDIR:-/tmp}/mfc-disks.XXXXXX")"
export PATH="$HOME/.cargo/bin:$PATH"

cleanup() {
  for m in "$WORK/io" "$WORK/main" "$WORK/vols/b"; do
    hdiutil detach -quiet -force "$m" 2>/dev/null || true
  done
  chmod -R u+w "$WORK" 2>/dev/null || true
  rm -rf "$WORK"
}
trap cleanup EXIT

cd "$REPO"
cargo test -q --no-run --test exchange_io --test exchange_faults 2>/dev/null

image() { # file system, size, image file, mount point
  hdiutil create -quiet -size "$2" -type SPARSE -fs "$1" -volname MFCIO "$3"
  mkdir -p "$4"
  hdiutil attach -quiet -nobrowse -mountpoint "$4" "$3.sparseimage"
}

for fs in "APFS" "Case-sensitive APFS" "ExFAT" "HFS+"; do
  echo "• exchange_io on $fs"
  image "$fs" 1g "$WORK/io" "$WORK/io"
  MFC_IO_ROOT="$WORK/io" MFC_LAB_RESERVE=0 cargo test -q --test exchange_io 2>&1 | grep -E "test result|FAILED|panicked" || true
  hdiutil detach -quiet "$WORK/io"
  rm -f "$WORK/io.sparseimage"
done

echo "• exchange_faults (sender APFS, receiver exFAT 256 MB)"
image "APFS" 2g "$WORK/main" "$WORK/main"
image "ExFAT" 256m "$WORK/b" "$WORK/vols/b"
chmod 555 "$WORK/vols"
MFC_IO_ROOT="$WORK/main" MFC_IO_B="$WORK/vols/b" MFC_IO_B_IMAGE="$WORK/b.sparseimage" MFC_LAB_RESERVE=0 \
  cargo test -q --test exchange_faults -- --ignored --test-threads=1 ${1:-} 2>&1 | grep -vE "^\s*$|^running|^failures:|RUST_BACKTRACE" || true
