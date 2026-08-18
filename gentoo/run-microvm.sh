#!/bin/bash

#sudo chmod 666 /dev/kvm

rm -fv serial.pty

QEMU_ARGS=(
    qemu-system-x86_64

    # Machine Type - As native as possible
    -M microvm #,x-option-roms=off,rtc=off
    --enable-kvm -cpu host

    # Machine size
    -smp 1 -m 1024M
    -object memory-backend-file,id=mem0,size=1024M,mem-path=/dev/shm,share=on -numa node,memdev=mem0

    # Open a TTY that clients can detach from
    -nodefaults -no-user-config -nographic
    -serial pty:serial.pty

    # Kernel / initrd
    # $ ./extract-boot.sh
    -kernel ./boot/kernel
    -append "earlyprintk=ttyS0 console=ttyS0 root=/dev/vda2"
    -initrd ./boot/initramfs.img

    # Rootfs
    -device virtio-blk-device,drive=img0
    -drive file=./image.qcow2,if=none,format=qcow2,id=img0,snapshot=on

    # Cloud-init drive
    -device virtio-blk-device,drive=img1
    -drive file=./cloud-init/seed.iso,if=none,format=raw,id=img1,readonly=on

    # virtio-fs
    -chardev socket,id=vfs0,path=vfs.sock
    -device vhost-user-fs-device,queue-size=1024,chardev=vfs0,tag=repos

    # Networking
    -device virtio-net-device,netdev=qemu0
    -netdev user,id=qemu0
    #-netdev bridge,id=qemu0,br=br0
)

exec "${QEMU_ARGS[@]}"
