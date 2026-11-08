use std::fs::{self};
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::process::CommandExt;
use std::process::{Child, Command};
use std::sync::atomic::{AtomicI32, Ordering};

use eyre::{Result, WrapErr, bail, eyre};
use indicatif::{ProgressBar, ProgressStyle};

mod qemu;
mod signing;
pub use qemu::{FIRST_CHILD_FD, QemuConfig, SharedDir, share_fd, shell_quote};
pub use signing::SigningHelper;

// microvm boots an external kernel/initrd, so we extract them from the image.
const BOOT_KERNEL: &str = "kernel";
const BOOT_INITRAMFS: &str = "initramfs.img";

/// Where staged fds are parked: above every destination, so dup2'ing sources
/// down into 3.. cannot clobber a source that has yet to be duped.
pub const STAGING_FD: RawFd = 128;

// qemu's pid, so the async-signal handler can forward termination to it.
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
        if entry.file_name().to_str().is_some_and(&pred) {
            return Ok(Some(entry.path().to_string_lossy().into_owned()));
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

/// Duplicate `fd` to some number at or above `lowest`, keeping it CLOEXEC.
///
/// Sources and destinations overlap in practice -- a supervisor holding only
/// std{in,out,err} gets 3, 4, 5 for its own sockets, which are exactly the
/// numbers the child wants -- so sources are moved out of the way here, in the
/// parent, where a failure is still reportable.
pub fn dup_above(fd: BorrowedFd<'_>, lowest: RawFd) -> Result<OwnedFd> {
    let raw = cvt(unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_DUPFD_CLOEXEC, lowest) })
        .wrap_err("fcntl(F_DUPFD_CLOEXEC)")?;
    Ok(unsafe { OwnedFd::from_raw_fd(raw) })
}

/// Spawn `cmd` with each `(src, dst)` duped into place and FD_CLOEXEC cleared
/// on `dst`, so the child inherits the sockets as known fd numbers.
///
/// `src` must not collide with any `dst`; stage them with [`dup_above`] first.
pub fn spawn_with_fds(cmd: &mut Command, pairs: &[(RawFd, RawFd)]) -> Result<Child> {
    let pairs = pairs.to_vec();
    unsafe {
        cmd.pre_exec(move || {
            for &(src, dst) in &pairs {
                // dup2(fd, fd) is a no-op that leaves FD_CLOEXEC set, so it is
                // always cleared explicitly rather than relying on dup2.
                if libc::dup2(src, dst) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                let flags = libc::fcntl(dst, libc::F_GETFD);
                if flags < 0 || libc::fcntl(dst, libc::F_SETFD, flags & !libc::FD_CLOEXEC) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
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
    // One share per listener, so a single pending connection is all we need.
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
/// leaking into the *other* children; spawn_with_fds re-clears it per child.
fn new_unix_socket() -> Result<OwnedFd> {
    let raw = cvt(unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0) })
        .wrap_err("socket")?;
    Ok(unsafe { OwnedFd::from_raw_fd(raw) })
}

/// Forward SIGINT/SIGTERM/SIGHUP to qemu so the whole VM stops with us.
pub fn install_signal_handlers() -> Result<()> {
    for sig in [
        signal_hook::consts::SIGINT,
        signal_hook::consts::SIGTERM,
        signal_hook::consts::SIGHUP,
    ] {
        // Safe because the action only calls kill(2) on a recorded pid.
        unsafe { signal_hook::low_level::register(sig, forward_sigterm) }
            .wrap_err_with(|| format!("registering a handler for signal {sig}"))?;
    }
    Ok(())
}

/// Async-signal-safe: only calls kill(2) on the recorded qemu pid.
fn forward_sigterm() {
    let qemu = QEMU_PID.load(Ordering::SeqCst);
    if qemu > 0 {
        unsafe { libc::kill(qemu, libc::SIGTERM) };
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

/// How long a child gets to honour SIGTERM before it is killed.
const REAP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

impl Drop for ReapGuard {
    fn drop(&mut self) {
        let pid = self.0.id() as i32;
        unsafe { libc::kill(pid, libc::SIGTERM) };

        // Give it REAP_TIMEOUT to exit gracefully, then SIGKILL so a wedged
        // process can never hang the supervisor's shutdown.
        let deadline = std::time::Instant::now() + REAP_TIMEOUT;
        while std::time::Instant::now() < deadline {
            match self.0.try_wait() {
                Ok(Some(_)) => return,
                Ok(None) => std::thread::sleep(std::time::Duration::from_millis(100)),
                // A failing try_wait will keep failing; don't burn the timeout
                // reading it as "still running".
                Err(e) => {
                    eprintln!("try_wait on pid {pid}: {e}");
                    break;
                }
            }
        }
        unsafe { libc::kill(pid, libc::SIGKILL) };
        let _ = self.0.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn abstract_sockaddr_is_nul_prefixed_and_exactly_sized() {
        let name = b"vfs-1234-0";
        let (addr, len) = abstract_sockaddr(name).unwrap();
        let path_off = std::mem::offset_of!(libc::sockaddr_un, sun_path);
        // The length must cover the leading NUL and the name, and no more: a
        // longer len would make the trailing zeroes part of the address.
        assert_eq!(len as usize, path_off + 1 + name.len());
        assert_eq!(addr.sun_path[0], 0);
        let got: Vec<u8> = addr.sun_path[1..=name.len()].iter().map(|&c| c as u8).collect();
        assert_eq!(got, name);
    }

    #[test]
    fn abstract_sockaddr_rejects_a_name_that_would_truncate() {
        let addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
        let too_long = vec![b'x'; addr.sun_path.len()];
        assert!(abstract_sockaddr(&too_long).is_err());
        assert!(abstract_sockaddr(&too_long[1..]).is_ok());
    }
}
