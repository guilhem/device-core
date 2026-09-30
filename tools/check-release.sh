#!/bin/bash
set -euo pipefail
cd "$(dirname "$0")/.."
name=device-core-${TARGET:?target required}
version=${VERSION:?version required}
cd build/release-check
sha256sum --check --strict "$name.tar.gz.sha256"
tar -xzf "$name.tar.gz" "$name/device-core" "$name/LICENSE" "$name/NOTICE"
binary=$PWD/$name/device-core
readelf -h -A -V -d "$binary" > "$name.elf.txt"
case "$TARGET" in
  arm-unknown-linux-gnueabihf)
    grep -Eq 'Machine: +ARM$' "$name.elf.txt"
    grep -Eq 'Tag_CPU_arch: v6([^0-9]|$)' "$name.elf.txt"
    grep -q 'Tag_ABI_VFP_args: VFP registers' "$name.elf.txt"
    ! grep -Eq 'Tag_CPU_arch: v([7-9]|[1-9][0-9])|Tag_THUMB_ISA_use: Thumb-2' "$name.elf.txt"
    libc_limit=2.41
    actual=$(qemu-arm -cpu arm1176 -L ../armv6-sysroot "$binary" --version)
    ;;
  aarch64-unknown-linux-gnu)
    grep -Eq 'Machine: +AArch64$' "$name.elf.txt"
    libc_limit=2.39
    actual=$("$binary" --version)
    ;;
  x86_64-unknown-linux-gnu)
    grep -Eq 'Machine: +Advanced Micro Devices X86-64$' "$name.elf.txt"
    libc_limit=2.39
    actual=$("$binary" --version)
    ;;
  *) exit 2 ;;
esac
libc_required=$(sed -n 's/.*Name: GLIBC_\([0-9.]*\).*/\1/p' "$name.elf.txt" | sort -V | tail -1)
test -n "$libc_required"
test "$(printf '%s\n%s\n' "$libc_limit" "$libc_required" | sort -V | tail -1)" = "$libc_limit"
test "$actual" = "device-core $version"
printf '%s: %s; glibc %s\n' "$TARGET" "$actual" "$libc_required"
