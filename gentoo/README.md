# Gentoo - Cloud Init

The tooling lives in the `vm` crate at the repo root;
the wrapper scripts here just call the shared binaries with this VM's flags

## Usage

### 1. ./fetch.sh

Downloads and verifies the latest gentoo di-amd64-cloudinit virtual machine
image, snapshots the image and extracts the kernel and initramfs into `./boot`.

    $ ./fetch.sh

The boot extraction step uses libguestfs (`guestfish --ro`).
We're using qemu microvms, which require direct booting from an external
kernel, so the kernel and initramfs can be booted from the host filesystem.

### 2. ./run.sh

Boots the microvm, supervising both child processes as a single tree:

  - **virtiofsd** shares mounts into the guest over virtio-fs.
  - **qemu** runs the microvm itself.

    $ ./run.sh

`--shared-dir` takes `<dir>:<tag>[:ro]`, where `<tag>` is what the guest mounts.
Print the qemu command without booting anything with `./run.sh --dry-run`.

The guest console is redirected to a pty symlinked as `serial.pty`; attach to it
with:

    $ minicom -D serial.pty

Inside the guest, mount the shared portage tree (tag=repos):

    $ mount -t virtiofs repos /var/db/repos

Notable qemu options (`./run.sh --dry-run` prints the full invocation):

    - -M microvm - a firecracker inspired lighter VM

    - -object memory-backend-file - required for virtio-fs.
        Its size tracks --memory, so that has to fit in /dev/shm.

        Avoid exactly 2G: on qemu 10.2.2 / linux 6.18 a microvm with
        2GiB of guest RAM gets an e820 map that ends flush against the
        MMIO hole, and the initramfs /init segfaults in ld.so. 1800M
        and 4G both boot.

    - -nodefaults, -nographic, and -serial pty:serial.pty
        qemu creates the serial.pty symlink on start and removes it on exit.
        Useful for running as a systemd service, and you still want
        to connect to the console interactively.

    - -kernel and -initrd - microvm needs these to be external to the image

    - virtio-blk-device and virtio-net-device - microvm has no PCI.
        Use virtio for everything.

    - vhost-user-fs-device - Efficiently mount host filesystems into the guest.
        We use this to access the host's portage tree.

Read more about [qemu microvm](https://www.qemu.org/docs/master/system/i386/microvm.html)
and [virtio-fs](https://virtio-fs.gitlab.io/).
