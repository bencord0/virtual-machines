#!/bin/bash

set -euo pipefail

# Extract just the kernel and initramfs from the verified image into ./boot.
#
# The image is opened read-only (guestfish --ro), so guestfs never modifies it.
# Only /boot/kernel-* and /boot/initramfs-*.img are copied out -- not all of
# /boot (which also holds EFI/, grub/ and microcode which we don't need).

IMAGE=image.qcow2
DEST=boot

mkdir -p "${DEST}"

guestfish --ro -a "${IMAGE}" -i \
    glob copy-out '/boot/kernel-*' "${DEST}" \
    : glob copy-out '/boot/initramfs-*.img' "${DEST}"

# Confirm both artifacts were extracted, then rename them
# so that we don't need to hard-code a kernel version.
shopt -s nullglob
kernels=("${DEST}"/kernel-*)
initrds=("${DEST}"/initramfs-*.img)
if [[ ${#kernels[@]} -eq 0 || ${#initrds[@]} -eq 0 ]]; then
    echo "error: kernel or initramfs not found in ${IMAGE}:/boot" >&2
    exit 1
fi

mv -f "${kernels[0]}" "${DEST}/kernel"
mv -f "${initrds[0]}" "${DEST}/initramfs.img"

echo "extracted ${DEST}/kernel and ${DEST}/initramfs.img"
