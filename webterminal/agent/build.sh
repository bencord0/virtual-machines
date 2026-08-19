#!/bin/bash
set -e

cargo +nightly \
    -Z script -Z unstable-options \
    build --release \
    --artifact-dir ../share \
    --manifest-path ./agent.rs
