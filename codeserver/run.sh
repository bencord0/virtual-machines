#!/bin/bash
# Run this VM.
# Attach to the guest console with:  minicom -D serial.pty

set -euo pipefail
cd "$(dirname "$0")"

RUN_ARGS=(
    cargo run

    --release
    --manifest-path ../Cargo.toml
    --bin run
    --

    # The host portage tree, read-only so a guest emerge can't corrupt it.
    # Mounted by cloud-init:
    #   $ mount -t virtiofs repos /var/db/repos
    --shared-dir /var/db/repos:repos:ro

    # Port forward web(8080)
    --hostfwd tcp::8080-:8080
)

exec "${RUN_ARGS[@]}" "${@}"
