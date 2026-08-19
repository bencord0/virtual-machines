#!/bin/bash
#
#     mount -t virtiofs agent /opt/agent
#
# Run this in the background before ./run-microvm.sh:
#
#     ./run-virtiofsd.sh &
#     ./run-microvm.sh

set -exuo pipefail

SOCKET="${SOCKET:-vfs.sock}"
SHARED_DIR="${SHARED_DIR:-./share}"

function cleanup() {
    rm -fv "${SOCKET}" "${SOCKET}.pid"
}

cd "$(dirname "$0")"
cleanup

trap cleanup EXIT
/usr/libexec/virtiofsd \
    --socket-path="$SOCKET" \
    --socket-group="$USER" \
    --shared-dir="$SHARED_DIR"
