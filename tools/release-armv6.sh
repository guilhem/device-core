#!/bin/bash
# Raspbian's ARMv6 libc is required; Debian/Ubuntu armhf libraries need ARMv7.
set -euo pipefail
repo=$(cd "$(dirname "$0")/.." && pwd)
sysroot=$repo/build/armv6-sysroot
mkdir -p "$sysroot/usr/lib" "$repo/build/armv6-debs"
ln -sfn usr/lib "$sysroot/lib"
while IFS=$'\t' read -r name url hash; do
  deb=$repo/build/armv6-debs/$name.deb
  if ! printf '%s  %s\n' "$hash" "$deb" | sha256sum --check --status; then
    curl --fail --location --retry 3 --proto '=https' --proto-redir '=https' "$url" -o "$deb.part"
    printf '%s  %s\n' "$hash" "$deb.part" | sha256sum --check --strict
    mv "$deb.part" "$deb"
  fi
  dpkg-deb --extract "$deb" "$sysroot"
done < <(jq -r 'to_entries[] | [.key, .value.url, .value.sha256] | @tsv' "$repo/tools/armv6-sysroot.lock.json")
while IFS= read -r -d '' link; do
  destination=$(readlink "$link")
  ln -sfn "$(realpath -m --relative-to="$(dirname "$link")" "$sysroot$destination")" "$link"
done < <(find "$sysroot" -type l -lname '/*' -print0)
linker=$sysroot/armv6-cc
printf '#!/bin/bash\nexec %s"$@"\n' "$(printf '%q ' clang --target=arm-linux-gnueabihf "--sysroot=$sysroot" "--gcc-toolchain=$sysroot/usr" -fuse-ld=lld -Wno-unused-command-line-argument -mcpu=arm1176jzf-s -mfpu=vfp -mfloat-abi=hard)" > "$linker"
chmod 755 "$linker"
if [[ -n ${GITHUB_ENV:-} ]]; then
  printf 'CC_arm_unknown_linux_gnueabihf=%s\nAR_arm_unknown_linux_gnueabihf=llvm-ar\nCARGO_TARGET_ARM_UNKNOWN_LINUX_GNUEABIHF_LINKER=%s\n' "$linker" "$linker" >> "$GITHUB_ENV"
fi
