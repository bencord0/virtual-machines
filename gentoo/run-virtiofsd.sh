#!/bin/bash
#
# virtio-fs daemon for the microvm.
#
# QEMU can pass in filesystems from the host to a guest
# using the virtio-fs protocol.
#
# Run this in the background before ./run-microvm.sh:
#
#     ./run-virtiofsd.sh &
#     ./run-microvm.sh
#
# The guest mounts it with (tag=repos):
#
#     mount -t virtiofs repos /var/db/repos

set -euo pipefail

SOCKET="${SOCKET:-vfs.sock}"
SHARED_DIR="${SHARED_DIR:-/var/db/repos}"

# Change to the script's directory so vfs.sock lands next to run-microvm.sh.
cd "$(dirname "$0")"

rm -fv "${SOCKET}" "${SOCKET}.pid"


exec /usr/libexec/virtiofsd \
    --socket-path="$SOCKET" \
    --shared-dir="$SHARED_DIR"
