#!/bin/bash
#
# Download + PGP-verify the latest Gentoo di cloud image.
# The kernel and initramfs will be extracted into ./boot.
#
#   ./fetch.sh --variant di-amd64-cloudinit (default)
#   ./fetch.sh --variant di-amd64-console

set -euo pipefail
cd "$(dirname "$0")"

exec cargo run --release --manifest-path ../Cargo.toml --bin fetch -- "$@"
