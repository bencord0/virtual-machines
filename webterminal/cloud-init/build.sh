#!/bin/bash

set -euo pipefail
cd "$(dirname "${0}")"

rm -fv seed.iso
truncate --size 2M seed.iso
mkfs.vfat -n CIDATA seed.iso
mcopy -oi seed.iso user-data meta-data vendor-data ::
