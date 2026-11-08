//! Boot a qemu microvm, with access to filesystems shared from the host:
//!
//!   * virtiofsd  -- one per share, each handed its own listening socket
//!   * qemu       -- the microvm hypervisor itself
//!
//! The guest mounts a share with:
//!
//!     mount -t virtiofs <tag> <mountpoint>

use std::os::fd::{AsFd, AsRawFd, RawFd};
use std::os::unix::process::CommandExt;
use std::process::Command;
use std::sync::atomic::Ordering;

use clap::Parser;
use eyre::{Result, WrapErr, bail};

use vm::{
    FIRST_CHILD_FD, QEMU_PID, QemuConfig, ReapGuard, STAGING_FD, SharedDir, abstract_connect,
    abstract_listener, dup_above, install_signal_handlers, share_fd, shell_quote, spawn_with_fds,
};

/// Supervise virtiofsd + qemu to boot a microvm.
#[derive(Parser, Debug)]
#[command(about, long_about = None)]
struct Args {
    /// Host directory to share into the guest over virtio-fs, as
    /// <dir>:<tag>[:ro], where <tag> is what the guest mounts. Repeatable.
    #[arg(long = "shared-dir", alias = "shared-dirs", value_name = "DIR:TAG[:ro]")]
    shared_dirs: Vec<SharedDir>,

    /// The virtiofsd binary (Gentoo installs it under libexec).
    #[arg(long, default_value = "/usr/libexec/virtiofsd")]
    virtiofsd: String,

    /// Path qemu symlinks the guest serial pty to (attach with e.g. minicom).
    #[arg(long, default_value = "./serial.pty")]
    serial: String,

    /// Guest memory (also sizes the shared memory-backend-file, so it has to fit in /dev/shm).
    #[arg(long, default_value = "4G")]
    memory: String,

    /// Number of vCPUs.
    #[arg(long, default_value_t = 2)]
    smp: usize,

    /// Pin qemu to specific CPUs, e.g. `--taskset 0,1`.
    /// default: All CPUs are available.
    #[arg(long, value_delimiter = ',')]
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

    /// Kernel command line.
    #[arg(long, default_value = "earlyprintk=ttyS0 console=ttyS0 root=/dev/vda2")]
    append: String,

    /// cloud-init seed image.
    #[arg(long, default_value = "./cloud-init/seed.iso")]
    seed: String,

    /// cloud-init seed image build script.
    #[arg(long, default_value = "./cloud-init/build.sh")]
    seed_build_script: String,

    /// Print the qemu command that would run, then exit.
    #[arg(long)]
    dry_run: bool,
}

impl Args {
    fn qemu_config(&self) -> QemuConfig {
        QemuConfig {
            shared_dirs: self.shared_dirs.clone(),
            serial: self.serial.clone(),
            memory: self.memory.clone(),
            smp: self.smp,
            hostfwd: self.hostfwd.clone(),
            image: self.image.clone(),
            kernel: self.kernel.clone(),
            initrd: self.initrd.clone(),
            append: self.append.clone(),
            seed: self.seed.clone(),
        }
    }
}

fn main() -> Result<()> {
    let args = Args::parse();
    let config = args.qemu_config();

    if args.dry_run {
        println!("{}", shell_quote(&config.argv()));
        return Ok(());
    }

    // qemu symlinks the guest pty here on start and hard-fails on a stale one
    // ("Failed to create PTY symlink: File exists"); it removes its own on exit.
    let _ = std::fs::remove_file(&args.serial);

    build_seed(&args.seed_build_script)?;

    // Terminating the supervisor should tear the whole VM down with it.
    install_signal_handlers().wrap_err("installing signal handlers")?;

    // One listener per share. virtiofsd's --fd takes a listening (not yet
    // connected) socket, and every `-chardev socket,fd=` is a separate
    // connection, so a shared listener would leave all but one share unpaired.
    let mut children = Vec::new();
    let mut staged = Vec::new();
    for (i, share) in config.shared_dirs.iter().enumerate() {
        let name = format!("vfs-{}-{i}", std::process::id());
        let (listener, addr, addr_len) = abstract_listener(name.as_bytes())
            .wrap_err_with(|| format!("creating vhost-user listener @{name}"))?;

        let mut cmd = Command::new(&args.virtiofsd);
        cmd.arg(format!("--fd={FIRST_CHILD_FD}"))
            .arg(format!("--shared-dir={}", share.dir));
        if share.readonly {
            cmd.arg("--readonly");
        }
        children.push(ReapGuard(
            spawn_with_fds(&mut cmd, &[(listener.as_raw_fd(), FIRST_CHILD_FD)])
                .wrap_err_with(|| format!("spawning virtiofsd for {}", share.dir))?,
        ));

        let conn = abstract_connect(&addr, addr_len)
            .wrap_err_with(|| format!("connecting to virtiofsd for {}", share.dir))?;
        drop(listener);

        // Park qemu's end above every destination: the parent's own sockets land
        // on 3, 4, 5..., which are exactly the numbers qemu wants them on.
        staged.push(dup_above(conn.as_fd(), STAGING_FD).wrap_err("staging qemu's socket")?);
    }

    let pairs: Vec<(RawFd, RawFd)> = staged
        .iter()
        .enumerate()
        .map(|(i, fd)| (fd.as_raw_fd(), share_fd(i)))
        .collect();

    let mut cmd = config.command();
    if let Some(cpus) = &args.taskset {
        pin_to_cpus(&mut cmd, cpus)?;
    }
    let mut qemu_child =
        spawn_with_fds(&mut cmd, &pairs).wrap_err("spawning qemu-system-x86_64")?;
    QEMU_PID.store(qemu_child.id() as i32, Ordering::SeqCst);
    drop(staged);

    // Wait for qemu (the workload); dropping the guards stops every virtiofsd.
    let status = qemu_child.wait().wrap_err("waiting for qemu")?;
    drop(children);

    match status.code() {
        // None: qemu took the SIGTERM we forwarded, i.e. we asked it to stop.
        Some(0) | None => Ok(()),
        Some(code) => std::process::exit(code),
    }
}

/// Build the cloud-init seed image. A signalled build leaves a stale seed.iso
/// in place, so unlike qemu's exit it is an error rather than a clean stop.
fn build_seed(script: &str) -> Result<()> {
    let status = Command::new(script)
        .status()
        .wrap_err_with(|| format!("running {script}"))?;
    match status.code() {
        Some(0) => Ok(()),
        Some(code) => bail!("{script} exited with {code}"),
        None => bail!("{script} was killed by a signal ({status})"),
    }
}

/// Restrict `cmd` to `cpus`. The set is built here rather than in `pre_exec`,
/// which runs post-fork and so must not allocate.
fn pin_to_cpus(cmd: &mut Command, cpus: &[usize]) -> Result<()> {
    let mut set: libc::cpu_set_t = unsafe { std::mem::zeroed() };
    unsafe { libc::CPU_ZERO(&mut set) };
    for &cpu in cpus {
        if cpu >= libc::CPU_SETSIZE as usize {
            bail!("cpu {cpu} is outside CPU_SETSIZE ({})", libc::CPU_SETSIZE);
        }
        unsafe { libc::CPU_SET(cpu, &mut set) };
    }

    unsafe {
        cmd.pre_exec(move || {
            // pid 0 = the calling thread, which becomes qemu; children inherit it.
            if libc::sched_setaffinity(0, std::mem::size_of_val(&set), &set) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    Ok(())
}
