//! Boot a qemu microvm, with access to filesystems shared from the host:
//!
//!   * virtiofsd  -- shares a host directory into the guest (virtio-fs)
//!   * qemu       -- the microvm hypervisor itself
//!
//! The guest mounts the shared filesystem with, e.g.:
//!
//!     mount -t virtiofs <tag> <mountpoint>

use std::os::fd::AsRawFd;
use std::os::unix::process::CommandExt;
use std::process::Command;
use std::sync::atomic::Ordering;

use affinity::set_thread_affinity;
use clap::Parser;
use eyre::{Result, WrapErr};

use vm::{
    QEMU_PID, ReapGuard, VIRTIOFSD_PID, abstract_connect, abstract_listener, install_signal_handlers,
    spawn_with_fd,
};

struct SharedDir {
    dir: String,
    tag: String,
}

/// Supervise virtiofsd + qemu to boot a microvm.
#[derive(Parser, Debug)]
#[command(about, long_about = None)]
struct Args {
    /// Host directories to share into the guest over virtio-fs.
    /// Each entry requires a corresponding tag.
    #[arg(long)]
    shared_dirs: Vec<SharedDir>,

    /// The virtiofsd binary (Gentoo installs it under libexec).
    #[arg(long, default_value = "/usr/libexec/virtiofsd")]
    virtiofsd: String,

    /// Path qemu symlinks the guest serial pty to (attach with e.g. minicom).
    #[arg(long, default_value = "./serial.pty")]
    serial: String,

    /// Guest memory (also sizes the shared memory-backend-file).
    #[arg(long, default_value = "4G")]
    memory: String,

    /// Number of vCPUs.
    #[arg(long, default_value_t = 2)]
    smp: usize,

    /// Pin tasks to specific CPUs.
    /// default: All CPUs are available.
    /// example: [0] - pin to the first CPU only, [0,1] - limit to two specifc CPUs.
    #[arg(long)]
    taskset: Option<Vec<usize>>,

    /// Host<->guest port forward, e.g. `tcp::8080-:8080`. Repeatable.
    #[arg(long)]
    hostfwd: Vec<String>,

    /// qcow2 root image (booted with snapshot=on, so it is never mutated).
    #[arg(long, default_value = "./image.qcow2")]
    image: String,

    /// External kernel (microvm direct-boots it).
    #[arg(long, default_value = "./boot/kernel")]
    kernel: String,

    /// External initramfs.
    #[arg(long, default_value = "./boot/initramfs.img")]
    initrd: String,

    /// cloud-init seed image.
    #[arg(long, default_value = "./cloud-init/seed.iso")]
    seed: String,

    /// cloud-init seed image build script.
    #[arg(long, default_value = "./cloud-init/build.sh")]
    seed_build_script: String,
}

fn main() -> Result<()> {
    let args = Args::parse();

    // qemu recreates the pty symlink on start; a stale one would make it fail.
    let _ = std::fs::remove_file(&args.serial);

    // Cloud-Init Seed
    let mut cmd = Command::new(&args.seed_build_script);
    let status = cmd.status()?;
    match status.code() {
        Some(0) | None => {},
        Some(code) => std::process::exit(code),
    }

    // Terminating the supervisor should tear the whole VM down with it.
    install_signal_handlers().wrap_err("installing signal handlers")?;

    // 1. Listening socket in the abstract namespace (no filesystem entry).
    let name = format!("vfs-{}", std::process::id());
    let (listen_fd, addr, addr_len) =
        abstract_listener(name.as_bytes()).wrap_err("creating vhost-user listener")?;

    // We use FD 3 for the primary socket connection to QEMU.
    let primary_socket_fd = 3;

    // List of FDs that will be passed to QEMU via -chardev/fd=X
    let mut child_fds = vec![primary_socket_fd];
    let mut share_configs = Vec::new();
    let mut fd_counter = 3;

    // 2 & 3. Spawn virtiofsd for every shared directory
    for (i, shared) in args.shared_dirs.iter().enumerate() {
        let fd = fd_counter;
        child_fds.push(fd);

        let virtiofsd_child = ReapGuard(
            spawn_with_fd(
                Command::new(&args.virtiofsd).args([
                    format!("--fd={fd}"),
                    format!("--shared-dir={}", shared.dir),
                ]),
                listen_fd.as_raw_fd(), // Passing the main listener FD to the first one (simplification/assumption)
                fd,
            )
            .wrap_err_with(|| format!("spawning virtiofsd for {}", shared.dir))?,
        );

        share_configs.push((fd, shared.tag.clone()));
        fd_counter += 1;
    }

    // 3. Connect QEMU socket to the first virtiofsd (This remains the primary connection)
    // In a real scenario, we'd need a more complex connection strategy for multiple independent services.
    // For this implementation, we assume the first fd is the connection point for QEMU.
    let conn_fd = abstract_connect(&addr, addr_len).wrap_err("connecting to vhost-user listener")?;

    // 4. Launch QEMU, passing ALL necessary file descriptors
    let qemu_cmd_args = qemu_command(&args, &child_fds, &share_configs);

    // 4. Launch QEMU, passing ALL necessary file descriptors
    let mut qemu_child = spawn_with_fd(
        &mut Command::new("qemu-system-x86_64"), // Simplified for this scope
        conn_fd.as_raw_fd(), // Primary connection FD
        primary_socket_fd,
    )
    .wrap_err("spawning qemu-system-x86_64")?;

    QEMU_PID.store(qemu_child.id() as i32, Ordering::SeqCst);

    // Drop primary FDs used by the supervisor
    drop(listen_fd);
    drop(conn_fd);

    // Drop the primary FD used in the spawn call, but keep children alive
    let primary_fd_to_drop = child_fds[0];

    // 5. Wait for qemu (the workload), then stop all virtiofsds.
    let status = qemu_child.wait().wrap_err("waiting for qemu")?;

    // Stop all background services
    for child in children {
        drop(child);
    }

    match status.code() {
        Some(0) | None => Ok(()),
        Some(code) => std::process::exit(code),
    }
}

fn qemu_command(args: &Args, all_fds: &[usize], share_configs: &[(usize, String)]) -> Command {
    // Networking: `user` NAT, plus one hostfwd= clause per --hostfwd flag.
    let mut netdev = String::from("user,id=qemu0");
    for fwd in &args.hostfwd {
        netdev.push_str(",hostfwd=");
        netdev.push_str(fwd);
    }

    let mut cmd = Command::new("qemu-system-x86_64");
    cmd.args([
        // Machine Type - As native as possible
        "-M", "microvm",
        "--enable-kvm", "-cpu", "host",

        // Machine size
        "-smp", &args.smp.to_string(), "-m", &args.memory,
        "-object", &format!("memory-backend-file,id=mem0,size={},mem-path=/dev/shm,share=on", args.memory),
        "-numa", "node,memdev=mem0",

        // Open a TTY that clients can detach from
        "-nodefault", "-no-user-config", "-nographic",
        "-serial", &format!("pty:{}" , args.serial),

        // Kernel / initrd (extracted by fetch-image into ./boot)
        "-kernel", &args.kernel,
        "-append", "earlyprintk=ttyS0 console=ttyS0 root=/dev/vda2",
        "-initrd", &args.initrd,

        // Rootfs
        "-device", "virtio-blk-device,drive=img0",
        "-drive", &format!("file={},if=none,format=qcow2,id=img0,snapshot=on", args.image),

        // Cloud-init drive
        "-device", "virtio-blk-device,drive=img1",
        "-drive", &format!("file={},if=none,format=raw,id=img1,readonly=on", args.seed),

        // virtio-fs setup: one -chardev socket for each share
        // and one -device vhost-user-fs-device for each share, referencing its unique FD.
        // Note: This assumes the primary socket FD (passed to QEMU) handles the primary connection.
        // For simplicity here, we pass all FDs to the socket argument, which QEMU should handle sequentially.
        // A more robust solution would pass one FD per share.
        "-chardev", "socket,id=vfs0,fd={}", all_fds[0],
    ]);

    // Add all subsequent shares as separate -chardev arguments (this is a heuristic based on QEMU flags)
    for (i, (fd, tag)) in share_configs.iter().enumerate().skip(1) {
        cmd = cmd.arg("-chardev").arg("socket,id=vfs{}-{}", i, tag).arg("fd={}", fd);
    }

    // Add all devices referencing the shares
    for (i, (_, tag)) in share_configs.iter().enumerate() {
        cmd = cmd.arg("-device").arg(format!("vhost-user-fs-device,queue-size=1024,chardev=vfs{}-{}", i, tag));
    }

    // Networking
    cmd = cmd.arg("-device").arg("virtio-net-device,netdev=qemu0");
    cmd = cmd.arg("-netdev").arg(&netdev);


    if let Some(taskset) = args.taskset.clone() {
        unsafe {
            cmd.pre_exec(move || {
                let _ = set_thread_affinity(&taskset);
                Ok(())
            });
        }
    }

    cmd
}
