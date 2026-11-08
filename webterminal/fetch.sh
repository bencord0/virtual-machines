#!/bin/bash

set -euo pipefail

rm -fv image.qcow2
rm -fv boot

../distfiles/fetch.sh
ln -sv ../distfiles/boot ./boot

qemu-img create \
    -f qcow2 -B qcow2 \
    -b ../distfiles/image.qcow2 \
    image.qcow2
