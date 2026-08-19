# Gentoo - Cloud Init

## Usage

1. ./fetch-image.rs

Downloads and verifies the latest gentoo di-amd64-cloudinit virtual machine image.

Self-contained rust/cargo script. Currently requires a nightly toolchain,
but that will settle down eventually. The script could have been bash, but this
seemed like something fun to try.

Execute it directly:

    $ ./fetch-image.rs

Or compile it first and run as a separate step.

    $ cargo +nightly -Z script -Z unstable-options \
        build --release --artifact-dir . --manifest-path fetch-image.rs
    $ ./fetch-image

Read more about [cargo scripts](https://rust-lang.github.io/rfcs/3502-cargo-script.html).

2. ./extract-boot.sh

Uses libguestfs to peek into the downloaded disk image to extract the kernel and
initramfs.

We're using qemu microvms, which require direct booting from an external kernel.

3. ./run-virtiofsd.sh

In a dedicated shell, and in a terminal that gives you sudo to root access.

    $ ./run-virtiofsd.sh

Read more about [virtio-fs](https://virtio-fs.gitlab.io/).

4. ./run-microvm.sh

Runs qemu itself. Read the script to learn it's magick.

    - -M microvm - a firecracker inspired lighter VM

    - -object memory-backend-file - required for virtio-fs

    - -nodefaults, -nographic, and -serial pty:serial.pty
        Useful for running as a systemd service, and you still want
        to connect to the console interactively.

        $ minicom -D serial.pty

    - -kernel and -initrd - microvm needs these to be external to the image

    - virtio-blk-device and virtio-net-device - microvm has no PCI.
        Use virtio for everything.

    - vhost-user-fs-device - Efficiently mount host filesystems into the guest.
        We use this to access the host's portage tree.

Read more about [qemu microvm](https://www.qemu.org/docs/master/system/i386/microvm.html)
