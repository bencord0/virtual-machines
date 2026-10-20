use std::fs::{self};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::process::CommandExt;
use std::process::{Child, Command};
use std::sync::atomic::{AtomicI32, Ordering};

use eyre::{Result, WrapErr, bail, eyre};
use indicatif::{ProgressBar, ProgressStyle};

mod signing;
pub use signing::SigningHelper;


// microvm boots an external kernel/initrd, so we extract them from the image.
const BOOT_KERNEL: &str = "kernel";
const BOOT_INITRAMFS: &str = "initramfs.img";

// Child pids, so the async-signal handler can forward termination to both.
pub static VIRTIOFSD_PID: AtomicI32 = AtomicI32::new(-1);
pub static QEMU_PID: AtomicI32 = AtomicI32::new(-1);

// -- boot extraction --------------------------------------------------------

/// Copy the kernel and initramfs out of the verified image into `dest`,
/// renaming them to stable names (kernel, initramfs.img) so callers don't need
/// to know the kernel version. Uses libguestfs (`guestfish --ro`), so the image
/// is opened read-only and never mutated.
pub fn extract_boot(image_file: &str, dest: &str) -> Result<()> {
    fs::create_dir_all(dest).wrap_err_with(|| format!("creating {dest}/"))?;

    // Depends on app-emulation/libguestfs
    let status = Command::new("guestfish")
        .args(["--ro", "-a", image_file, "-i"])
        .args(["glob", "copy-out", "/boot/kernel-*", dest, ":"])
        .args(["glob", "copy-out", "/boot/initramfs-*.img", dest])
        .status()
        .wrap_err("running guestfish (is app-emulation/libguestfs installed?)")?;
    if !status.success() {
        bail!("guestfish exited with {status}");
    }

    // Confirm both artifacts landed, then rename to version-independent names.
    let kernel = find_boot_file(dest, |n| n.starts_with("kernel-"))?
        .ok_or_else(|| eyre!("no /boot/kernel-* found in {image_file}"))?;
    let initrd = find_boot_file(dest, |n| n.starts_with("initramfs-") && n.ends_with(".img"))?
        .ok_or_else(|| eyre!("no /boot/initramfs-*.img found in {image_file}"))?;

    let kernel_dest = format!("{dest}/{BOOT_KERNEL}");
    let initrd_dest = format!("{dest}/{BOOT_INITRAMFS}");
    fs::rename(&kernel, &kernel_dest)
        .wrap_err_with(|| format!("renaming {kernel} -> {kernel_dest}"))?;
    fs::rename(&initrd, &initrd_dest)
        .wrap_err_with(|| format!("renaming {initrd} -> {initrd_dest}"))?;

    println!("extracted {kernel_dest} and {initrd_dest}");
    Ok(())
}

/// Return the path of the first entry in `dir` whose filename matches `pred`.
fn find_boot_file(dir: &str, pred: impl Fn(&str) -> bool) -> Result<Option<String>> {
    for entry in fs::read_dir(dir).wrap_err_with(|| format!("reading {dir}/"))? {
        let entry = entry.wrap_err_with(|| format!("reading an entry in {dir}/"))?;
        if let Some(name) = entry.file_name().to_str() {
            if pred(name) {
                return Ok(Some(entry.path().to_string_lossy().into_owned()));
            }
        }
    }
    Ok(None)
}

// -- progress bar -----------------------------------------------------------

/// A byte-count progress bar (rate + ETA) rendered to stderr.
pub fn byte_bar(len: u64, msg: &'static str) -> ProgressBar {
    let pb = ProgressBar::new(len);
    pb.set_style(
        ProgressStyle::with_template(
            "{msg} {bytes}/{total_bytes} [{wide_bar}] {bytes_per_sec} eta {eta}",
        )
        .unwrap()
        .progress_chars("=> "),
    );
    pb.set_message(msg);
    pb
}

// -- filesystem helper ------------------------------------------------------

/// `ln -sfn target link`: create/replace `link` as a symlink to `target`.
pub fn symlink_force(target: &str, link: &str) -> Result<()> {
    match fs::symlink_metadata(link) {
        Ok(_) => fs::remove_file(link).wrap_err_with(|| format!("removing existing {link}"))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e).wrap_err_with(|| format!("inspecting {link}")),
    }
    std::os::unix::fs::symlink(target, link)
        .wrap_err_with(|| format!("symlinking {link} -> {target}"))?;
    Ok(())
}

// -- socket + supervisor plumbing -------------------------------------------

/// Spawn `cmd` with `src_fd` duped onto `dst_fd` (and FD_CLOEXEC cleared there),
/// so the child inherits the socket end as a known fd number.
pub fn spawn_with_fd(cmd: &mut Command, src_fd: RawFd, dst_fd: RawFd) -> Result<Child> {
    unsafe {
        cmd.pre_exec(move || {
            if libc::dup2(src_fd, dst_fd) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            let flags = libc::fcntl(dst_fd, libc::F_GETFD);
            if flags < 0 || libc::fcntl(dst_fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    Ok(cmd.spawn()?)
}

/// Build an abstract-namespace `sockaddr_un` and its length for `name`.
fn abstract_sockaddr(name: &[u8]) -> Result<(libc::sockaddr_un, libc::socklen_t)> {
    let mut addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    let path_off = std::mem::offset_of!(libc::sockaddr_un, sun_path);
    // +1 for the leading NUL that marks an abstract socket.
    if name.len() + 1 > addr.sun_path.len() {
        bail!("abstract socket name too long: {} bytes", name.len());
    }
    addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
    for (i, &b) in name.iter().enumerate() {
        addr.sun_path[i + 1] = b as libc::c_char;
    }
    let len = (path_off + 1 + name.len()) as libc::socklen_t;
    Ok((addr, len))
}

/// Create a listening AF_UNIX socket bound to an abstract address.
pub fn abstract_listener(name: &[u8]) -> Result<(OwnedFd, libc::sockaddr_un, libc::socklen_t)> {
    let (addr, len) = abstract_sockaddr(name)?;
    let fd = new_unix_socket()?;
    let sa = &addr as *const _ as *const libc::sockaddr;
    cvt(unsafe { libc::bind(fd.as_raw_fd(), sa, len) }).wrap_err("bind")?;
    cvt(unsafe { libc::listen(fd.as_raw_fd(), 1) }).wrap_err("listen")?;
    Ok((fd, addr, len))
}

/// Connect a fresh AF_UNIX socket to an abstract address, returning the fd.
pub fn abstract_connect(addr: &libc::sockaddr_un, len: libc::socklen_t) -> Result<OwnedFd> {
    let fd = new_unix_socket()?;
    let sa = addr as *const _ as *const libc::sockaddr;
    cvt(unsafe { libc::connect(fd.as_raw_fd(), sa, len) }).wrap_err("connect")?;
    Ok(fd)
}

/// A CLOEXEC AF_UNIX stream socket wrapped as an OwnedFd. CLOEXEC keeps it from
/// leaking into the *other* child; spawn_with_fd re-clears it on fd 3 only.
fn new_unix_socket() -> Result<OwnedFd> {
    let raw = cvt(unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0) })
        .wrap_err("socket")?;
    Ok(unsafe { OwnedFd::from_raw_fd(raw) })
}

/// Forward SIGINT/SIGTERM to both children so the whole VM stops with us.
pub fn install_signal_handlers() -> Result<()> {
    for sig in [libc::SIGINT, libc::SIGTERM] {
        let handler = forward_sigterm as *const () as libc::sighandler_t;
        if unsafe { libc::signal(sig, handler) } == libc::SIG_ERR {
            return Err(std::io::Error::last_os_error()).wrap_err("signal");
        }
    }
    Ok(())
}

/// Async-signal-safe: only calls kill(2) on the recorded child pids.
extern "C" fn forward_sigterm(_sig: libc::c_int) {
    let qemu = QEMU_PID.load(Ordering::SeqCst);
    if qemu > 0 {
        unsafe { libc::kill(qemu, libc::SIGTERM) };
    }

    let virtiofsd = VIRTIOFSD_PID.load(Ordering::SeqCst);
    if virtiofsd > 0 {
        unsafe { libc::kill(virtiofsd, libc::SIGTERM) };
    }
}

fn cvt(ret: libc::c_int) -> Result<libc::c_int> {
    if ret < 0 {
        Err(std::io::Error::last_os_error().into())
    } else {
        Ok(ret)
    }
}

/// Wraps virtiofsd's Child so it is SIGTERM'd and reaped on any exit path.
pub struct ReapGuard(pub Child);

impl Drop for ReapGuard {
    fn drop(&mut self) {
        let pid = self.0.id() as i32;
        unsafe { libc::kill(pid, libc::SIGTERM) };

        // Give it a moment to exit gracefully, then SIGKILL so a wedged
        // process can never hang the supervisor's shutdown.
        for _ in 0..50 {
            match self.0.try_wait() {
                Ok(Some(_)) => return,
                _ => std::thread::sleep(std::time::Duration::from_millis(100)),
            }
        }
        unsafe { libc::kill(pid, libc::SIGKILL) };
        let _ = self.0.wait();
    }
}
